#![cfg(feature = "test-sbf")]

mod common;
use common::*;

use anchor_lang::{InstructionData, ToAccountMetas};
use anchor_spl::token::spl_token;
use bidon_zk::BidonError;
use light_program_test::Rpc;
use solana_sdk::{
    instruction::Instruction,
    pubkey::Pubkey,
    signature::{Keypair, Signer},
};

const CONTENT: [u8; 32] = [9u8; 32];

/// Anchor custom error code = 6000 + variant index (#[error_code] offset).
fn anchor_code(e: BidonError) -> u32 {
    6000 + e as u32
}

/// Build a cancel_auction instruction (creator-only). fee-payer/relayer signs at send time.
/// `creator_token` receives the refunded deposit (+ any dust) before the vault closes (schema 3).
fn cancel_ix(ctx: &Ctx, creator: Pubkey, creator_token: Pubkey, rent_recipient: Pubkey) -> Instruction {
    Instruction {
        program_id: bidon_zk::ID,
        accounts: bidon_zk::accounts::CancelAuction {
            auction: ctx.auction_pda,
            vault: ctx.vault_pda,
            usdc_mint: ctx.mint,
            creator_token,
            creator,
            rent_recipient,
            token_program: spl_token::ID,
        }
        .to_account_metas(None),
        data: bidon_zk::instruction::CancelAuction {}.data(),
    }
}

/// Assert an on-chain transaction failed with the expected Anchor custom error code.
fn assert_custom<T: std::fmt::Debug, E: std::fmt::Debug>(res: Result<T, E>, code: u32) {
    let err = res.expect_err("expected on-chain failure");
    let s = format!("{:?}", err);
    assert!(
        s.contains(&format!("Custom({})", code)),
        "expected Custom({}); got: {}",
        code,
        s
    );
}

/// happy: creator cancels an EMPTY auction (relayer fronted rent, creator 0 SOL) → vault + Auction
/// closed, relayer (rent_payer) recovers all rent. No time gate.
#[tokio::test]
async fn test_cancel_auction_happy() {
    let mut rpc = new_rpc().await;
    let ctx = setup(&mut rpc, MIN_BID).await;

    // Vault holds only the refundable deposit before cancel (schema 3).
    assert_eq!(
        token_amount(&mut rpc, ctx.vault_pda).await,
        bidon_zk::CREATOR_DEPOSIT,
        "vault holds the deposit pre-cancel"
    );

    let rent_before = rpc.get_balance(&ctx.payer.pubkey()).await.unwrap();
    let ix = cancel_ix(&ctx, ctx.creator.pubkey(), ctx.creator_token, ctx.payer.pubkey());
    rpc.create_and_send_transaction(&[ix], &ctx.payer.pubkey(), &[&ctx.payer, &ctx.creator])
        .await
        .unwrap();

    assert!(
        rpc.get_account(ctx.vault_pda).await.unwrap().is_none(),
        "vault closed"
    );
    assert!(
        rpc.get_account(ctx.auction_pda).await.unwrap().is_none(),
        "Auction closed"
    );
    // Deposit was refunded to the creator (10.0 funded → still 10.0 after 0.5 out at create + 0.5 back).
    assert_eq!(
        token_amount(&mut rpc, ctx.creator_token).await,
        10_000_000,
        "creator got the deposit back"
    );
    assert!(
        rpc.get_balance(&ctx.payer.pubkey()).await.unwrap() > rent_before,
        "relayer recovered the rent"
    );
}

/// negative: a placed bid makes proposal_count > 0 → cancel reverts (AuctionNotEmpty), Auction lives.
#[tokio::test]
async fn test_cancel_rejects_non_empty() {
    let mut rpc = new_rpc().await;
    let ctx = setup(&mut rpc, MIN_BID).await;

    let (bidder, bidder_token) = funded_bidder(&mut rpc, &ctx, 1_000_000).await;
    do_place_bid(&mut rpc, &ctx, &bidder, bidder_token, 0, CONTENT, 1_000_000).await;

    let ix = cancel_ix(&ctx, ctx.creator.pubkey(), ctx.creator_token, ctx.payer.pubkey());
    let res = rpc
        .create_and_send_transaction(&[ix], &ctx.payer.pubkey(), &[&ctx.payer, &ctx.creator])
        .await;
    assert_custom(res, anchor_code(BidonError::AuctionNotEmpty));

    assert!(
        rpc.get_account(ctx.auction_pda).await.unwrap().is_some(),
        "Auction still alive after rejected cancel"
    );
}

/// negative: a non-creator signer cannot cancel — has_one fails (Unauthorized), Auction lives.
#[tokio::test]
async fn test_cancel_rejects_non_creator() {
    let mut rpc = new_rpc().await;
    let ctx = setup(&mut rpc, MIN_BID).await;

    let intruder = Keypair::new();
    let ix = cancel_ix(&ctx, intruder.pubkey(), ctx.creator_token, ctx.payer.pubkey());
    let res = rpc
        .create_and_send_transaction(&[ix], &ctx.payer.pubkey(), &[&ctx.payer, &intruder])
        .await;
    assert_custom(res, anchor_code(BidonError::Unauthorized));

    assert!(
        rpc.get_account(ctx.auction_pda).await.unwrap().is_some(),
        "Auction still alive after rejected cancel"
    );
}
