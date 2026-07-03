#![cfg(feature = "test-sbf")]

mod common;
use common::*;
use solana_sdk::signature::Signer;

const CONTENT: [u8; 32] = [3u8; 32];

/// Full place_bid on a new proposal: pulls USDC into the vault, creates ProposalTotal +
/// Bid (compressed, combined proof), bumps the auction leader. Gasless.
#[tokio::test]
async fn test_place_bid() {
    let mut rpc = new_rpc().await;
    let ctx = setup(&mut rpc, MIN_BID).await;
    let (bidder, bidder_token) = funded_bidder(&mut rpc, &ctx, 1_000_000).await;

    let amount = 500_000u64; // 0.5 USDC
    let pid = 0u64;
    let (p_addr, b_addr) =
        do_place_bid(&mut rpc, &ctx, &bidder, bidder_token, pid, CONTENT, amount).await;

    let proposal = proposal_total(&mut rpc, p_addr).await;
    assert_eq!(proposal.total, amount);
    assert_eq!(proposal.creator, bidder.pubkey());
    assert_eq!(proposal.content_hash, CONTENT);

    let bid = bid_state(&mut rpc, b_addr).await;
    assert_eq!(bid.amount, amount);
    assert_eq!(bid.proposal, pid);
    assert_eq!(bid.bidder, bidder.pubkey());

    // USDC moved into the vault (on top of the creator's deposit); bidder debited.
    assert_eq!(
        token_amount(&mut rpc, ctx.vault_pda).await,
        amount + bidon_zk::CREATOR_DEPOSIT
    );
    assert_eq!(token_amount(&mut rpc, bidder_token).await, 1_000_000 - amount);

    // Auction leader + counters.
    let auction = get_auction(&mut rpc, ctx.auction_pda).await;
    assert_eq!(auction.winner_proposal, pid);
    assert_eq!(auction.winner_amount, amount);
    assert_eq!(auction.total_staked, amount);
    assert_eq!(auction.proposal_count, 1);
}
