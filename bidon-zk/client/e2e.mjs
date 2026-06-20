// bidon-zk devnet e2e. Grows step by step.
//  - non-Light: setup (mint + Config), create_auction
//  - Light: place_bid (proof via Helius Photon, tx sent over public RPC)
import { existsSync, readFileSync, writeFileSync } from 'fs';
import { createHash } from 'crypto';
import { Keypair, PublicKey, LAMPORTS_PER_SOL } from '@solana/web3.js';
import { createMint, getOrCreateAssociatedTokenAccount, mintTo } from '@solana/spl-token';
import BN from 'bn.js';
import {
  PROGRAM_ID, RPC_URL, HELIUS_RPC, configPda, auctionPda, vaultPda,
  ixInitialize, ixCreateAuction, decodeConfig, decodeAuction,
  loadKeypair, connection, sendIx, cuLimit,
} from './lib.mjs';
import { lightRpc, buildPlaceBid, decodeProposalTotal, decodeBid } from './light.mjs';

const STATE = './.state.json';
const loadState = () => (existsSync(STATE) ? JSON.parse(readFileSync(STATE, 'utf8')) : {});
const saveState = (s) => writeFileSync(STATE, JSON.stringify(s, null, 2));

const FEE_BPS = 370;
const MIN_BID = 100_000n;        // 0.1 USDC
const BID_AMOUNT = 1_000_000n;   // 1 USDC

const ok = (cond, msg) => { if (!cond) throw new Error('ASSERT FAILED: ' + msg); console.log('  ✓', msg); };
const contentHash = (text) => createHash('sha256').update(text).digest();
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

// Photon may index a freshly created compressed account with a lag — retry-wait.
async function waitCompressed(lrpc, addrPk, tries = 15, delayMs = 4000) {
  for (let i = 0; i < tries; i++) {
    const acc = await lrpc.getCompressedAccount(new BN(addrPk.toBytes()));
    if (acc) return acc;
    process.stdout.write(`  …waiting for Photon to index (${i + 1}/${tries})\r`);
    await sleep(delayMs);
  }
  return null;
}

async function setup(conn, relayer) {
  const state = loadState();
  let mint;
  if (state.mint) { mint = new PublicKey(state.mint); console.log('mint (reused):', mint.toBase58()); }
  else {
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
  return { mint, cfg: decodeConfig(cfgAcc.data) };
}

async function fundedBidder(conn, relayer, mint, amount) {
  const bidder = Keypair.generate(); // gasless: 0 SOL
  const ata = await getOrCreateAssociatedTokenAccount(conn, relayer, mint, bidder.publicKey);
  await mintTo(conn, relayer, mint, ata.address, relayer, amount);
  return { bidder, bidderToken: ata.address };
}

async function createAuction(conn, ctx) {
  const cfg = decodeConfig((await conn.getAccountInfo(configPda())).data);
  const id = cfg.auctionCount;
  const creator = Keypair.generate();
  const auction = auctionPda(id);
  const vault = vaultPda(auction);
  const sig = await sendIx(conn, ixCreateAuction({
    id, minBid: MIN_BID, durationSecs: 3600n,
    creator: creator.publicKey, payer: ctx.payer.publicKey, usdcMint: ctx.mint,
  }), ctx.payer, [creator], 'create_auction');
  console.log(`  auction #${id} created:`, sig.slice(0, 16) + '…');
  return { id, creator, auction, vault };
}

async function testPlaceBid(conn, lrpc, ctx, auc) {
  console.log('\n== place_bid (Light: proof via Photon, send via public) ==');
  const { bidder, bidderToken } = await fundedBidder(conn, ctx.payer, ctx.mint, BID_AMOUNT);
  console.log('  bidder:', bidder.publicKey.toBase58(), '(0 SOL, gasless)');

  const ch = contentHash('proposal-0: best meme of the week');
  const built = await buildPlaceBid(lrpc, { ...ctx, auction: auc.auction, vault: auc.vault }, bidder, bidderToken, 0n, ch, BID_AMOUNT);
  console.log('  proposal addr:', built.proposalAddress.toBase58());
  console.log('  bid addr:', built.bidAddress.toBase58());

  const sig = await sendIx(conn, [cuLimit(400_000), built.ix], ctx.payer, [bidder], 'place_bid');
  console.log('  place_bid sig:', sig);

  // read compressed accounts back via Photon (with indexing-lag retry)
  const pAcc = await waitCompressed(lrpc, built.proposalAddress);
  ok(pAcc !== null, 'ProposalTotal compressed account exists');
  const p = decodeProposalTotal(pAcc.data.data);
  ok(p.total === BID_AMOUNT, `ProposalTotal.total == ${BID_AMOUNT}`);
  ok(Buffer.from(p.contentHash).equals(ch), 'ProposalTotal.content_hash matches');

  const bAcc = await waitCompressed(lrpc, built.bidAddress);
  ok(bAcc !== null, 'Bid compressed account exists');
  const b = decodeBid(bAcc.data.data);
  ok(b.amount === BID_AMOUNT, `Bid.amount == ${BID_AMOUNT}`);
  ok(b.proposal === 0n, 'Bid.proposal == 0');

  // auction leader updated
  const a = decodeAuction((await conn.getAccountInfo(auc.auction)).data);
  ok(a.totalStaked === BID_AMOUNT, `auction.total_staked == ${BID_AMOUNT}`);
  ok(a.winnerProposal === 0n && a.winnerAmount === BID_AMOUNT, 'auction leader == proposal 0');
  ok(a.proposalCount === 1n, 'auction.proposal_count == 1');
}

async function main() {
  console.log('send RPC:', RPC_URL, '\nphoton  :', HELIUS_RPC.split('?')[0]);
  const conn = connection();
  const lrpc = lightRpc(HELIUS_RPC);
  const relayer = loadKeypair('./.relayer.json');
  const bal = await conn.getBalance(relayer.publicKey);
  console.log('relayer:', relayer.publicKey.toBase58(), '|', (bal / LAMPORTS_PER_SOL).toFixed(4), 'SOL\n');

  const { mint, cfg } = await setup(conn, relayer);
  const ctx = { payer: relayer, config: configPda(), mint };
  console.log('Config: fee', cfg.feeBps, '| auctions', cfg.auctionCount.toString());

  console.log('\n== create_auction (gasless) ==');
  const auc = await createAuction(conn, ctx);

  await testPlaceBid(conn, lrpc, ctx, auc);

  console.log('\nE2E OK (through place_bid)');
}

main().catch((e) => { console.error('\nE2E FAIL:', e); process.exit(1); });
