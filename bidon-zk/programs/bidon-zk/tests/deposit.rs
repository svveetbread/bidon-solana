#![cfg(feature = "test-sbf")]
//! Schema-3 GLOBAL creator-deposit tests. The deposit is a returnable anti-spam stake pulled from the
//! creator into a single GLOBAL vault at create_auction and reclaimed exactly ONCE per auction — at
//! claim (settled) OR cancel (empty), never both. The vault is commingled across auctions, so the
//! highest-risk properties are (a) the no-double-refund guard (claim-then-cancel refunds only once) and
//! (b) cross-auction isolation (cancelling one auction never touches another's deposit).

mod common;
use common::*;

use light_program_test::Rpc;
use solana_sdk::signature::Signer;

const C0: [u8; 32] = [1u8; 32];
const C1: [u8; 32] = [2u8; 32];

const DEPOSIT: u64 = bidon_zk::CREATOR_DEPOSIT;

/// 1. create_auction pulls exactly CREATOR_DEPOSIT from the creator into the GLOBAL deposit vault; the
/// per-auction vault stays empty; the auction is on schema 3.
#[tokio::test]
async fn test_create_pulls_deposit_to_global_vault() {
    let mut rpc = new_rpc().await;
    let ctx = setup(&mut rpc, MIN_BID).await; // setup creates ONE auction

    assert_eq!(token_amount(&mut rpc, ctx.deposit_vault_pda).await, DEPOSIT);
    // Creator funded with 10 USDC in setup, debited by the deposit.
    assert_eq!(token_amount(&mut rpc, ctx.creator_token).await, 10_000_000 - DEPOSIT);
    // The deposit is GLOBAL, not per-auction: the auction vault holds nothing yet.
    assert_eq!(token_amount(&mut rpc, ctx.vault_pda).await, 0);
    assert_eq!(get_auction(&mut rpc, ctx.auction_pda).await.schema_version, 3);
}

/// 2. A creator funded with LESS than CREATOR_DEPOSIT cannot create an auction (the deposit transfer
/// reverts the whole tx). auction_count stays 0.
#[tokio::test]
async fn test_create_fails_without_deposit_funds() {
    let mut rpc = new_rpc().await;
    let payer = rpc.get_payer().insecure_clone();
    let owner = solana_sdk::signature::Keypair::new();
    rpc.airdrop_lamports(&owner.pubkey(), 5_000_000_000).await.unwrap();

    let mint = create_mint(&mut rpc, &payer).await;
    let (config_pda, _) =
        solana_sdk::pubkey::Pubkey::find_program_address(&[b"config"], &bidon_zk::ID);
    initialize_config(&mut rpc, &owner, config_pda, mint).await;
    init_deposit_vault(&mut rpc, &payer, config_pda, mint).await;

    let creator = solana_sdk::signature::Keypair::new();
    // Fund the creator with LESS than CREATOR_DEPOSIT.
    let creator_token =
        funded_token_account(&mut rpc, &payer, mint, &creator.pubkey(), DEPOSIT - 1).await;

    let id = 0u64;
    let (auction_pda, _) = solana_sdk::pubkey::Pubkey::find_program_address(
        &[b"auction", &id.to_le_bytes()],
        &bidon_zk::ID,
    );
    let (vault_pda, _) = solana_sdk::pubkey::Pubkey::find_program_address(
        &[b"vault", auction_pda.as_ref()],
        &bidon_zk::ID,
    );
    let res = rpc
        .create_and_send_transaction(
            &[create_auction_ix(
                &payer, &creator, creator_token, config_pda, auction_pda, vault_pda, mint, id,
                MIN_BID, 1,
            )],
            &payer.pubkey(),
            &[&payer, &creator],
        )
        .await;
    assert!(res.is_err(), "create must fail when creator can't cover the deposit");
    // The whole tx reverted: no auction was created and the deposit vault is still empty.
    assert!(rpc.get_account(auction_pda).await.unwrap().is_none());
    assert_eq!(token_amount(&mut rpc, deposit_vault_pda()).await, 0);
}

/// 3. Full flow: winner + loser bids, warp, claim (creator gets payout − fee + DEPOSIT back, deposit
/// vault drained), loser withdraws, per-auction vault drains to 0, close succeeds. Conservation holds.
#[tokio::test]
async fn test_full_flow_refunds_deposit() {
    let mut rpc = new_rpc().await;
    let ctx = setup(&mut rpc, MIN_BID).await;

    // Winner proposal 0 @ 0.8, loser proposal 1 @ 0.3.
    let (winner, w_token) = funded_bidder(&mut rpc, &ctx, 1_000_000).await;
    do_place_bid(&mut rpc, &ctx, &winner, w_token, 0, C0, 800_000).await;
    let (loser, l_token) = funded_bidder(&mut rpc, &ctx, 1_000_000).await;
    do_place_bid(&mut rpc, &ctx, &loser, l_token, 1, C1, 300_000).await;

    assert_eq!(token_amount(&mut rpc, ctx.vault_pda).await, 1_100_000);
    assert_eq!(token_amount(&mut rpc, ctx.deposit_vault_pda).await, DEPOSIT);

    let creator_token = ctx.creator_token; // reuse the funded creator ATA (already debited by DEPOSIT)
    let creator_before = token_amount(&mut rpc, creator_token).await;
    let fee_token = token_account(&mut rpc, &ctx.payer, ctx.mint, &ctx.owner.pubkey()).await;

    warp_past(&mut rpc, 3601);
    do_claim(&mut rpc, &ctx, creator_token, fee_token).await;

    let pot = 800_000u64; // only proposal 0 wins
    let fee = pot * 370 / 10_000;
    let payout = pot - fee;
    // Creator receives payout + refunded DEPOSIT.
    assert_eq!(
        token_amount(&mut rpc, creator_token).await,
        creator_before + payout + DEPOSIT
    );
    assert_eq!(token_amount(&mut rpc, fee_token).await, fee);
    // Global deposit vault drained by exactly one DEPOSIT.
    assert_eq!(token_amount(&mut rpc, ctx.deposit_vault_pda).await, 0);

    // Loser withdraws; per-auction vault drains to 0; close succeeds.
    do_withdraw(&mut rpc, &ctx, loser.pubkey(), l_token, 1, 300_000).await;
    assert_eq!(token_amount(&mut rpc, l_token).await, 1_000_000);
    assert_eq!(token_amount(&mut rpc, ctx.vault_pda).await, 0);
    do_close_auction(&mut rpc, &ctx).await;
    assert!(rpc.get_account(ctx.auction_pda).await.unwrap().is_none());

    // Conservation of the per-auction pool: payout + fee + refund == total_staked.
    assert_eq!(payout + fee + 300_000, 1_100_000);
}

/// 4. Empty auction: cancel refunds the DEPOSIT (deposit vault drained), and both the auction and its
/// vault are closed.
#[tokio::test]
async fn test_empty_cancel_refunds_deposit() {
    let mut rpc = new_rpc().await;
    let ctx = setup(&mut rpc, MIN_BID).await;

    assert_eq!(token_amount(&mut rpc, ctx.deposit_vault_pda).await, DEPOSIT);
    let creator_before = token_amount(&mut rpc, ctx.creator_token).await;

    do_cancel_auction(&mut rpc, &ctx).await;

    // Creator got the deposit back; the global vault is empty; accounts closed.
    assert_eq!(
        token_amount(&mut rpc, ctx.creator_token).await,
        creator_before + DEPOSIT
    );
    assert_eq!(token_amount(&mut rpc, ctx.deposit_vault_pda).await, 0);
    assert!(rpc.get_account(ctx.auction_pda).await.unwrap().is_none());
    assert!(rpc.get_account(ctx.vault_pda).await.unwrap().is_none());
}

/// 5. CRITICAL double-refund guard: an empty schema-3 auction is settled via claim (winners_pot == 0,
/// refunds the deposit, sets creator_paid), then cancelled. Cancel must NOT refund a second time — the
/// global vault must drop by EXACTLY one DEPOSIT total. A second refund would drain another auction's
/// deposit from the commingled vault.
#[tokio::test]
async fn test_no_double_refund_claim_then_cancel() {
    let mut rpc = new_rpc().await;
    let ctx = setup(&mut rpc, MIN_BID).await;

    let deposit_before = token_amount(&mut rpc, ctx.deposit_vault_pda).await;
    assert_eq!(deposit_before, DEPOSIT);
    let creator_before = token_amount(&mut rpc, ctx.creator_token).await;
    let fee_token = token_account(&mut rpc, &ctx.payer, ctx.mint, &ctx.owner.pubkey()).await;

    // Claim on the EMPTY auction: no winners → no payout, but the deposit is refunded and creator_paid set.
    warp_past(&mut rpc, 3601);
    do_claim(&mut rpc, &ctx, ctx.creator_token, fee_token).await;
    assert!(get_auction(&mut rpc, ctx.auction_pda).await.creator_paid);
    assert_eq!(token_amount(&mut rpc, ctx.deposit_vault_pda).await, 0);
    assert_eq!(token_amount(&mut rpc, ctx.creator_token).await, creator_before + DEPOSIT);

    // Cancel the now-settled empty auction: it must close the accounts but MUST NOT refund again.
    do_cancel_auction(&mut rpc, &ctx).await;
    // Deposit vault still 0 (no second refund); creator balance unchanged since claim.
    assert_eq!(token_amount(&mut rpc, ctx.deposit_vault_pda).await, 0);
    assert_eq!(token_amount(&mut rpc, ctx.creator_token).await, creator_before + DEPOSIT);
    // Net movement out of the global vault: exactly ONE DEPOSIT.
    assert_eq!(deposit_before - token_amount(&mut rpc, ctx.deposit_vault_pda).await, DEPOSIT);
    assert!(rpc.get_account(ctx.auction_pda).await.unwrap().is_none());
}

/// 6. Cross-auction isolation: two auctions each deposit into the ONE global vault (2×DEPOSIT).
/// Cancelling A refunds exactly A's DEPOSIT and leaves B's untouched; cancelling B then drains the last.
#[tokio::test]
async fn test_two_auctions_isolated_deposits() {
    let mut rpc = new_rpc().await;
    let a = setup(&mut rpc, MIN_BID).await; // auction 0
    let b = create_extra_auction(&mut rpc, &a, 1, MIN_BID, 1).await; // auction 1, same global vault

    // Both deposits are in the ONE global vault.
    assert_eq!(token_amount(&mut rpc, a.deposit_vault_pda).await, 2 * DEPOSIT);

    let a_creator_before = token_amount(&mut rpc, a.creator_token).await;
    let b_creator_before = token_amount(&mut rpc, b.creator_token).await;

    // Cancel A: refunds exactly A's DEPOSIT, leaves B's in the vault.
    do_cancel_auction(&mut rpc, &a).await;
    assert_eq!(token_amount(&mut rpc, a.creator_token).await, a_creator_before + DEPOSIT);
    assert_eq!(token_amount(&mut rpc, a.deposit_vault_pda).await, DEPOSIT); // B's untouched
    assert_eq!(token_amount(&mut rpc, b.creator_token).await, b_creator_before); // B not paid

    // Cancel B: drains the last DEPOSIT.
    do_cancel_auction(&mut rpc, &b).await;
    assert_eq!(token_amount(&mut rpc, b.creator_token).await, b_creator_before + DEPOSIT);
    assert_eq!(token_amount(&mut rpc, b.deposit_vault_pda).await, 0);
}

/// 7. Abandoned deposit stays LOCKED: there is no auto-sweep. Warping past end + grace does NOT move the
/// deposit; it remains in the global vault (the deterrent). force_close cannot touch it either (it only
/// closes an empty PER-AUCTION vault, and requires creator_paid, which an abandoned auction never sets).
#[tokio::test]
async fn test_abandoned_deposit_locked() {
    let mut rpc = new_rpc().await;
    let ctx = setup(&mut rpc, MIN_BID).await;

    assert_eq!(token_amount(&mut rpc, ctx.deposit_vault_pda).await, DEPOSIT);

    // Warp far past end + the close grace. No crank sweeps the deposit.
    warp_past(&mut rpc, 3601 + 7 * 24 * 60 * 60 + 10);

    // The deposit is still locked in the global vault — no auto-sweep path exists.
    assert_eq!(token_amount(&mut rpc, ctx.deposit_vault_pda).await, DEPOSIT);
    // The auction is still there (never settled, so it can't be force-closed / closed).
    assert!(rpc.get_account(ctx.auction_pda).await.unwrap().is_some());
}
