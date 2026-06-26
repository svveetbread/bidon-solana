#![cfg(feature = "test-sbf")]
//! Regression tests for the internal audit fixes (see AUDIT.md).
//!  - C-1 (Critical): withdraw must reject a Bid that belongs to a DIFFERENT auction
//!    (cross-auction vault drain).
//!  - H-1 (High): zero-amount bids must be rejected even when min_bid == 0.

mod common;
use common::*;
use solana_sdk::signature::Signer;

const C0: [u8; 32] = [1u8; 32];
const C1: [u8; 32] = [2u8; 32];

/// C-1: a Bid staked into auction B cannot be redeemed against auction A's vault.
/// Both auctions are funded with a losing proposal 0 + a winning proposal 1, so the attack
/// passes the is_winner gate (pid 0 is a loser in A) and would, WITHOUT the fix, drain A's
/// funded vault. With the fix it reverts (ProposalIdMismatch) before any transfer.
#[tokio::test]
async fn test_cross_auction_withdraw_rejected() {
    let mut rpc = new_rpc().await;
    let a = setup(&mut rpc, MIN_BID).await; // auction id 0
    let b = create_extra_auction(&mut rpc, &a, 1, MIN_BID, 1).await; // auction id 1

    // Fund auction A: proposal 0 loses (0.3), proposal 1 wins (0.8). A.vault = 1.1 USDC.
    let (a0, a0_token) = funded_bidder(&mut rpc, &a, 1_000_000).await;
    do_place_bid(&mut rpc, &a, &a0, a0_token, 0, C0, 300_000).await;
    let (a1, a1_token) = funded_bidder(&mut rpc, &a, 1_000_000).await;
    do_place_bid(&mut rpc, &a, &a1, a1_token, 1, C1, 800_000).await;

    // Fund auction B identically; B's proposal-0 backer is the attacker's reusable losing Bid.
    let (b0, b0_token) = funded_bidder(&mut rpc, &b, 1_000_000).await;
    do_place_bid(&mut rpc, &b, &b0, b0_token, 0, C0, 300_000).await;
    let (b1, b1_token) = funded_bidder(&mut rpc, &b, 1_000_000).await;
    do_place_bid(&mut rpc, &b, &b1, b1_token, 1, C1, 800_000).await;

    warp_past(&mut rpc, 3601);

    let a_vault_before = token_amount(&mut rpc, a.vault_pda).await;
    assert_eq!(a_vault_before, 1_100_000);

    // ATTACK: present B's proposal-0 Bid against auction A's accounts/vault. Must be rejected.
    let attack = try_withdraw_xauction(&mut rpc, &a, &b, b0.pubkey(), b0_token, 0, 300_000).await;
    assert!(attack.is_err(), "cross-auction withdraw MUST be rejected (C-1)");

    // A's vault is untouched; the attacker's B token account got nothing.
    assert_eq!(token_amount(&mut rpc, a.vault_pda).await, 1_100_000);
    assert_eq!(token_amount(&mut rpc, b0_token).await, 700_000); // 1.0 funded - 0.3 staked

    // CONTROL: the SAME Bid withdraws legitimately against its OWN auction B.
    do_withdraw(&mut rpc, &b, b0.pubkey(), b0_token, 0, 300_000).await;
    assert_eq!(token_amount(&mut rpc, b0_token).await, 1_000_000); // refunded
    assert_eq!(token_amount(&mut rpc, b.vault_pda).await, 800_000); // winner pool remains
}

/// H-1: with min_bid == 0, a zero-amount bid is still rejected (InvalidAmount); a 1-unit bid works.
#[tokio::test]
async fn test_reject_zero_amount_bid() {
    let mut rpc = new_rpc().await;
    let ctx = setup_n(&mut rpc, 0, 1).await; // min_bid = 0

    let (bidder, token) = funded_bidder(&mut rpc, &ctx, 1_000_000).await;

    // amount == 0 must be rejected even though 0 >= min_bid(0).
    let zero = try_place_bid(&mut rpc, &ctx, &bidder, token, 0, C0, 0).await;
    assert!(zero.is_err(), "zero-amount bid MUST be rejected (H-1)");
    assert_eq!(token_amount(&mut rpc, ctx.vault_pda).await, 0);

    // A positive bid still works (sanity).
    let ok = try_place_bid(&mut rpc, &ctx, &bidder, token, 0, C0, 1).await;
    assert!(ok.is_ok(), "positive bid should succeed");
    assert_eq!(token_amount(&mut rpc, ctx.vault_pda).await, 1);
}
