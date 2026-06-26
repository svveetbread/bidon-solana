# bidon

**Gasless USDC "creator auctions" on Solana.** Fans back a creator's ideas/options with USDC; after
the timer ends, the most-backed idea(s) fund the creator (minus a small fee) and everyone else is
refunded — a parimutuel auction. Bidders and creators sign with **0 SOL** (a relayer fronts fees);
per-bid state is **rent-free** via [Light Protocol](https://lightprotocol.com) ZK Compression.

- **Program (devnet):** [`4Pfc1jdDXX4EMFoe7FxNGMfQmSgZSegJn7DCHkxbnfXz`](https://explorer.solana.com/address/4Pfc1jdDXX4EMFoe7FxNGMfQmSgZSegJn7DCHkxbnfXz?cluster=devnet)
- **Security audit:** [`bidon-zk/AUDIT.md`](bidon-zk/AUDIT.md)
- **License:** [Apache-2.0](LICENSE)

## How it works

Hybrid account model:

- **Config / Auction / Vault** — regular accounts (the hot leaderboard + the USDC pool). The only
  rent in the system; the relayer fronts it and reclaims it when the auction is closed.
- **ProposalTotal / Bid** — rent-free **compressed** accounts (Light): per-proposal aggregate and
  per-backer position. `place_bid` / `raise_bid` / `top_up_bid` / `withdraw` cost ≈ $0.001 spent, $0 frozen.

Gates are purely time-based (`now >= end_time`); there is no finalize step. The top-N winners are kept
on-chain in `Auction.winners[]`. After end: winners' pool → creator (`claim_winnings`); losers reclaim
their stake (`withdraw`, permissionless, no deadline); GC reclaims rent (`close_auction` /
`force_close_auction`).

## Layout

```
bidon-zk/
  programs/bidon-zk/   Anchor program (the on-chain logic) + tests
  client/              off-chain clients (keeper auto-resolver, Light helpers)
  kora/                Kora gasless-relayer config (Dockerfile, kora.toml)
e2e/                   end-to-end devnet scripts
```

## Build & test

Requires the Solana/Anchor toolchain + the [ZK Compression CLI](https://www.zkcompression.com/) (Light
test-validator) for the integration tests.

```bash
cd bidon-zk
cargo build-sbf            # build the program
cargo test-sbf             # run the full test suite (incl. audit regression tests)
```

## Verifying

The deployed bytecode is public on-chain (Explorer link above). The readable source here compiles to it;
a reproducible **verified build** (`solana-verify`) ties the two together — see AUDIT.md for status.
