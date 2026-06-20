// bidon-zk devnet e2e. Grows step by step.
//  - non-Light: setup (mint + Config), create_auction
//  - Light: place_bid (proof via Helius Photon, tx sent over public RPC)
import './load-env.mjs'; // must be first — loads .env before lib.mjs reads process.env
import { existsSync, readFileSync, writeFileSync } from 'fs';
import { createHash } from 'crypto';
import { Keypair, PublicKey, LAMPORTS_PER_SOL } from '@solana/web3.js';
import { createMint, getOrCreateAssociatedTokenAccount, mintTo } from '@solana/spl-token';
import BN from 'bn.js';
import {
  PROGRAM_ID, RPC_URL, HELIUS_RPC, configPda, auctionPda, vaultPda,
  ixInitialize, ixCreateAuction, ixClaimWinnings, ixCloseAuction, decodeConfig, decodeAuction,
  loadKeypair, connection, sendIx, cuLimit,
} from './lib.mjs';
import { lightRpc, buildPlaceBid, buildRaiseBid, buildTopUpBid, buildWithdraw, decodeProposalTotal, decodeBid } from './light.mjs';

const STATE = './.state.json';
const loadState = () => (existsSync(STATE) ? JSON.parse(readFileSync(STATE, 'utf8')) : {});
const saveState = (s) => writeFileSync(STATE, JSON.stringify(s, null, 2));

const FEE_BPS = 370;
const MIN_BID = 100_000n;        // 0.1 USDC
const BID_AMOUNT = 1_000_000n;   // 1 USDC (b1 places proposal 0)
const RAISE_AMOUNT = 500_000n;   // 0.5 USDC (b2 backs proposal 0)
const TOPUP_AMOUNT = 300_000n;   // 0.3 USDC (b1 tops up)

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

async function createAuction(conn, ctx, durationSecs = 3600n) {
  const cfg = decodeConfig((await conn.getAccountInfo(configPda())).data);
  const id = cfg.auctionCount;
  const creator = Keypair.generate();
  const auction = auctionPda(id);
  const vault = vaultPda(auction);
  const sig = await sendIx(conn, ixCreateAuction({
    id, minBid: MIN_BID, durationSecs,
    creator: creator.publicKey, payer: ctx.payer.publicKey, usdcMint: ctx.mint,
  }), ctx.payer, [creator], 'create_auction');
  console.log(`  auction #${id} created:`, sig.slice(0, 16) + '…');
  return { id, creator, auction, vault };
}

async function tokenBalance(conn, ata) {
  const r = await conn.getTokenAccountBalance(ata);
  return BigInt(r.value.amount);
}

async function testPlaceBid(conn, lrpc, ctx, auc) {
  console.log('\n== place_bid (Light: proof via Photon, send via public) ==');
  const { bidder, bidderToken } = await fundedBidder(conn, ctx.payer, ctx.mint, BID_AMOUNT + TOPUP_AMOUNT);
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
  return { bidder, bidderToken, proposalAddress: built.proposalAddress };
}

async function testRaiseBid(conn, lrpc, ctx, auc) {
  console.log('\n== raise_bid (new backer on proposal 0) ==');
  const { bidder, bidderToken } = await fundedBidder(conn, ctx.payer, ctx.mint, RAISE_AMOUNT);
  console.log('  backer:', bidder.publicKey.toBase58(), '(0 SOL)');
  const built = await buildRaiseBid(lrpc, { ...ctx, auction: auc.auction, vault: auc.vault }, bidder, bidderToken, 0n, RAISE_AMOUNT);
  const sig = await sendIx(conn, [cuLimit(400_000), built.ix], ctx.payer, [bidder], 'raise_bid');
  console.log('  raise sig:', sig.slice(0, 16) + '…');

  const expected = BID_AMOUNT + RAISE_AMOUNT;
  const pAcc = await waitCompressed(lrpc, built.proposalAddress);
  ok(decodeProposalTotal(pAcc.data.data).total === expected, `ProposalTotal.total == ${expected}`);
  const bAcc = await waitCompressed(lrpc, built.bidAddress);
  ok(decodeBid(bAcc.data.data).amount === RAISE_AMOUNT, `new Bid.amount == ${RAISE_AMOUNT}`);
  const a = decodeAuction((await conn.getAccountInfo(auc.auction)).data);
  ok(a.totalStaked === expected, `auction.total_staked == ${expected}`);
  ok(a.winnerAmount === expected, `winner_amount == ${expected}`);
  return { bidder, bidderToken };
}

async function testTopUpBid(conn, lrpc, ctx, auc, b1) {
  console.log('\n== top_up_bid (b1 tops up own Bid) ==');
  const built = await buildTopUpBid(lrpc, { ...ctx, auction: auc.auction, vault: auc.vault }, b1.bidder, b1.bidderToken, 0n, TOPUP_AMOUNT);
  const sig = await sendIx(conn, [cuLimit(400_000), built.ix], ctx.payer, [b1.bidder], 'top_up_bid');
  console.log('  top_up sig:', sig.slice(0, 16) + '…');

  const expectedTotal = BID_AMOUNT + RAISE_AMOUNT + TOPUP_AMOUNT;
  const expectedBid = BID_AMOUNT + TOPUP_AMOUNT;
  const pAcc = await waitCompressed(lrpc, built.proposalAddress);
  ok(decodeProposalTotal(pAcc.data.data).total === expectedTotal, `ProposalTotal.total == ${expectedTotal}`);
  const bAcc = await waitCompressed(lrpc, built.bidAddress);
  ok(decodeBid(bAcc.data.data).amount === expectedBid, `b1 Bid.amount == ${expectedBid}`);
  const a = decodeAuction((await conn.getAccountInfo(auc.auction)).data);
  ok(a.totalStaked === expectedTotal, `auction.total_staked == ${expectedTotal}`);
}

// Full settle lifecycle on a short auction: 2 proposals (winner + loser), wait past
// end_time, claim_winnings, withdraw (loser), close_auction. Checks Σin == Σout.
async function testSettle(conn, lrpc, ctx) {
  console.log('\n== settle: short auction → claim + withdraw + close ==');
  const WIN = 1_000_000n, LOSE = 200_000n;
  const auc = await createAuction(conn, ctx, 16n);

  const bw = await fundedBidder(conn, ctx.payer, ctx.mint, WIN);
  const wB = await buildPlaceBid(lrpc, { ...ctx, auction: auc.auction, vault: auc.vault }, bw.bidder, bw.bidderToken, 0n, contentHash('winner'), WIN);
  await sendIx(conn, [cuLimit(400_000), wB.ix], ctx.payer, [bw.bidder], 'place winner');
  console.log('  proposal 0 (winner) placed:', WIN.toString());

  const bl = await fundedBidder(conn, ctx.payer, ctx.mint, LOSE);
  const lB = await buildPlaceBid(lrpc, { ...ctx, auction: auc.auction, vault: auc.vault }, bl.bidder, bl.bidderToken, 1n, contentHash('loser'), LOSE);
  await sendIx(conn, [cuLimit(400_000), lB.ix], ctx.payer, [bl.bidder], 'place loser');
  console.log('  proposal 1 (loser) placed:', LOSE.toString());

  ok(await tokenBalance(conn, auc.vault) === WIN + LOSE, `vault holds Σin == ${WIN + LOSE}`);

  console.log('  waiting ~18s for end_time…');
  await sleep(18000);

  // claim_winnings: creator gets winner pool minus fee; fee_receiver gets fee
  const creatorToken = (await getOrCreateAssociatedTokenAccount(conn, ctx.payer, ctx.mint, auc.creator.publicKey)).address;
  const feeReceiverToken = (await getOrCreateAssociatedTokenAccount(conn, ctx.payer, ctx.mint, ctx.payer.publicKey)).address;
  const feeBefore = await tokenBalance(conn, feeReceiverToken);
  await sendIx(conn, ixClaimWinnings({ id: auc.id, vault: auc.vault, creatorToken, feeReceiverToken, usdcMint: ctx.mint }), ctx.payer, [], 'claim');
  const fee = (WIN * BigInt(FEE_BPS)) / 10_000n;
  const payout = WIN - fee;
  ok(await tokenBalance(conn, creatorToken) === payout, `creator got winner−fee == ${payout}`);
  ok((await tokenBalance(conn, feeReceiverToken)) - feeBefore === fee, `fee_receiver got fee == ${fee}`);

  // withdraw: loser reclaims stake + compressed Bid closed (permissionless, relayer pays)
  const wd = await buildWithdraw(lrpc, { ...ctx, auction: auc.auction, vault: auc.vault }, bl.bidder.publicKey, bl.bidderToken, 1n);
  await sendIx(conn, [cuLimit(400_000), wd.ix], ctx.payer, [], 'withdraw');
  ok(await tokenBalance(conn, bl.bidderToken) === LOSE, `loser reclaimed stake == ${LOSE}`);
  ok(await tokenBalance(conn, auc.vault) === 0n, 'vault drained to 0 (Σin == Σout)');

  // close_auction: vault + Auction closed, rent → relayer
  await sendIx(conn, ixCloseAuction({ id: auc.id, rentRecipient: ctx.payer.publicKey }), ctx.payer, [], 'close');
  ok((await conn.getAccountInfo(auc.auction)) === null, 'Auction account closed');
  ok((await conn.getAccountInfo(auc.vault)) === null, 'vault account closed');
}

// Concurrency: two backers build raise against the SAME proposal state. The first
// lands and nullifies the proposal hash; the second carries a now-stale proof and is
// rejected on-chain; a client retry with a fresh proof succeeds. Validates the
// nullifier model (contention = latency, not a cap).
async function testConcurrency(conn, lrpc, ctx) {
  console.log('\n== concurrency: stale-proof nullifier → client retry ==');
  const auc = await createAuction(conn, ctx, 3600n);
  const actx = { ...ctx, auction: auc.auction, vault: auc.vault };

  const b0 = await fundedBidder(conn, ctx.payer, ctx.mint, 1_000_000n);
  const p0 = await buildPlaceBid(lrpc, actx, b0.bidder, b0.bidderToken, 0n, contentHash('hot proposal'), 1_000_000n);
  await sendIx(conn, [cuLimit(400_000), p0.ix], ctx.payer, [b0.bidder], 'place');
  await waitCompressed(lrpc, p0.proposalAddress);

  // both built against the same current proposal state
  const ba = await fundedBidder(conn, ctx.payer, ctx.mint, 300_000n);
  const bb = await fundedBidder(conn, ctx.payer, ctx.mint, 300_000n);
  const builtA = await buildRaiseBid(lrpc, actx, ba.bidder, ba.bidderToken, 0n, 300_000n);
  const builtB = await buildRaiseBid(lrpc, actx, bb.bidder, bb.bidderToken, 0n, 300_000n);

  await sendIx(conn, [cuLimit(400_000), builtA.ix], ctx.payer, [ba.bidder], 'raise A');
  console.log('  raise A landed (nullifies proposal hash)');

  let rejected = false;
  try {
    await sendIx(conn, [cuLimit(400_000), builtB.ix], ctx.payer, [bb.bidder], 'raise B (stale)');
  } catch (e) {
    rejected = true;
    console.log('  raise B stale-proof rejected on-chain (expected):', (e.transactionMessage || e.message || '').slice(0, 70));
  }
  ok(rejected, 'concurrent raise with stale proof rejected on-chain');

  await waitCompressed(lrpc, builtA.bidAddress); // let A index so B sees fresh state
  const retryB = await buildRaiseBid(lrpc, actx, bb.bidder, bb.bidderToken, 0n, 300_000n);
  await sendIx(conn, [cuLimit(400_000), retryB.ix], ctx.payer, [bb.bidder], 'raise B retry');
  console.log('  raise B retry with fresh proof landed');

  const pAcc = await waitCompressed(lrpc, p0.proposalAddress);
  ok(decodeProposalTotal(pAcc.data.data).total === 1_600_000n, 'ProposalTotal.total == 1.6M (1M + A 0.3M + B 0.3M)');
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

  const only = process.argv[2]; // e.g. "concurrency" to run just that scenario
  if (only === 'concurrency') {
    await testConcurrency(conn, lrpc, ctx);
  } else {
    console.log('\n== create_auction (gasless) ==');
    const auc = await createAuction(conn, ctx);
    const b1 = await testPlaceBid(conn, lrpc, ctx, auc);
    await testRaiseBid(conn, lrpc, ctx, auc);
    await testTopUpBid(conn, lrpc, ctx, auc, b1);
    await testSettle(conn, lrpc, ctx);
    await testConcurrency(conn, lrpc, ctx);
  }

  console.log('\nE2E OK — place/raise/top_up + claim/withdraw/close (Σin==Σout) + concurrency-retry');
}

main().catch((e) => { console.error('\nE2E FAIL:', e); process.exit(1); });
