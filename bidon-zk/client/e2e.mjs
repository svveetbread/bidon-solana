// bidon-zk devnet e2e. Grows step by step. Currently: idempotent setup (mint + Config)
// and the non-Light path (create_auction, decode). Light bids are added next.
import { existsSync, readFileSync, writeFileSync } from 'fs';
import { Keypair, PublicKey, LAMPORTS_PER_SOL } from '@solana/web3.js';
import { createMint } from '@solana/spl-token';
import {
  PROGRAM_ID, RPC_URL, configPda, auctionPda, vaultPda,
  ixInitialize, ixCreateAuction, decodeConfig, decodeAuction,
  loadKeypair, connection, sendIx,
} from './lib.mjs';

const STATE = './.state.json';
const loadState = () => (existsSync(STATE) ? JSON.parse(readFileSync(STATE, 'utf8')) : {});
const saveState = (s) => writeFileSync(STATE, JSON.stringify(s, null, 2));

const FEE_BPS = 370;
const MIN_BID = 100_000n; // 0.1 USDC (6 decimals)

const ok = (cond, msg) => { if (!cond) throw new Error('ASSERT FAILED: ' + msg); console.log('  ✓', msg); };

async function setup(conn, relayer) {
  const state = loadState();
  let mint;
  if (state.mint) {
    mint = new PublicKey(state.mint);
    console.log('mint (reused):', mint.toBase58());
  } else {
    mint = await createMint(conn, relayer, relayer.publicKey, null, 6);
    state.mint = mint.toBase58(); saveState(state);
    console.log('mint (created):', mint.toBase58());
  }

  const config = configPda();
  let cfgAcc = await conn.getAccountInfo(config);
  if (!cfgAcc) {
    const sig = await sendIx(conn, ixInitialize({
      owner: relayer.publicKey, feeBps: FEE_BPS, feeReceiver: relayer.publicKey, usdcMint: mint,
    }), relayer, [], 'initialize');
    console.log('Config initialized:', sig);
    cfgAcc = await conn.getAccountInfo(config);
  }
  const cfg = decodeConfig(cfgAcc.data);
  return { mint, cfg };
}

async function testCreateAuction(conn, relayer, mint, cfg) {
  console.log('\n== create_auction (gasless: creator 0 SOL) ==');
  const id = cfg.auctionCount; // next id must equal auction_count
  const creator = Keypair.generate(); // 0 SOL — only signs
  const auction = auctionPda(id);
  const vault = vaultPda(auction);
  console.log('  id:', id.toString(), 'creator:', creator.publicKey.toBase58());

  const sig = await sendIx(conn, ixCreateAuction({
    id, minBid: MIN_BID, durationSecs: 3600n,
    creator: creator.publicKey, payer: relayer.publicKey, usdcMint: mint,
  }), relayer, [creator], 'create_auction');
  console.log('  sig:', sig);

  const aAcc = await conn.getAccountInfo(auction);
  ok(aAcc !== null, 'auction account created');
  const a = decodeAuction(aAcc.data);
  ok(a.id === id, `auction.id == ${id}`);
  ok(a.creator.equals(creator.publicKey), 'auction.creator == creator');
  ok(a.minBid === MIN_BID, `min_bid == ${MIN_BID}`);
  ok(a.feeBps === FEE_BPS, `fee_bps snapshot == ${FEE_BPS}`);
  ok(a.creatorPaid === false, 'creator_paid == false');
  ok(a.proposalCount === 0n, 'proposal_count == 0');
  ok(a.rentPayer.equals(relayer.publicKey), 'rent_payer == relayer');

  const vAcc = await conn.getAccountInfo(vault);
  ok(vAcc !== null, 'vault (SPL token account) created');

  // config.auction_count advanced
  const cfg2 = decodeConfig((await conn.getAccountInfo(configPda())).data);
  ok(cfg2.auctionCount === id + 1n, `config.auction_count advanced to ${id + 1n}`);

  return { id, creator, auction, vault };
}

async function main() {
  console.log('RPC:', RPC_URL, '\nprogram:', PROGRAM_ID.toBase58());
  const conn = connection();
  const relayer = loadKeypair('./.relayer.json');
  const bal = await conn.getBalance(relayer.publicKey);
  console.log('relayer:', relayer.publicKey.toBase58(), '|', (bal / LAMPORTS_PER_SOL).toFixed(4), 'SOL');

  const { mint, cfg } = await setup(conn, relayer);
  console.log('Config: fee', cfg.feeBps, 'usdc', cfg.usdcMint.toBase58(), 'auctions', cfg.auctionCount.toString());

  await testCreateAuction(conn, relayer, mint, cfg);

  console.log('\nE2E (non-Light) OK');
}

main().catch((e) => { console.error('E2E FAIL:', e); process.exit(1); });
