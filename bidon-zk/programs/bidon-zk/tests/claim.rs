#![cfg(feature = "test-sbf")]

mod common;
use common::*;
use solana_sdk::signature::Signer;

const CONTENT: [u8; 32] = [7u8; 32];

/// After end_time, the creator claims the winning pool minus fee; fee goes to the
/// fee_receiver; the vault is drained; creator_paid is set.
#[tokio::test]
async fn test_claim_winnings() {
    let mut rpc = new_rpc().await;
    let ctx = setup(&mut rpc, MIN_BID).await;

    // One bidder on one proposal -> it is the winner.
    let (bidder, bidder_token) = funded_bidder(&mut rpc, &ctx, 1_000_000).await;
    let amount = 1_000_000u64; // 1.0 USDC
    do_place_bid(&mut rpc, &ctx, &bidder, bidder_token, 0, CONTENT, amount).await;

    // creator + fee_receiver USDC accounts (fee_receiver == owner in setup).
    let creator_token = token_account(&mut rpc, &ctx.payer, ctx.mint, &ctx.creator.pubkey()).await;
    let fee_receiver_token =
        token_account(&mut rpc, &ctx.payer, ctx.mint, &ctx.owner.pubkey()).await;

    warp_past(&mut rpc, 3601);
    do_claim(&mut rpc, &ctx, creator_token, fee_receiver_token).await;

    // fee = 1.0 * 370 / 10000 = 0.037; payout = 0.963. Creator also gets the 0.5 deposit back.
    let fee = amount * 370 / 10_000;
    assert_eq!(
        token_amount(&mut rpc, creator_token).await,
        amount - fee + bidon_zk::CREATOR_DEPOSIT
    );
    assert_eq!(token_amount(&mut rpc, fee_receiver_token).await, fee);
    // Vault drained: winners_pot (payout+fee) + deposit all left, nothing stranded.
    assert_eq!(token_amount(&mut rpc, ctx.vault_pda).await, 0);
    assert!(get_auction(&mut rpc, ctx.auction_pda).await.creator_paid);
}
