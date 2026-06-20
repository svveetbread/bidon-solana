// Kora PoC: prove the bidon-zk gasless path works through a real Kora relayer on devnet.
// create_auction + place_bid are built with payer = Kora fee-payer pubkey; users (creator,
// bidder) sign only their authority part with 0 SOL; Kora adds the fee-payer signature and
// pays SOL (Auction/Vault rent + Light CPI fee + tx fee). Run: node kora-poc.mjs
import './load-env.mjs';
import { existsSync, readFileSync } from 'fs';
import { createHash } from 'crypto';
import { Connection, Keypair, PublicKey, LAMPORTS_PER_SOL } from '@solana/web3.js';
import { getOrCreateAssociatedTokenAccount, mintTo } from '@solana/spl-token';
import BN from 'bn.js';
import {
  HELIUS_RPC, configPda, auctionPda, vaultPda,
  ixCreateAuction, decodeConfig, decodeAuction, loadKeypair, cuLimit,
} from './lib.mjs';

// blockhash/funding RPC: public devnet (synced). Avoid the Helius .env node (it was stuck).
const PUBLIC_RPC = 'https://api.devnet.solana.com';
import { lightRpc, buildPlaceBid, decodeProposalTotal } from './light.mjs';
import { KoraClient, sendViaKora } from './kora-client.mjs';

const KORA_URL = process.env.KORA_URL || 'http://localhost:8080';
const MIN_BID = 100_000n, BID = 1_000_000n;
const ok = (c, m) => { if (!c) throw new Error('ASSERT FAILED: ' + m); console.log('  ✓', m); };
const contentHash = (t) => createHash('sha256').update(t).digest();
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
async function waitCompressed(lrpc, addrPk, tries = 15, delayMs = 4000) {
  for (let i = 0; i < tries; i++) {
    const acc = await lrpc.getCompressedAccount(new BN(addrPk.toBytes()));
    if (acc) return acc;
    await sleep(delayMs);
  }
  return null;
}

async function main() {
  console.log('Kora:', KORA_URL, '| send RPC:', PUBLIC_RPC, '| photon:', HELIUS_RPC.split('?')[0]);
  const conn = new Connection(PUBLIC_RPC, 'confirmed');
  const lrpc = lightRpc(HELIUS_RPC);
  const relayer = loadKeypair('./.relayer.json'); // only funds bidder USDC (setup), NOT used to sign Kora tx
  const kora = new KoraClient(KORA_URL);

  // 1. discover Kora fee payer
  const { signer_address, payment_address } = await kora.getPayerSigner();
  const koraPayer = new PublicKey(signer_address);
  console.log('Kora fee payer:', koraPayer.toBase58(), '| payment:', payment_address);
  ok(koraPayer.equals(relayer.publicKey), 'Kora fee payer == our relayer (8pXtJA)');

  const mint = new PublicKey(JSON.parse(readFileSync('./.state.json', 'utf8')).mint);
  const ctx = { payer: { publicKey: koraPayer }, config: configPda(), mint };

  // 2. create_auction via Kora — creator signs with 0 SOL, Kora pays rent
  const cfg = decodeConfig((await conn.getAccountInfo(configPda())).data);
  const id = cfg.auctionCount;
  const creator = Keypair.generate();
  const auction = auctionPda(id), vault = vaultPda(auction);
  console.log(`\n== create_auction #${id} via Kora (creator 0 SOL) ==`);
  const createIx = ixCreateAuction({ id, minBid: MIN_BID, durationSecs: 3600n, creator: creator.publicKey, payer: koraPayer, usdcMint: mint });
  const sig1 = await sendViaKora(conn, kora, koraPayer, [createIx], [creator]);
  console.log('  sig:', sig1);
  ok((await conn.getAccountInfo(auction)) !== null, 'Auction created (Kora paid rent)');
  ok(decodeAuction((await conn.getAccountInfo(auction)).data).rentPayer.equals(koraPayer), 'auction.rent_payer == Kora');

  // 3. place_bid via Kora — bidder signs with 0 SOL, Kora pays Light fee + tx fee
  console.log('\n== place_bid via Kora (bidder 0 SOL) ==');
  const bidder = Keypair.generate();
  const ata = (await getOrCreateAssociatedTokenAccount(conn, relayer, mint, bidder.publicKey)).address; // USDC funding = setup
  await mintTo(conn, relayer, mint, ata, relayer, BID);
  const built = await buildPlaceBid(lrpc, { ...ctx, auction, vault }, bidder, ata, 0n, contentHash('kora proposal'), BID);
  const bidderSol = await conn.getBalance(bidder.publicKey);
  const sig2 = await sendViaKora(conn, kora, koraPayer, [cuLimit(400_000), built.ix], [bidder]);
  console.log('  sig:', sig2);
  ok(bidderSol === 0, 'bidder had 0 SOL (gasless)');
  const pAcc = await waitCompressed(lrpc, built.proposalAddress);
  ok(pAcc !== null && decodeProposalTotal(pAcc.data.data).total === BID, `ProposalTotal.total == ${BID} (compressed, Kora-paid)`);

  console.log('\nKORA POC OK — create_auction + place_bid gasless via Kora relayer');
}

main().catch((e) => { console.error('\nKORA POC FAIL:', e); process.exit(1); });
