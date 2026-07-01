# Security Policy

bidon is a gasless parimutuel USDC auctions dApp on Solana. It is currently on **devnet** (beta).

Program ID (devnet): `4Pfc1jdDXX4EMFoe7FxNGMfQmSgZSegJn7DCHkxbnfXz`
Site: https://bidon.live

## Reporting a vulnerability

Please report security issues **privately**, not via public issues:

- Preferred: open a private advisory at
  https://github.com/svveetbread/bidon-solana/security/advisories/new
- Or reach out via X: [@svveetbread](https://x.com/svveetbread)

Please include: a description, impact, reproduction steps, and any relevant transactions/addresses.

## Scope

- On-chain program (`bidon-zk`, Anchor + Light Protocol ZK Compression).
- Off-chain relayer (Kora), keeper, and the frontend/proxy on bidon.live.

## What to expect

- We aim to acknowledge reports within a few days.
- This is a solo/beta project on devnet; funds at risk are test-only for now.
- Coordinated disclosure is appreciated. A public audit is planned before mainnet.

## Non-qualifying

- Findings that require the relayer's private key or Cloudflare/Railway account access.
- Denial-of-service on the free devnet infrastructure (known, mitigated pre-mainnet).
