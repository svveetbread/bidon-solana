#![cfg(feature = "test-sbf")]
//! Regression tests for the internal audit fixes (see AUDIT.md).
//!  - C-1 (Critical): withdraw must reject a Bid that belongs to a DIFFERENT auction
//!    (cross-auction vault drain).
//!  - H-1 (High): zero-amount bids must be rejected even when min_bid == 0.
//!  - N-2 (round 2): on a v2 anti-snipe auction, every bid MUST supply the AuctionExt companion —
//!    omitting the optional account (to silently disable the end_time extension) is rejected.

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

    // Vault = 1.1 staked + 0.5 unclaimed creator deposit (schema 3).
    let a_vault_before = token_amount(&mut rpc, a.vault_pda).await;
    assert_eq!(a_vault_before, 1_100_000 + bidon_zk::CREATOR_DEPOSIT);

    // ATTACK: present B's proposal-0 Bid against auction A's accounts/vault. Must be rejected.
    let attack = try_withdraw_xauction(&mut rpc, &a, &b, b0.pubkey(), b0_token, 0, 300_000).await;
    assert!(attack.is_err(), "cross-auction withdraw MUST be rejected (C-1)");

    // A's vault is untouched; the attacker's B token account got nothing.
    assert_eq!(token_amount(&mut rpc, a.vault_pda).await, 1_100_000 + bidon_zk::CREATOR_DEPOSIT);
    assert_eq!(token_amount(&mut rpc, b0_token).await, 700_000); // 1.0 funded - 0.3 staked

    // CONTROL: the SAME Bid withdraws legitimately against its OWN auction B.
    do_withdraw(&mut rpc, &b, b0.pubkey(), b0_token, 0, 300_000).await;
    assert_eq!(token_amount(&mut rpc, b0_token).await, 1_000_000); // refunded
    // Winner pool 0.8 remains + B's unclaimed 0.5 deposit.
    assert_eq!(token_amount(&mut rpc, b.vault_pda).await, 800_000 + bidon_zk::CREATOR_DEPOSIT);
}

/// H-1: with min_bid == 0, a zero-amount bid is still rejected (InvalidAmount); a 1-unit bid works.
#[tokio::test]
async fn test_reject_zero_amount_bid() {
    let mut rpc = new_rpc().await;
    let ctx = setup_n(&mut rpc, 0, 1).await; // min_bid = 0

    let (bidder, token) = funded_bidder(&mut rpc, &ctx, 1_000_000).await;

    // Vault starts at CREATOR_DEPOSIT (schema 3): the deposit sits there from create.
    let base = bidon_zk::CREATOR_DEPOSIT;

    // amount == 0 must be rejected even though 0 >= min_bid(0). Vault unchanged (deposit only).
    let zero = try_place_bid(&mut rpc, &ctx, &bidder, token, 0, C0, 0).await;
    assert!(zero.is_err(), "zero-amount bid MUST be rejected (H-1)");
    assert_eq!(token_amount(&mut rpc, ctx.vault_pda).await, base);

    // A positive bid still works (sanity): deposit + 1.
    let ok = try_place_bid(&mut rpc, &ctx, &bidder, token, 0, C0, 1).await;
    assert!(ok.is_ok(), "positive bid should succeed");
    assert_eq!(token_amount(&mut rpc, ctx.vault_pda).await, base + 1);
}

/// N-2: on a v2 (anti-snipe) auction the AuctionExt companion is MANDATORY on every bid. Omitting it
/// (passing the program-id `None` sentinel) previously let a sniper silently disable the end_time
/// extension and take the final slot uncontested. The bid must now revert (AntisnipeExtRequired), with
/// no funds moved; supplying the companion succeeds.
#[tokio::test]
async fn test_bid_without_antisnipe_ext_rejected() {
    let mut rpc = new_rpc().await;
    let ctx = setup(&mut rpc, MIN_BID).await; // fresh auction → anti-snipe schema (v2)

    // Sanity: new auctions are created on the deposit schema (3), which still enforces the companion.
    assert_eq!(get_auction(&mut rpc, ctx.auction_pda).await.schema_version, 3);

    // Vault starts at CREATOR_DEPOSIT (schema 3).
    let base = bidon_zk::CREATOR_DEPOSIT;
    let (bidder, token) = funded_bidder(&mut rpc, &ctx, 1_000_000).await;

    // ATTACK: place_bid WITHOUT the companion must be rejected; vault stays at the deposit (atomic revert).
    let no_ext = try_place_bid_without_ext(&mut rpc, &ctx, &bidder, token, 0, C0, 300_000).await;
    assert!(no_ext.is_err(), "bid without AuctionExt MUST be rejected on a v3 auction (N-2)");
    assert_eq!(token_amount(&mut rpc, ctx.vault_pda).await, base, "no funds moved on reverted bid");

    // CONTROL: the same bid WITH the companion succeeds.
    let with_ext = try_place_bid(&mut rpc, &ctx, &bidder, token, 0, C0, 300_000).await;
    assert!(with_ext.is_ok(), "bid with AuctionExt should succeed");
    assert_eq!(token_amount(&mut rpc, ctx.vault_pda).await, base + 300_000);
}
