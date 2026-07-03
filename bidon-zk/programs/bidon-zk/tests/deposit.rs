#![cfg(feature = "test-sbf")]
//! Refundable anti-spam creator deposit (approach "B", schema 3). Creating an auction pulls a
//! CREATOR_DEPOSIT (0.5 USDC) from the creator into the vault; an honest creator gets it back at
//! claim (winner settled) or at cancel (empty auction). It is NOT a fee — abandoning an auction
//! locks that capital (the intended spam deterrent). These tests assert the deposit is pulled,
//! refunded, and (when abandoned) locked, and that fund-conservation still holds end to end.

mod common;
use common::*;

use anchor_lang::{InstructionData, ToAccountMetas};
use anchor_spl::token::spl_token;
use light_program_test::{program_test::LightProgramTest, Rpc};
use solana_sdk::{instruction::Instruction, signature::Signer};

const C0: [u8; 32] = [1u8; 32];
const C1: [u8; 32] = [2u8; 32];

/// force_close_auction (permissionless GC after end_time + CLOSE_GRACE_SECS). Returns Ok/Err so a
/// test can assert it reverts. fee_receiver_token is the configured fee_receiver's (owner's) ATA.
async fn try_force_close(
    rpc: &mut LightProgramTest,
    ctx: &Ctx,
    fee_receiver_token: solana_sdk::pubkey::Pubkey,
) -> std::result::Result<(), ()> {
    let ix = Instruction {
        program_id: bidon_zk::ID,
        accounts: bidon_zk::accounts::ForceCloseAuction {
            config: ctx.config_pda,
            auction: ctx.auction_pda,
            vault: ctx.vault_pda,
            usdc_mint: ctx.mint,
            fee_receiver_token,
            rent_recipient: ctx.payer.pubkey(),
            token_program: spl_token::ID,
        }
        .to_account_metas(None),
        data: bidon_zk::instruction::ForceCloseAuction {}.data(),
    };
    rpc.create_and_send_transaction(&[ix], &ctx.payer.pubkey(), &[&ctx.payer])
        .await
        .map(|_| ())
        .map_err(|_| ())
}

/// 1. create_auction pulls exactly CREATOR_DEPOSIT into the vault and marks schema 3.
#[tokio::test]
async fn test_create_pulls_deposit() {
    let mut rpc = new_rpc().await;
    let ctx = setup(&mut rpc, MIN_BID).await;

    let dep = bidon_zk::CREATOR_DEPOSIT;
    // Vault holds only the deposit right after create.
    assert_eq!(token_amount(&mut rpc, ctx.vault_pda).await, dep);
    // Creator's ATA was funded with 10.0 and debited the deposit.
    assert_eq!(token_amount(&mut rpc, ctx.creator_token).await, 10_000_000 - dep);
    // Auction is on the deposit schema.
    assert_eq!(get_auction(&mut rpc, ctx.auction_pda).await.schema_version, 3);
}

/// 2. A creator whose ATA holds less than CREATOR_DEPOSIT cannot create — the deposit pull reverts.
#[tokio::test]
async fn test_create_fails_without_deposit_funds() {
    let mut rpc = new_rpc().await;
    // Bring up Config + mint without creating the base auction (reuse setup, then create a SECOND
    // auction id=1 whose creator is underfunded).
    let base = setup(&mut rpc, MIN_BID).await;

    let creator = solana_sdk::signature::Keypair::new();
    let id = 1u64;
    let (auction_pda, _) = solana_sdk::pubkey::Pubkey::find_program_address(
        &[b"auction", &id.to_le_bytes()],
        &bidon_zk::ID,
    );
    let (vault_pda, _) = solana_sdk::pubkey::Pubkey::find_program_address(
        &[b"vault", auction_pda.as_ref()],
        &bidon_zk::ID,
    );
    // Fund the creator with LESS than the deposit (0.1 USDC < 0.5 USDC).
    let creator_token =
        funded_token_account(&mut rpc, &base.payer, base.mint, &creator.pubkey(), 100_000).await;

    let ix = create_auction_ix(
        &base.payer,
        &creator,
        base.config_pda,
        auction_pda,
        vault_pda,
        base.mint,
        creator_token,
        id,
        MIN_BID,
        1,
    );
    let res = rpc
        .create_and_send_transaction(&[ix], &base.payer.pubkey(), &[&base.payer, &creator])
        .await;
    assert!(res.is_err(), "create must revert when the creator cannot fund the deposit");
    // Nothing left behind: the auction account was never created (atomic revert).
    assert!(
        rpc.get_account(auction_pda).await.unwrap().is_none(),
        "auction not created on a reverted deposit pull"
    );
}

/// 3. Full flow: create → winner + loser bids → claim → withdraw → close, with the deposit refunded
///    to the creator at claim. Asserts master fund-conservation: USDC out == staked + deposit.
#[tokio::test]
async fn test_full_flow_refunds_deposit() {
    let mut rpc = new_rpc().await;
    let ctx = setup(&mut rpc, MIN_BID).await;

    let dep = bidon_zk::CREATOR_DEPOSIT;

    // Winner: proposal 1 @ 0.8; loser: proposal 0 @ 0.3.
    let (loser, loser_t) = funded_bidder(&mut rpc, &ctx, 1_000_000).await;
    do_place_bid(&mut rpc, &ctx, &loser, loser_t, 0, C0, 300_000).await;
    let (winner, winner_t) = funded_bidder(&mut rpc, &ctx, 1_000_000).await;
    do_place_bid(&mut rpc, &ctx, &winner, winner_t, 1, C1, 800_000).await;

    // Vault = 1.1 staked + 0.5 deposit.
    assert_eq!(token_amount(&mut rpc, ctx.vault_pda).await, 1_100_000 + dep);

    let creator_token = token_account(&mut rpc, &ctx.payer, ctx.mint, &ctx.creator.pubkey()).await;
    let fee_receiver_token =
        token_account(&mut rpc, &ctx.payer, ctx.mint, &ctx.owner.pubkey()).await;

    warp_past(&mut rpc, 3601);
    do_claim(&mut rpc, &ctx, creator_token, fee_receiver_token).await;

    // winners_pot = 0.8; fee = 0.8 * 370/10000; payout = pot - fee. Creator ALSO gets the deposit.
    let pot = 800_000u64;
    let fee = pot * 370 / 10_000;
    let payout = pot - fee;
    assert_eq!(token_amount(&mut rpc, creator_token).await, payout + dep);
    assert_eq!(token_amount(&mut rpc, fee_receiver_token).await, fee);
    // Vault now holds only the loser's un-withdrawn 0.3 (deposit + winners_pot both left).
    assert_eq!(token_amount(&mut rpc, ctx.vault_pda).await, 300_000);

    // Loser withdraws → vault drained.
    do_withdraw(&mut rpc, &ctx, loser.pubkey(), loser_t, 0, 300_000).await;
    assert_eq!(token_amount(&mut rpc, loser_t).await, 1_000_000); // fully refunded
    assert_eq!(token_amount(&mut rpc, ctx.vault_pda).await, 0);

    // close_auction now succeeds (vault empty, creator paid).
    do_close_auction(&mut rpc, &ctx).await;
    assert!(rpc.get_account(ctx.auction_pda).await.unwrap().is_none());
    assert!(rpc.get_account(ctx.vault_pda).await.unwrap().is_none());

    // Master conservation: sum of USDC OUT == total staked (1.1) + deposit (0.5).
    let staked = 1_100_000u64;
    let out = (payout + dep) + fee + 300_000; // creator (payout+deposit) + fee_receiver + loser refund
    assert_eq!(out, staked + dep);
}

/// 4. Empty-auction cancel refunds the whole vault (deposit + any dust) to the creator, then closes.
#[tokio::test]
async fn test_empty_cancel_refunds_deposit() {
    let mut rpc = new_rpc().await;
    let ctx = setup(&mut rpc, MIN_BID).await;

    let dep = bidon_zk::CREATOR_DEPOSIT;
    // Vault holds only the deposit; creator is down 0.5.
    assert_eq!(token_amount(&mut rpc, ctx.vault_pda).await, dep);
    assert_eq!(token_amount(&mut rpc, ctx.creator_token).await, 10_000_000 - dep);

    let ix = Instruction {
        program_id: bidon_zk::ID,
        accounts: bidon_zk::accounts::CancelAuction {
            auction: ctx.auction_pda,
            vault: ctx.vault_pda,
            usdc_mint: ctx.mint,
            creator_token: ctx.creator_token,
            creator: ctx.creator.pubkey(),
            rent_recipient: ctx.payer.pubkey(),
            token_program: spl_token::ID,
        }
        .to_account_metas(None),
        data: bidon_zk::instruction::CancelAuction {}.data(),
    };
    rpc.create_and_send_transaction(&[ix], &ctx.payer.pubkey(), &[&ctx.payer, &ctx.creator])
        .await
        .unwrap();

    // Deposit refunded in full (creator back to 10.0); vault + auction gone.
    assert_eq!(token_amount(&mut rpc, ctx.creator_token).await, 10_000_000);
    assert!(rpc.get_account(ctx.vault_pda).await.unwrap().is_none(), "vault closed");
    assert!(rpc.get_account(ctx.auction_pda).await.unwrap().is_none(), "auction closed");
}

/// 5. An abandoned auction's deposit is LOCKED: force_close reverts while the vault is non-empty
///    (the deposit is still there), so the deposit cannot be swept — the deterrent holds.
#[tokio::test]
async fn test_abandoned_deposit_locked() {
    let mut rpc = new_rpc().await;
    let ctx = setup(&mut rpc, MIN_BID).await;

    let dep = bidon_zk::CREATOR_DEPOSIT;
    assert_eq!(token_amount(&mut rpc, ctx.vault_pda).await, dep);

    let fee_receiver_token =
        token_account(&mut rpc, &ctx.payer, ctx.mint, &ctx.owner.pubkey()).await;

    // Warp past end_time + the full close grace period.
    warp_past(&mut rpc, 3601 + bidon_zk::CLOSE_GRACE_SECS);

    // force_close must revert: the vault still holds the deposit (require!(vault.amount == 0) fails).
    let res = try_force_close(&mut rpc, &ctx, fee_receiver_token).await;
    assert!(res.is_err(), "force_close MUST revert while the deposit is still in the vault");

    // The deposit is still locked in the vault (deterrent); nothing was swept to the fee_receiver.
    assert_eq!(token_amount(&mut rpc, ctx.vault_pda).await, dep, "deposit stays locked");
    assert_eq!(token_amount(&mut rpc, fee_receiver_token).await, 0, "nothing swept");
    assert!(
        rpc.get_account(ctx.auction_pda).await.unwrap().is_some(),
        "auction still alive (force_close reverted)"
    );
}
