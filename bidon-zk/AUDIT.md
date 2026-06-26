# bidon-zk — Internal Security Audit

**Program:** `bidon-zk` (parimutuel USDC auctions on Solana via Light Protocol ZK Compression)
**Program ID:** `4Pfc1jdDXX4EMFoe7FxNGMfQmSgZSegJn7DCHkxbnfXz`
**Scope:** `programs/bidon-zk/src/lib.rs` (the whole program, ~1135 lines) + bid/withdraw/claim/close paths.
**Method:** 5 independent adversarial reviewers, each owning one dimension (fund-safety & accounting; top-N
winners logic; compressed-account / Light-CPI binding; access control & account validation; arithmetic &
economic/griefing), followed by orchestrator verification of every flagged finding against source.
**Status:** internal pre-mainnet review. Not a substitute for a paid external audit, but the findings below
(esp. C-1) are real and were verified line-by-line.

---

## Summary

| # | Severity | Title | Status |
|---|---|---|---|
| **C-1** | **Critical** | `withdraw` doesn't bind the compressed `Bid` to the auction → cross-auction vault drain | ✅ **Fixed** |
| **H-1** | **High** | `amount == 0` accepted (min_bid floor missing); `InvalidAmount` defined but never enforced → relayer-funded spam | ✅ **Fixed** |
| **M-1** | **Medium** | `close_auction` hard-requires `vault.amount == 0` → stranded dust / un-cranked losers permanently lock relayer rent | ✅ **Fixed** (new `force_close_auction`) |
| **M-2** | **Medium** | No max auction duration → long-lock griefing of rent + stakes | ✅ **Fixed** (90-day cap) |
| **L-1** | **Low** | `top_up_bid` didn't rebind the `Bid` to the auction (benign, but inconsistent) | ✅ **Fixed** |
| **L-2** | **Low** | Tie at eviction boundary keeps larger-pid incumbent, contradicting the `(total desc, pid asc)` doc | ◻️ Documented trade-off |
| **L-3** | **Low/Info** | `withdraw` transfers refund *before* closing the Bid (safe by atomicity; close-first is cleaner) | ◻️ Optional |

**Verdict:** the money-movement core is fundamentally sound — fund conservation holds in any claim/withdraw
ordering, every caller-supplied "current amount/owner/total" is cryptographically bound to committed state by
Light's inclusion proof, the winner/loser partition is exact and frozen at `end_time`, and all arithmetic is
`checked_*`. The one **critical** defect (C-1) was a missing binding in `withdraw` that the prior informal
review had added to `raise_bid`/`top_up_bid` but not here. It is now fixed. Ship the M-1 follow-up before
mainnet to close the only remaining stuck-funds vector.

---

## C-1 — CRITICAL — Cross-auction vault drain via unbound `Bid` in `withdraw`

**Found independently by 2 of 5 reviewers; verified against source.**

`withdraw` drains `ctx.accounts.auction`'s vault by `bid_current_amount` and closes a compressed `Bid`
identified by `bid_meta` + `proof`. The Light proof guarantees the `Bid` leaf is real and program-owned — but
**not** that it belongs to the auction whose vault is being drained. The `Bid` address is
`derive_address([BID_SEED, auction_key, proposal_id, bidder], V2)`, yet `withdraw` never re-derived it against
`ctx.accounts.auction` (unlike `raise_bid`/`top_up_bid`, which rebind their *proposal* address — the "Fix #3"
guard). The `is_winner` gate reads the *chosen* auction's `winners`, so a foreign proposal_id reads as a loser
and passes.

**Attack:** Attacker stakes `X` on a losing proposal in their own auction **A** (a real, program-owned Bid).
After `end_time`, they call `withdraw` passing auction **B** (any funded victim auction) as the accounts, but
auction A's `bid_meta`/`proof`/`proposal_id`/`bidder`/`bid_current_amount = X`. The winner-gate passes (A's pid
isn't in B's winners), `bidder_token.owner == bidder` passes, and **`X` is transferred out of B's vault** to the
attacker, then A's Bid is closed. By making their A proposal a *winner in A*, the attacker also reclaims `X` from
A as creator — netting `+X` per drained victim vault, repeatable. **Direct theft of USDC + denial of settlement**
(B's vault desyncs from its `winners`, so honest B losers can no longer all withdraw and/or claim trips the
`winners_pot <= vault.amount` gate, freezing the pool).

**Fix (applied):** re-derive the expected Bid address from `ctx.accounts.auction` and require equality *before*
the refund:
```rust
let (expected_bid_address, _) = derive_address(
    &[BID_SEED, auction_key.as_ref(), pid_le.as_ref(), bidder.as_ref()],
    &Pubkey::new_from_array(ADDRESS_TREE_V2), &crate::ID);
require!(bid_meta.address == expected_bid_address, BidonError::ProposalIdMismatch);
```

---

## H-1 — HIGH — Zero/no-op bids accepted; relayer-funded spam

Every bid entrypoint gated only on `amount >= min_bid`; `create_auction` put no floor on `min_bid`; and the
defined `BidonError::InvalidAmount` ("must be greater than zero") was **never referenced** (verified: the only
occurrence is its definition). With `min_bid == 0`, a `0`-USDC bid is accepted — each one makes the **relayer**
(the Light fee payer) create two rent-free compressed accounts at real SOL cost, with no per-auction cap. An
attacker turns the gasless relayer into a free faucet (asymmetric economic DoS) and can pollute the winners array
with zero-total junk.

**Fix (applied):** `require!(amount > 0, BidonError::InvalidAmount);` in `place_bid`/`raise_bid`/`top_up_bid`.
**Recommended follow-up:** a protocol-level `min_bid` floor and a per-auction `proposal_count` cap so the
attacker's cost is always ≥ the relayer's marginal cost.

---

## M-1 — MEDIUM — `close_auction` can be permanently bricked (rent lock)

`close_auction` requires `vault.amount == 0`. That zero depends on **every** losing bid being withdrawn — which
is driven by an off-chain feed (`bidon-store`/keeper). Three ways it never reaches zero: (a) a loser whose USDC
token account no longer exists can't be refunded; (b) anyone can `spl-token transfer` dust straight into the
(derivable) SPL vault; (c) the off-chain index drops a `(proposal_id, bidder)` entry. Any of these strands the
residual **and** permanently locks the relayer's Auction+vault rent — the only rent in the system — silently
breaking the gasless economic loop.

**Fix (applied — new permissionless `force_close_auction`; `close_auction` left unchanged, no FE/keeper break):** a time-gated **force-close/sweep**:
after `end_time + GRACE` with `creator_paid`, allow `close_auction` to sweep any residual `vault.amount` to a
designated sink (e.g. `fee_receiver`) before closing, instead of hard-requiring exactly zero. This makes close
always completable and recovers stray funds instead of trapping them. (Requires adding a sink token account to
the `CloseAuction` context — do it deliberately, with a test.)

---

## M-2 — MEDIUM — No max duration → long-lock griefing  ✅ Fixed

`create_auction` accepted any `duration_secs > 0` (overflow-guarded but unbounded). A griefer could set a
decades-out `end_time`, place one bid, and lock the relayer's rent + that stake until then. **Fix (applied):**
`MAX_DURATION_SECS = 365 days` (year-long auctions are a supported product feature; the cap only blocks
absurd/overflow durations, and matches the frontend's 365-day max); `require!(duration_secs > 0 && duration_secs <= MAX_DURATION_SECS, ...)`.

---

## Deployment & test status (2026-06-26)
- **Deployed to devnet** (program `4Pfc…`, latest upgrade slot **472107430** — incl. C-1/H-1/M-2 + **M-1
  `force_close_auction`**; data 413664 bytes; same program id → the Render frontend runs against the fixed code).
  Fixes built clean via `cargo build-sbf`. Earlier slots: 472091090 (C-1/H-1/M-2), 470942061 (pre-audit).
- **Integration tests NOT executed.** The fixes are verified by: clean SBF compile + line-by-line trace of C-1 +
  two independent reviewers + C-1 being the exact binding pattern already used (and tested) in raise_bid/top_up_bid.
  The full `cargo test-sbf` could not run locally: the gnark ZK prover is non-viable in the available WSL
  (~47 min for a single proof). **Recommended real-world smoke test on devnet via the UI:** create an auction,
  place a bid, and confirm a LOSER withdraw still refunds correctly (verifies the new C-1 binding doesn't reject
  legitimate withdrawals), plus a >90-day (e.g. 365-day) auction creates successfully.

---

## L-1 — LOW — `top_up_bid` Bid not rebound to auction  ✅ Fixed
Same structural gap as C-1 but benign (bidder signs, funds their own bid — no theft). Fixed for consistency by
rebinding the Bid address, so every instruction that touches a Bid now enforces the same auction binding.

## L-2 — LOW — Tie-break eviction vs documented order
Eviction uses `strictly_beats` (`>`, total only) while ordering uses `ranks_above` (total desc, **pid asc**).
At an exact eviction-boundary tie, the larger-pid incumbent keeps the slot, contradicting the doc's
`(total desc, pid asc)` promise. **Money is conserved** (the displaced equal-total proposal just refunds), and
the "ties keep incumbent" behavior is intentional (frontrun resistance, N==1 byte-compat with legacy). Action:
either align eviction with `ranks_above`, or soften the doc comment so it doesn't overstate the tie-break.

## L-3 — LOW/Info — `withdraw` refund precedes Bid close
The SPL refund (line ~551) runs before the compressed Bid close (~579). Safe today: a wrong `bid_current_amount`
makes the close fail and the whole tx (incl. the refund) reverts atomically; a replay finds the leaf nullified.
Optional hardening: close first, then refund, so nullification precedes money movement (don't rely on rollback).

---

## Verified sound (no action)

- **Fund conservation:** `winners_pot + Σ(loser refunds) == total_staked == accounted_vault` in any ordering of
  claim/withdraw. Claim pays exactly `winners_pot` once (`creator_paid` flag); losers refund exactly their bound
  `Bid.amount`; the `winners_pot <= vault.amount` gate is a fail-closed safety net that honest flow can't trip.
- **Light-CPI binding:** `new_mut`/`new_close` hash the full struct + address + program owner; lying about
  `proposal_current_total`, `proposal_creator`, `content_hash`, `bid_current_amount`, `bidder`, or `proposal_id`
  yields a leaf hash with no inclusion proof → revert. *(Conditional on Light SDK proof semantics — pin &
  re-confirm the `light-sdk` version at deploy; this is the load-bearing assumption behind C-1's fix and the
  no-spoofing guarantees.)*
- **Winner/loser partition:** both gates read the SAME `winners[0..filled]`, frozen after `end_time` (all
  mutators require `now < end_time`). `[0..filled]` excludes default pid=0 tail slots, so a real pid=0 is never
  confused with an empty slot. Exact, non-overlapping partition.
- **Top-N engine:** insertion-sort stays sorted under monotonic-increasing totals; `winners[i].total` always
  equals the proposal's current aggregate; `winner_count` validated 1..=10 with no setter, plus a defensive
  clamp — no OOB/panic.
- **Fee math:** `fee = floor(pot*bps/10000)`, `payout = pot - fee`, computed once on the whole pot → exact
  `payout + fee == pot`, dust to creator, `checked_mul`/`checked_div`. No drift.
- **Access control:** every PDA seed/bump-checked; USDC mint pinned to `config.usdc_mint` on every path;
  `claim` recipients owner+mint-constrained to the real creator/fee_receiver; `set_config` owner-gated;
  `initialize` non-reinitializable; `rent_recipient` address-bound to `auction.rent_payer`; permissionless
  `claim`/`withdraw`/`close` cannot redirect funds *within* an auction. One-Bid-per-(auction,pid,bidder) and
  nullify-on-close prevent double-stake/double-refund. `id == auction_count` sequencing is race-safe.
- **Arithmetic:** all add/mul/sub/div in money paths are `checked_*`; no unchecked op found.

---

## Fixes applied in this pass (source only — redeploy required)
1. C-1 `withdraw` Bid↔auction binding (critical).
2. H-1 `require!(amount > 0)` in place/raise/top_up.
3. M-2 `MAX_DURATION_SECS` 90-day cap.
4. L-1 `top_up_bid` Bid binding (consistency).

## Recommended before mainnet (not yet applied)
- **M-1 keeper wiring** — `force_close_auction` is deployed; wire the keeper to invoke it for auctions stuck past the 7-day grace (it's permissionless on-chain but not auto-called yet).
- H-1 follow-ups: protocol `min_bid` floor + per-auction proposal cap (bound relayer cost on-chain).
- Pin and re-confirm the `light-sdk` version; the binding guarantees above depend on its proof semantics.
- Regression tests for C-1/H-1 (added alongside this audit) + re-run full `cargo test-sbf`.
