#![cfg(feature = "test-sbf")]

mod common;
use common::*;

const CONTENT: [u8; 32] = [5u8; 32];

/// Raising an existing proposal: a new backer (raise_bid: update ProposalTotal + create Bid)
/// and an own top-up (top_up_bid: update both). Verifies aggregate, positions, leader, vault.
#[tokio::test]
async fn test_raise_and_top_up() {
    let mut rpc = new_rpc().await;
    let ctx = setup(&mut rpc, MIN_BID).await;

    // A opens proposal 0 with 0.5 USDC.
    let (a, a_token) = funded_bidder(&mut rpc, &ctx, 1_000_000).await;
    let (p_addr, a_bid) = do_place_bid(&mut rpc, &ctx, &a, a_token, 0, CONTENT, 500_000).await;
    assert_eq!(proposal_total(&mut rpc, p_addr).await.total, 500_000);

    // B raises proposal 0 as a NEW backer with 0.3 USDC -> proposal total 0.8.
    let (b, b_token) = funded_bidder(&mut rpc, &ctx, 1_000_000).await;
    let b_bid = do_raise_bid(&mut rpc, &ctx, &b, b_token, 0, 300_000).await;
    assert_eq!(proposal_total(&mut rpc, p_addr).await.total, 800_000);
    assert_eq!(bid_state(&mut rpc, b_bid).await.amount, 300_000);

    // A tops up its own Bid by 0.2 -> A's bid 0.7, proposal total 1.0.
    do_top_up_bid(&mut rpc, &ctx, &a, a_token, 0, 200_000).await;
    assert_eq!(bid_state(&mut rpc, a_bid).await.amount, 700_000);
    assert_eq!(proposal_total(&mut rpc, p_addr).await.total, 1_000_000);

    // Leader = proposal 0 @ 1.0; vault holds 1.0; total_staked 1.0.
    let auction = get_auction(&mut rpc, ctx.auction_pda).await;
    assert_eq!(auction.winner_proposal, 0);
    assert_eq!(auction.winner_amount, 1_000_000);
    assert_eq!(auction.total_staked, 1_000_000);
    assert_eq!(token_amount(&mut rpc, ctx.vault_pda).await, 1_000_000);
}
