// bidon-zk client core: manual instruction building (no anchor IDL — IDL-build is broken
// on this toolchain). Anchor discriminator = sha256("global:<ix>")[0..8] + borsh args.
// Light-compressed instructions (place_bid/raise_bid/top_up_bid/withdraw) live in light.mjs.
import { createHash } from 'crypto';
import { readFileSync } from 'fs';
import {
  Connection, Keypair, PublicKey, Transaction, TransactionInstruction,
  SystemProgram, sendAndConfirmTransaction, ComputeBudgetProgram,
} from '@solana/web3.js';
import { TOKEN_PROGRAM_ID } from '@solana/spl-token';

// ---- constants ----
export const PROGRAM_ID = new PublicKey('4Pfc1jdDXX4EMFoe7FxNGMfQmSgZSegJn7DCHkxbnfXz');
export const RPC_URL = process.env.SOLANA_DEVNET_RPC || 'https://api.devnet.solana.com';

const CONFIG_SEED = Buffer.from('config');
const AUCTION_SEED = Buffer.from('auction');
const VAULT_SEED = Buffer.from('vault');

// ---- anchor discriminator ----
export function disc(name) {
  return createHash('sha256').update(`global:${name}`).digest().subarray(0, 8);
}

// ---- borsh primitive encoders ----
export const u8 = (n) => { const b = Buffer.alloc(1); b.writeUInt8(Number(n)); return b; };
export const u16 = (n) => { const b = Buffer.alloc(2); b.writeUInt16LE(Number(n)); return b; };
export const u64 = (n) => { const b = Buffer.alloc(8); b.writeBigUInt64LE(BigInt(n)); return b; };
export const i64 = (n) => { const b = Buffer.alloc(8); b.writeBigInt64LE(BigInt(n)); return b; };
export const pk = (p) => (p instanceof PublicKey ? p : new PublicKey(p)).toBuffer();
export const bytes32 = (arr) => {
  const b = Buffer.from(arr);
  if (b.length !== 32) throw new Error(`expected 32 bytes, got ${b.length}`);
  return b;
};

// ---- PDAs ----
export const configPda = () => PublicKey.findProgramAddressSync([CONFIG_SEED], PROGRAM_ID)[0];
export const auctionPda = (id) =>
  PublicKey.findProgramAddressSync([AUCTION_SEED, u64(id)], PROGRAM_ID)[0];
export const vaultPda = (auction) =>
  PublicKey.findProgramAddressSync([VAULT_SEED, auction.toBuffer()], PROGRAM_ID)[0];

// ---- account meta helper ----
const m = (pubkey, isSigner, isWritable) => ({ pubkey, isSigner, isWritable });

// ---- non-Light instruction builders (accounts mirror lib.rs Accounts structs) ----

// initialize(fee_bps: u16, fee_receiver: Pubkey, usdc_mint: Pubkey)
export function ixInitialize({ owner, feeBps, feeReceiver, usdcMint }) {
  const config = configPda();
  const data = Buffer.concat([disc('initialize'), u16(feeBps), pk(feeReceiver), pk(usdcMint)]);
  return new TransactionInstruction({
    programId: PROGRAM_ID,
    keys: [m(config, false, true), m(owner, true, true), m(SystemProgram.programId, false, false)],
    data,
  });
}

// set_config(fee_bps: u16, fee_receiver: Pubkey)
export function ixSetConfig({ owner, feeBps, feeReceiver }) {
  const config = configPda();
  const data = Buffer.concat([disc('set_config'), u16(feeBps), pk(feeReceiver)]);
  return new TransactionInstruction({
    programId: PROGRAM_ID,
    keys: [m(config, false, true), m(owner, true, false)],
    data,
  });
}

// create_auction(id: u64, min_bid: u64, duration_secs: i64)
export function ixCreateAuction({ id, minBid, durationSecs, creator, payer, usdcMint }) {
  const config = configPda();
  const auction = auctionPda(id);
  const vault = vaultPda(auction);
  const data = Buffer.concat([disc('create_auction'), u64(id), u64(minBid), i64(durationSecs)]);
  return new TransactionInstruction({
    programId: PROGRAM_ID,
    keys: [
      m(config, false, true),
      m(auction, false, true),
      m(usdcMint, false, false),
      m(vault, false, true),
      m(creator, true, false),
      m(payer, true, true),
      m(TOKEN_PROGRAM_ID, false, false),
      m(SystemProgram.programId, false, false),
    ],
    data,
  });
}

// claim_winnings() — permissionless
export function ixClaimWinnings({ id, vault, creatorToken, feeReceiverToken, usdcMint }) {
  const config = configPda();
  const auction = auctionPda(id);
  const data = disc('claim_winnings');
  return new TransactionInstruction({
    programId: PROGRAM_ID,
    keys: [
      m(config, false, false),
      m(auction, false, true),
      m(vault, false, true),
      m(creatorToken, false, true),
      m(feeReceiverToken, false, true),
      m(usdcMint, false, false),
      m(TOKEN_PROGRAM_ID, false, false),
    ],
    data,
  });
}

// close_auction() — permissionless GC
export function ixCloseAuction({ id, rentRecipient }) {
  const auction = auctionPda(id);
  const vault = vaultPda(auction);
  const data = disc('close_auction');
  return new TransactionInstruction({
    programId: PROGRAM_ID,
    keys: [
      m(auction, false, true),
      m(vault, false, true),
      m(rentRecipient, false, true),
      m(TOKEN_PROGRAM_ID, false, false),
    ],
    data,
  });
}

// ---- account decoders ----
// Config: owner(32) fee_bps(2) fee_receiver(32) usdc_mint(32) auction_count(8) bump(1), after 8-disc
export function decodeConfig(buf) {
  let o = 8;
  const owner = new PublicKey(buf.subarray(o, o + 32)); o += 32;
  const feeBps = buf.readUInt16LE(o); o += 2;
  const feeReceiver = new PublicKey(buf.subarray(o, o + 32)); o += 32;
  const usdcMint = new PublicKey(buf.subarray(o, o + 32)); o += 32;
  const auctionCount = buf.readBigUInt64LE(o); o += 8;
  const bump = buf.readUInt8(o);
  return { owner, feeBps, feeReceiver, usdcMint, auctionCount, bump };
}

// Auction: id(8) creator(32) min_bid(8) fee_bps(2) end_time(8 i64) creator_paid(1)
//   total_staked(8) proposal_count(8) winner_proposal(8) winner_amount(8) rent_payer(32) bump(1)
export function decodeAuction(buf) {
  let o = 8;
  const id = buf.readBigUInt64LE(o); o += 8;
  const creator = new PublicKey(buf.subarray(o, o + 32)); o += 32;
  const minBid = buf.readBigUInt64LE(o); o += 8;
  const feeBps = buf.readUInt16LE(o); o += 2;
  const endTime = buf.readBigInt64LE(o); o += 8;
  const creatorPaid = buf.readUInt8(o) === 1; o += 1;
  const totalStaked = buf.readBigUInt64LE(o); o += 8;
  const proposalCount = buf.readBigUInt64LE(o); o += 8;
  const winnerProposal = buf.readBigUInt64LE(o); o += 8;
  const winnerAmount = buf.readBigUInt64LE(o); o += 8;
  const rentPayer = new PublicKey(buf.subarray(o, o + 32)); o += 32;
  const bump = buf.readUInt8(o);
  return {
    id, creator, minBid, feeBps, endTime, creatorPaid, totalStaked,
    proposalCount, winnerProposal, winnerAmount, rentPayer, bump,
  };
}

// ---- io / rpc helpers ----
export function loadKeypair(path) {
  return Keypair.fromSecretKey(Uint8Array.from(JSON.parse(readFileSync(path, 'utf8'))));
}
export function connection() {
  return new Connection(RPC_URL, 'confirmed');
}
export const cuLimit = (n) => ComputeBudgetProgram.setComputeUnitLimit({ units: n });

export async function sendIx(conn, ixs, payer, signers, label = 'tx') {
  const tx = new Transaction().add(...(Array.isArray(ixs) ? ixs : [ixs]));
  const sig = await sendAndConfirmTransaction(conn, tx, [payer, ...signers], {
    commitment: 'confirmed', skipPreflight: false,
  });
  return sig;
}
