#![cfg(feature = "test-sbf")]

//! Top-N (winner_count) settlement tests. Every scenario asserts the master money
//! invariant after end_time: Σ(payouts + fee + refunds) == total_staked AND vault == 0,
//! i.e. not a single USDC is lost or paid out twice. The winners[0..filled] slice is the
//! single on-chain source of truth both claim and the withdraw gate read, so the
//! winner/loser sets are an exact, non-overlapping partition of all proposal_ids.

mod common;
use common::*;

use light_program_test::{program_test::LightProgramTest, Rpc};
use solana_sdk::{
    pubkey::Pubkey,
    signature::{Keypair, Signer},
};

const FEE_BPS: u64 = 370; // matches initialize_config

fn ch(n: u8) -> [u8; 32] {
    [n + 1; 32]
}

fn fee_of(pot: u64) -> u64 {
    pot * FEE_BPS / 10_000
}

/// Place a NEW proposal (pid must equal the current proposal_count) with a fresh funded
/// bidder; returns (bidder, token) so the caller can withdraw/refund-check later.
async fn place(
    rpc: &mut LightProgramTest,
    ctx: &Ctx,
    pid: u64,
    amount: u64,
) -> (Keypair, Pubkey) {
    let (b, t) = funded_bidder(rpc, ctx, amount).await;
    do_place_bid(rpc, ctx, &b, t, pid, ch(pid as u8), amount).await;
    (b, t)
}

/// N==1 must be byte-for-byte the legacy single-winner behavior: strict-`>` leader,
/// only the top proposal is unwithdrawable, everyone else refunds in full.
#[tokio::test]
async fn n1_eq_legacy() {
    let mut rpc = new_rpc().await;
    let ctx = setup_n(&mut rpc, MIN_BID, 1).await;

    let (a, a_t) = place(&mut rpc, &ctx, 0, 300_000).await; // loser
    let (b, b_t) = place(&mut rpc, &ctx, 1, 800_000).await; // winner
    let (c, c_t) = place(&mut rpc, &ctx, 2, 500_000).await; // loser

    let au = get_auction(&mut rpc, ctx.auction_pda).await;
    assert_eq!(au.winner_count, 1);
    assert_eq!(au.winners_filled, 1);
    assert_eq!(au.winners[0].proposal_id, 1);
    assert_eq!(au.winners[0].total, 800_000);
    // legacy fields kept in sync with winners[0].
    assert_eq!(au.winner_proposal, 1);
    assert_eq!(au.winner_amount, 800_000);
    assert_eq!(au.total_staked, 1_600_000);

    let creator_t = token_account(&mut rpc, &ctx.payer, ctx.mint, &ctx.creator.pubkey()).await;
    let fee_t = token_account(&mut rpc, &ctx.payer, ctx.mint, &ctx.owner.pubkey()).await;

    warp_past(&mut rpc, 3601);
    do_claim(&mut rpc, &ctx, creator_t, fee_t).await;

    let fee = fee_of(800_000);
    assert_eq!(token_amount(&mut rpc, creator_t).await, 800_000 - fee);
    assert_eq!(token_amount(&mut rpc, fee_t).await, fee);

    // The winner cannot withdraw; the two losers refund in full.
    assert!(try_withdraw(&mut rpc, &ctx, b.pubkey(), b_t, 1, 800_000)
        .await
        .is_err());
    do_withdraw(&mut rpc, &ctx, a.pubkey(), a_t, 0, 300_000).await;
    do_withdraw(&mut rpc, &ctx, c.pubkey(), c_t, 2, 500_000).await;
    assert_eq!(token_amount(&mut rpc, a_t).await, 300_000);
    assert_eq!(token_amount(&mut rpc, c_t).await, 500_000);

    // Master invariant: nothing lost, vault drained.
    let out = (800_000 - fee) + fee + 300_000 + 500_000;
    assert_eq!(out, au.total_staked);
    assert_eq!(token_amount(&mut rpc, ctx.vault_pda).await, 0);
}

/// N=3 with 5 distinct totals: top-3 pool goes to the creator (minus one fee on the pool),
/// the bottom-2 refund. Σ == total_staked, vault == 0.
#[tokio::test]
async fn top3_distinct() {
    let mut rpc = new_rpc().await;
    let ctx = setup_n(&mut rpc, MIN_BID, 3).await;

    let (w0, t0) = place(&mut rpc, &ctx, 0, 600_000).await; // winner #1
    let (_w1, _t1) = place(&mut rpc, &ctx, 1, 500_000).await; // winner #2
    let (_w2, _t2) = place(&mut rpc, &ctx, 2, 400_000).await; // winner #3
    let (l3, l3_t) = place(&mut rpc, &ctx, 3, 300_000).await; // loser
    let (l4, l4_t) = place(&mut rpc, &ctx, 4, 200_000).await; // loser

    let au = get_auction(&mut rpc, ctx.auction_pda).await;
    assert_eq!(au.winners_filled, 3);
    assert_eq!((au.winners[0].proposal_id, au.winners[0].total), (0, 600_000));
    assert_eq!((au.winners[1].proposal_id, au.winners[1].total), (1, 500_000));
    assert_eq!((au.winners[2].proposal_id, au.winners[2].total), (2, 400_000));
    assert_eq!(au.total_staked, 2_000_000);

    let creator_t = token_account(&mut rpc, &ctx.payer, ctx.mint, &ctx.creator.pubkey()).await;
    let fee_t = token_account(&mut rpc, &ctx.payer, ctx.mint, &ctx.owner.pubkey()).await;

    warp_past(&mut rpc, 3601);
    do_claim(&mut rpc, &ctx, creator_t, fee_t).await;

    let pot = 1_500_000u64; // 600k + 500k + 400k
    let fee = fee_of(pot);
    let payout = pot - fee;
    assert_eq!(token_amount(&mut rpc, creator_t).await, payout);
    assert_eq!(token_amount(&mut rpc, fee_t).await, fee);

    // A winner cannot withdraw; losers refund.
    assert!(try_withdraw(&mut rpc, &ctx, w0.pubkey(), t0, 0, 600_000)
        .await
        .is_err());
    do_withdraw(&mut rpc, &ctx, l3.pubkey(), l3_t, 3, 300_000).await;
    do_withdraw(&mut rpc, &ctx, l4.pubkey(), l4_t, 4, 200_000).await;
    assert_eq!(token_amount(&mut rpc, l3_t).await, 300_000);
    assert_eq!(token_amount(&mut rpc, l4_t).await, 200_000);

    let out = payout + fee + 300_000 + 200_000;
    assert_eq!(out, au.total_staked);
    assert_eq!(token_amount(&mut rpc, ctx.vault_pda).await, 0);
}

/// Equal totals at the top boundary: the result is deterministic by proposal_id (smaller pid
/// ranks higher), and a later equal-total challenger does NOT evict an incumbent.
#[tokio::test]
async fn tie_break_equal_totals() {
    let mut rpc = new_rpc().await;
    let ctx = setup_n(&mut rpc, MIN_BID, 2).await;

    let (w0, t0) = place(&mut rpc, &ctx, 0, 500_000).await; // seats first -> top
    let (w1, t1) = place(&mut rpc, &ctx, 1, 500_000).await; // seats second -> top
    let (l2, l2_t) = place(&mut rpc, &ctx, 2, 500_000).await; // equal, late -> excluded

    let au = get_auction(&mut rpc, ctx.auction_pda).await;
    assert_eq!(au.winners_filled, 2);
    // Smaller pids occupy the top; pid=2 is the odd one out despite equal total.
    assert_eq!(au.winners[0].proposal_id, 0);
    assert_eq!(au.winners[1].proposal_id, 1);
    assert!(!au.winners[0..2].iter().any(|w| w.proposal_id == 2));

    let creator_t = token_account(&mut rpc, &ctx.payer, ctx.mint, &ctx.creator.pubkey()).await;
    let fee_t = token_account(&mut rpc, &ctx.payer, ctx.mint, &ctx.owner.pubkey()).await;

    warp_past(&mut rpc, 3601);
    do_claim(&mut rpc, &ctx, creator_t, fee_t).await;

    let pot = 1_000_000u64;
    let fee = fee_of(pot);

    // Both seated winners are unwithdrawable; the excluded equal-total proposal refunds.
    assert!(try_withdraw(&mut rpc, &ctx, w0.pubkey(), t0, 0, 500_000)
        .await
        .is_err());
    assert!(try_withdraw(&mut rpc, &ctx, w1.pubkey(), t1, 1, 500_000)
        .await
        .is_err());
    do_withdraw(&mut rpc, &ctx, l2.pubkey(), l2_t, 2, 500_000).await;
    assert_eq!(token_amount(&mut rpc, l2_t).await, 500_000);

    let out = (pot - fee) + fee + 500_000;
    assert_eq!(out, au.total_staked);
    assert_eq!(token_amount(&mut rpc, ctx.vault_pda).await, 0);
}

/// Boundary N/N+1: an out-of-top proposal raises strictly above the bottom winner, evicts it,
/// and the evicted proposal becomes withdrawable while the new entrant does not.
#[tokio::test]
async fn boundary_evict() {
    let mut rpc = new_rpc().await;
    let ctx = setup_n(&mut rpc, MIN_BID, 2).await;

    let (_w0, _t0) = place(&mut rpc, &ctx, 0, 500_000).await; // stays top
    let (l1, l1_t) = place(&mut rpc, &ctx, 1, 400_000).await; // bottom winner, then evicted
    let (p2, p2_t) = place(&mut rpc, &ctx, 2, 300_000).await; // out of top initially

    let au = get_auction(&mut rpc, ctx.auction_pda).await;
    assert_eq!(au.winners_filled, 2);
    assert!(!au.winners[0..2].iter().any(|w| w.proposal_id == 2));

    // New backer pushes proposal 2 to 500k -> strictly beats pid1 (400k) -> evicts it.
    let (r2, r2_t) = funded_bidder(&mut rpc, &ctx, 200_000).await;
    do_raise_bid(&mut rpc, &ctx, &r2, r2_t, 2, 200_000).await;

    let au = get_auction(&mut rpc, ctx.auction_pda).await;
    assert_eq!(au.winners_filled, 2);
    assert_eq!(au.winners[0].proposal_id, 0); // pid0 (500k) still on top (tie keeps smaller pid)
    assert_eq!(au.winners[1].proposal_id, 2); // pid2 (500k) took the bottom slot
    assert!(!au.winners[0..2].iter().any(|w| w.proposal_id == 1)); // pid1 evicted
    assert_eq!(au.total_staked, 1_400_000);

    let creator_t = token_account(&mut rpc, &ctx.payer, ctx.mint, &ctx.creator.pubkey()).await;
    let fee_t = token_account(&mut rpc, &ctx.payer, ctx.mint, &ctx.owner.pubkey()).await;

    warp_past(&mut rpc, 3601);
    do_claim(&mut rpc, &ctx, creator_t, fee_t).await;

    let pot = 1_000_000u64; // pid0 500k + pid2 500k
    let fee = fee_of(pot);

    // pid2 is now a winner (both backers locked); pid1 (evicted) refunds.
    assert!(try_withdraw(&mut rpc, &ctx, p2.pubkey(), p2_t, 2, 300_000)
        .await
        .is_err());
    assert!(try_withdraw(&mut rpc, &ctx, r2.pubkey(), r2_t, 2, 200_000)
        .await
        .is_err());
    do_withdraw(&mut rpc, &ctx, l1.pubkey(), l1_t, 1, 400_000).await;
    assert_eq!(token_amount(&mut rpc, l1_t).await, 400_000);

    let out = (pot - fee) + fee + 400_000;
    assert_eq!(out, au.total_staked);
    assert_eq!(token_amount(&mut rpc, ctx.vault_pda).await, 0);
}

/// proposal_count <= N: every proposal wins, there are no losers, the whole pool goes to the
/// creator in one claim and the vault is empty immediately (no withdraws).
#[tokio::test]
async fn all_proposals_win() {
    let mut rpc = new_rpc().await;
    let ctx = setup_n(&mut rpc, MIN_BID, 10).await;

    let (w0, t0) = place(&mut rpc, &ctx, 0, 300_000).await;
    let (_w1, _t1) = place(&mut rpc, &ctx, 1, 400_000).await;
    let (_w2, _t2) = place(&mut rpc, &ctx, 2, 500_000).await;

    let au = get_auction(&mut rpc, ctx.auction_pda).await;
    assert_eq!(au.winners_filled, 3);
    assert_eq!(au.total_staked, 1_200_000);

    let creator_t = token_account(&mut rpc, &ctx.payer, ctx.mint, &ctx.creator.pubkey()).await;
    let fee_t = token_account(&mut rpc, &ctx.payer, ctx.mint, &ctx.owner.pubkey()).await;

    warp_past(&mut rpc, 3601);
    do_claim(&mut rpc, &ctx, creator_t, fee_t).await;

    let pot = 1_200_000u64;
    let fee = fee_of(pot);
    assert_eq!(token_amount(&mut rpc, creator_t).await, pot - fee);
    assert_eq!(token_amount(&mut rpc, fee_t).await, fee);
    // Vault empty right after claim — there is nothing to withdraw.
    assert_eq!(token_amount(&mut rpc, ctx.vault_pda).await, 0);

    // Nobody can withdraw — all proposals are winners.
    assert!(try_withdraw(&mut rpc, &ctx, w0.pubkey(), t0, 0, 300_000)
        .await
        .is_err());

    let out = (pot - fee) + fee;
    assert_eq!(out, au.total_staked);
}

/// winner_count must be 1..=10; 0 and 11 are rejected, 1 and 10 accepted.
#[tokio::test]
async fn winner_count_validation() {
    let mut rpc = new_rpc().await;
    let payer = rpc.get_payer().insecure_clone();
    let owner = Keypair::new();
    rpc.airdrop_lamports(&owner.pubkey(), 5_000_000_000)
        .await
        .unwrap();
    let mint = create_mint(&mut rpc, &payer).await;
    let (config_pda, _) = Pubkey::find_program_address(&[b"config"], &bidon_zk::ID);
    initialize_config(&mut rpc, &owner, config_pda, mint).await;
    let creator = Keypair::new();

    let pdas = |id: u64| {
        let (a, _) = Pubkey::find_program_address(&[b"auction", &id.to_le_bytes()], &bidon_zk::ID);
        let (v, _) = Pubkey::find_program_address(&[b"vault", a.as_ref()], &bidon_zk::ID);
        (a, v)
    };

    // id=0, winner_count=0 -> rejected (auction_count stays 0 on revert).
    let (a0, v0) = pdas(0);
    let r = rpc
        .create_and_send_transaction(
            &[create_auction_ix(
                &payer, &creator, config_pda, a0, v0, mint, 0, MIN_BID, 0,
            )],
            &payer.pubkey(),
            &[&payer, &creator],
        )
        .await;
    assert!(r.is_err(), "winner_count=0 must fail");

    // id=0, winner_count=11 -> rejected.
    let r = rpc
        .create_and_send_transaction(
            &[create_auction_ix(
                &payer, &creator, config_pda, a0, v0, mint, 0, MIN_BID, 11,
            )],
            &payer.pubkey(),
            &[&payer, &creator],
        )
        .await;
    assert!(r.is_err(), "winner_count=11 must fail");

    // id=0, winner_count=1 -> ok (auction_count -> 1).
    let r = rpc
        .create_and_send_transaction(
            &[create_auction_ix(
                &payer, &creator, config_pda, a0, v0, mint, 0, MIN_BID, 1,
            )],
            &payer.pubkey(),
            &[&payer, &creator],
        )
        .await;
    assert!(r.is_ok(), "winner_count=1 must succeed");

    // id=1, winner_count=10 -> ok (boundary max).
    let (a1, v1) = pdas(1);
    let r = rpc
        .create_and_send_transaction(
            &[create_auction_ix(
                &payer, &creator, config_pda, a1, v1, mint, 1, MIN_BID, 10,
            )],
            &payer.pubkey(),
            &[&payer, &creator],
        )
        .await;
    assert!(r.is_ok(), "winner_count=10 must succeed");
}

/// After a full settlement (claim + all losing withdraws drain the vault), close_auction
/// succeeds and both the Auction and its vault are gone (rent returned to the relayer).
#[tokio::test]
async fn close_after_drain() {
    let mut rpc = new_rpc().await;
    let ctx = setup_n(&mut rpc, MIN_BID, 2).await;

    let (_w0, _t0) = place(&mut rpc, &ctx, 0, 500_000).await; // winner
    let (_w1, _t1) = place(&mut rpc, &ctx, 1, 400_000).await; // winner
    let (l2, l2_t) = place(&mut rpc, &ctx, 2, 300_000).await; // loser

    let creator_t = token_account(&mut rpc, &ctx.payer, ctx.mint, &ctx.creator.pubkey()).await;
    let fee_t = token_account(&mut rpc, &ctx.payer, ctx.mint, &ctx.owner.pubkey()).await;

    warp_past(&mut rpc, 3601);
    do_claim(&mut rpc, &ctx, creator_t, fee_t).await;
    do_withdraw(&mut rpc, &ctx, l2.pubkey(), l2_t, 2, 300_000).await;
    assert_eq!(token_amount(&mut rpc, ctx.vault_pda).await, 0);

    do_close_auction(&mut rpc, &ctx).await;
    assert!(rpc.get_account(ctx.auction_pda).await.unwrap().is_none());
    assert!(rpc.get_account(ctx.vault_pda).await.unwrap().is_none());
}
