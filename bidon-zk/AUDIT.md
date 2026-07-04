# bidon-zk — Security Review Summary

**Program:** `bidon-zk` — parimutuel USDC auctions on Solana (Anchor 0.31 + Light Protocol ZK Compression)
**Program ID (devnet):** `4Pfc1jdDXX4EMFoe7FxNGMfQmSgZSegJn7DCHkxbnfXz`
**Status:** internal, pre-mainnet. A third-party audit is planned before mainnet.

bidon-zk has gone through several rounds of internal adversarial security review — independent
reviewers each owning one dimension (fund safety & accounting, top-N winner logic, compressed-account
/ Light-CPI binding, access control & account validation, arithmetic & economic griefing) — plus a
separate off-chain perimeter review (gasless relayer, permissionless keeper, web proxy, off-chain
store). Every finding was verified against source and remediated; on-chain fixes ship with regression
tests run under `cargo test-sbf`.

This public file is a high-level summary. Detailed findings with reproduction steps are kept in a
private internal report and shared with auditors and grant reviewers on request (responsible
disclosure — please report issues via [SECURITY.md](../SECURITY.md)).

## Findings — all remediated

| # | Severity | Area | Summary | Status |
|---|----------|------|---------|--------|
| C-1 | Critical | on-chain | Compressed-account ↔ auction binding on the refund path | ✅ Fixed |
| H-1 | High | on-chain | Bid-amount input validation | ✅ Fixed |
| M-1 | Medium | on-chain | Rent recovery for fully-settled auctions | ✅ Addressed |
| M-2 | Medium | on-chain | Bounded auction duration | ✅ Fixed |
| L-1..3 | Low | on-chain | Binding consistency & tie-break documentation | ✅ Addressed |
| P-\* | Crit–Med | perimeter | Secret handling, relayer policy filter, per-IP rate-limiting, security headers | ✅ Fixed |
| N-1 | High | keeper | Account-creation hardening on the permissionless crank | ✅ Fixed |
| N-2 | Medium | on-chain | Anti-snipe end-time-extension enforcement | ✅ Fixed |
| N-3 | Medium | store | Authentication on off-chain feed writes | ✅ Fixed |
| N-4 | Low | store | Authentication on the display-name registry | ✅ Fixed |

## Verified sound (no action needed)

- **Fund conservation.** The winners' pot plus all loser refunds equals exactly what was staked into
  the vault, in any ordering of claims and withdrawals; every money-path operation is checked
  arithmetic.
- **Light-CPI binding.** Every stored field is committed to the compressed-account leaf hash and
  verified by an inclusion proof, so a caller cannot lie about totals, amounts, owners, or addresses.
- **Winner/loser partition.** Frozen at `end_time`; the claim and withdraw gates read the same
  on-chain winners array, so the partition cannot diverge.
- **Access control.** PDA seeds/bumps are enforced, the USDC mint is pinned on every token path, and
  the permissionless handlers (claim / withdraw / close) cannot redirect funds within an auction.
- **Economic anti-spam.** Creating an auction locks a small **refundable** USDC deposit from the
  creator into a single global vault, returned in full once at settlement or cancel (a bond, not a
  fee). Each auction reclaims exactly its own deposit at most once, so mass auction-creation spam ties
  up the spammer's own capital and no auction can drain another's deposit.

## Testing

The full integration suite passes under `cargo test-sbf` (Light Protocol `program-test`), including a
regression test for each remediated on-chain finding and for the refundable-deposit flow (deposit,
refund, double-refund prevention, and cross-auction deposit isolation).
