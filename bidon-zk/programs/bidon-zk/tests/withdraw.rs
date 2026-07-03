#![cfg(feature = "test-sbf")]

mod common;
use common::*;
use solana_sdk::signature::Signer;

const C0: [u8; 32] = [1u8; 32];
const C1: [u8; 32] = [2u8; 32];

/// After end_time, a losing bidder reclaims their stake and the compressed Bid is closed
/// (double-refund guard). The winning pool stays in the vault for the creator.
#[tokio::test]
async fn test_withdraw_loser() {
    let mut rpc = new_rpc().await;
    let ctx = setup(&mut rpc, MIN_BID).await;

    // A bids 0.3 on proposal 0; B bids 0.8 on proposal 1 -> proposal 1 wins.
    let (a, a_token) = funded_bidder(&mut rpc, &ctx, 1_000_000).await;
    let (_a_p, a_bid) = do_place_bid(&mut rpc, &ctx, &a, a_token, 0, C0, 300_000).await;
    let (b, b_token) = funded_bidder(&mut rpc, &ctx, 1_000_000).await;
    do_place_bid(&mut rpc, &ctx, &b, b_token, 1, C1, 800_000).await;

    assert_eq!(
        get_auction(&mut rpc, ctx.auction_pda).await.winner_proposal,
        1
    );

    warp_past(&mut rpc, 3601);

    // A (proposal 0 != winner 1) withdraws: refund 0.3, Bid closed.
    do_withdraw(&mut rpc, &ctx, a.pubkey(), a_token, 0, 300_000).await;
    assert_eq!(token_amount(&mut rpc, a_token).await, 1_000_000);
    let acc = compressed(&mut rpc, a_bid).await;
    assert_eq!(acc.data, Some(Default::default()));

    // Vault still holds B's winning 0.8 plus the creator's unclaimed 0.5 deposit (claim not run).
    assert_eq!(
        token_amount(&mut rpc, ctx.vault_pda).await,
        800_000 + bidon_zk::CREATOR_DEPOSIT
    );
}
