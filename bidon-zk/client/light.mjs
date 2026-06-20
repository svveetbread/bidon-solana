// Light-compressed instructions for bidon-zk: place_bid / raise_bid / top_up_bid / withdraw.
// Proof + compressed-account reads come from a Photon RPC (Helius); the resulting
// instruction is sent over any RPC. Packing mirrors the official create-and-update TS
// example: featureFlags V2 + newWithSystemAccountsV2 + insertOrGet indices used directly.
// Borsh layouts mirror light-sdk Rust structs:
//   ValidityProof = Option<CompressedProof{a[32],b[64],c[32]}>
//   PackedAddressTreeInfo = u8 mt_index, u8 queue_index, u16 root_index
//   PackedStateTreeInfo   = u16 root_index, bool prove_by_index, u8 mt, u8 queue, u32 leaf
//   CompressedAccountMeta = PackedStateTreeInfo + address[32] + u8 output_index
import BN from 'bn.js';
import { PublicKey, TransactionInstruction } from '@solana/web3.js';
import {
  createRpc, deriveAddressSeedV2, deriveAddressV2, featureFlags, VERSION,
  PackedAccounts, SystemAccountMetaConfig, selectStateTreeInfo, TreeType,
} from '@lightprotocol/stateless.js';
import { TOKEN_PROGRAM_ID } from '@solana/spl-token';
import { PROGRAM_ID, disc, u8, u16, u32, u64, boolByte, bytes32 } from './lib.mjs';

featureFlags.version = VERSION.V2; // V2 system-account layout (matches our V2 program)

const PROPOSAL_SEED = Buffer.from('proposal');
const BID_SEED = Buffer.from('bid');
const m = (pubkey, isSigner, isWritable) => ({ pubkey, isSigner, isWritable });

export function lightRpc(url, prover = 'https://prover.helius.dev') {
  return createRpc(url, url, prover);
}

// ---- Light struct borsh encoders ----
export function encValidityProof(cp) {
  if (!cp) return Buffer.from([0]);
  return Buffer.concat([Buffer.from([1]), Buffer.from(cp.a), Buffer.from(cp.b), Buffer.from(cp.c)]);
}
const encAddrTreeInfo = (t) =>
  Buffer.concat([u8(t.addressMerkleTreePubkeyIndex), u8(t.addressQueuePubkeyIndex), u16(t.rootIndex)]);
const encStateTreeInfo = (t) =>
  Buffer.concat([u16(t.rootIndex), boolByte(t.proveByIndex), u8(t.merkleTreePubkeyIndex), u8(t.queuePubkeyIndex), u32(t.leafIndex)]);
const encMeta = (meta) =>
  Buffer.concat([encStateTreeInfo(meta.treeInfo), bytes32(meta.address), u8(meta.outputStateTreeIndex)]);

// ---- address derivation (mirrors derive_address in lib.rs) ----
export function proposalAddress(addressTree, auction, pid) {
  return deriveAddressV2(deriveAddressSeedV2([PROPOSAL_SEED, auction.toBuffer(), u64(pid)]), addressTree, PROGRAM_ID);
}
export function bidAddress(addressTree, auction, pid, bidder) {
  return deriveAddressV2(deriveAddressSeedV2([BID_SEED, auction.toBuffer(), u64(pid), bidder.toBuffer()]), addressTree, PROGRAM_ID);
}

// ---- compressed-account decoders (raw struct data, no 8-byte disc) ----
export function decodeProposalTotal(buf) {
  return { creator: new PublicKey(buf.subarray(0, 32)), contentHash: Buffer.from(buf.subarray(32, 64)), total: buf.readBigUInt64LE(64) };
}
export function decodeBid(buf) {
  return { bidder: new PublicKey(buf.subarray(0, 32)), proposal: buf.readBigUInt64LE(32), amount: buf.readBigUInt64LE(40) };
}

// ---- shared named accounts for place/raise/top_up (PlaceBid/RaiseBid layout) ----
function bidAccounts(ctx, bidder, bidderToken) {
  return [
    m(ctx.config, false, false),
    m(ctx.auction, false, true),
    m(ctx.vault, false, true),
    m(ctx.mint, false, false),
    m(bidderToken, false, true),
    m(bidder, true, false),
    m(ctx.payer.publicKey, true, true),
    m(TOKEN_PROGRAM_ID, false, false),
  ];
}

async function v2StateTree(rpc) {
  return selectStateTreeInfo(await rpc.getStateTreeInfos(), TreeType.StateV2);
}

// place_bid: new proposal — create ProposalTotal + Bid under one combined proof (2 new addresses).
export async function buildPlaceBid(rpc, ctx, bidder, bidderToken, pid, contentHash, amount) {
  const at = await rpc.getAddressTreeInfoV2();
  const pAddr = proposalAddress(at.tree, ctx.auction, pid);
  const bAddr = bidAddress(at.tree, ctx.auction, pid, bidder.publicKey);

  const proof = await rpc.getValidityProofV0([], [
    { tree: at.tree, queue: at.queue, address: new BN(pAddr.toBytes()) },
    { tree: at.tree, queue: at.queue, address: new BN(bAddr.toBytes()) },
  ]);

  const stInfo = await v2StateTree(rpc);
  const packed = PackedAccounts.newWithSystemAccountsV2(SystemAccountMetaConfig.new(PROGRAM_ID));
  const outputStateTreeIndex = packed.insertOrGet(stInfo.queue);
  const addrQueueIndex = packed.insertOrGet(at.queue);
  const addrTreeIndex = packed.insertOrGet(at.tree);
  const remainingAccounts = packed.toAccountMetas().remainingAccounts;

  const data = Buffer.concat([
    disc('place_bid'),
    encValidityProof(proof.compressedProof),
    encAddrTreeInfo({ addressMerkleTreePubkeyIndex: addrTreeIndex, addressQueuePubkeyIndex: addrQueueIndex, rootIndex: proof.rootIndices[0] }),
    encAddrTreeInfo({ addressMerkleTreePubkeyIndex: addrTreeIndex, addressQueuePubkeyIndex: addrQueueIndex, rootIndex: proof.rootIndices[1] }),
    u8(outputStateTreeIndex),
    bytes32(contentHash),
    u64(amount),
  ]);
  const ix = new TransactionInstruction({
    programId: PROGRAM_ID,
    keys: [...bidAccounts(ctx, bidder.publicKey, bidderToken), ...remainingAccounts],
    data,
  });
  return { ix, proposalAddress: pAddr, bidAddress: bAddr };
}

// raise_bid: existing proposal, new backer — update ProposalTotal (input) + create Bid (new address).
export async function buildRaiseBid(rpc, ctx, bidder, bidderToken, pid, amount) {
  const at = await rpc.getAddressTreeInfoV2();
  const pAddr = proposalAddress(at.tree, ctx.auction, pid);
  const bAddr = bidAddress(at.tree, ctx.auction, pid, bidder.publicKey);
  const pAcc = await rpc.getCompressedAccount(new BN(pAddr.toBytes()));
  const pState = decodeProposalTotal(pAcc.data.data);

  const proof = await rpc.getValidityProofV0(
    [{ hash: pAcc.hash, tree: pAcc.treeInfo.tree, queue: pAcc.treeInfo.queue }],
    [{ tree: at.tree, queue: at.queue, address: new BN(bAddr.toBytes()) }],
  );

  const packed = PackedAccounts.newWithSystemAccountsV2(SystemAccountMetaConfig.new(PROGRAM_ID));
  const proposalMeta = {
    treeInfo: {
      rootIndex: proof.rootIndices[0], proveByIndex: proof.proveByIndices[0],
      merkleTreePubkeyIndex: packed.insertOrGet(pAcc.treeInfo.tree),
      queuePubkeyIndex: packed.insertOrGet(pAcc.treeInfo.queue),
      leafIndex: pAcc.leafIndex,
    },
    address: pAcc.address, outputStateTreeIndex: packed.insertOrGet(pAcc.treeInfo.queue),
  };
  const addrQueueIndex = packed.insertOrGet(at.queue);
  const addrTreeIndex = packed.insertOrGet(at.tree);
  const remainingAccounts = packed.toAccountMetas().remainingAccounts;

  const data = Buffer.concat([
    disc('raise_bid'),
    encValidityProof(proof.compressedProof),
    u64(pid),
    encMeta(proposalMeta),
    Buffer.from(pState.creator.toBytes()),
    bytes32(pState.contentHash),
    u64(pState.total),
    encAddrTreeInfo({ addressMerkleTreePubkeyIndex: addrTreeIndex, addressQueuePubkeyIndex: addrQueueIndex, rootIndex: proof.rootIndices[1] }),
    u8(proposalMeta.outputStateTreeIndex),
    u64(amount),
  ]);
  const ix = new TransactionInstruction({
    programId: PROGRAM_ID,
    keys: [...bidAccounts(ctx, bidder.publicKey, bidderToken), ...remainingAccounts],
    data,
  });
  return { ix, bidAddress: bAddr, proposalAddress: pAddr, proposalTotalBefore: pState.total };
}

// top_up_bid: own existing Bid — update both ProposalTotal and Bid (two inclusions).
export async function buildTopUpBid(rpc, ctx, bidder, bidderToken, pid, amount) {
  const at = await rpc.getAddressTreeInfoV2();
  const pAddr = proposalAddress(at.tree, ctx.auction, pid);
  const bAddr = bidAddress(at.tree, ctx.auction, pid, bidder.publicKey);
  const pAcc = await rpc.getCompressedAccount(new BN(pAddr.toBytes()));
  const bAcc = await rpc.getCompressedAccount(new BN(bAddr.toBytes()));
  const pState = decodeProposalTotal(pAcc.data.data);
  const bState = decodeBid(bAcc.data.data);

  const proof = await rpc.getValidityProofV0([
    { hash: pAcc.hash, tree: pAcc.treeInfo.tree, queue: pAcc.treeInfo.queue },
    { hash: bAcc.hash, tree: bAcc.treeInfo.tree, queue: bAcc.treeInfo.queue },
  ], []);

  const packed = PackedAccounts.newWithSystemAccountsV2(SystemAccountMetaConfig.new(PROGRAM_ID));
  const proposalMeta = {
    treeInfo: {
      rootIndex: proof.rootIndices[0], proveByIndex: proof.proveByIndices[0],
      merkleTreePubkeyIndex: packed.insertOrGet(pAcc.treeInfo.tree),
      queuePubkeyIndex: packed.insertOrGet(pAcc.treeInfo.queue),
      leafIndex: pAcc.leafIndex,
    },
    address: pAcc.address, outputStateTreeIndex: packed.insertOrGet(pAcc.treeInfo.queue),
  };
  const bidMeta = {
    treeInfo: {
      rootIndex: proof.rootIndices[1], proveByIndex: proof.proveByIndices[1],
      merkleTreePubkeyIndex: packed.insertOrGet(bAcc.treeInfo.tree),
      queuePubkeyIndex: packed.insertOrGet(bAcc.treeInfo.queue),
      leafIndex: bAcc.leafIndex,
    },
    address: bAcc.address, outputStateTreeIndex: packed.insertOrGet(bAcc.treeInfo.queue),
  };
  const remainingAccounts = packed.toAccountMetas().remainingAccounts;

  const data = Buffer.concat([
    disc('top_up_bid'),
    encValidityProof(proof.compressedProof),
    u64(pid),
    encMeta(proposalMeta),
    Buffer.from(pState.creator.toBytes()),
    bytes32(pState.contentHash),
    u64(pState.total),
    encMeta(bidMeta),
    u64(bState.amount),
    u64(amount),
  ]);
  const ix = new TransactionInstruction({
    programId: PROGRAM_ID,
    keys: [...bidAccounts(ctx, bidder.publicKey, bidderToken), ...remainingAccounts],
    data,
  });
  return { ix, proposalAddress: pAddr, bidAddress: bAddr };
}

// withdraw: after end_time, losing bidder reclaims stake + close compressed Bid. Permissionless.
export async function buildWithdraw(rpc, ctx, bidderPubkey, bidderToken, pid) {
  const at = await rpc.getAddressTreeInfoV2();
  const bAddr = bidAddress(at.tree, ctx.auction, pid, bidderPubkey);
  const bAcc = await rpc.getCompressedAccount(new BN(bAddr.toBytes()));
  const bState = decodeBid(bAcc.data.data);

  const proof = await rpc.getValidityProofV0(
    [{ hash: bAcc.hash, tree: bAcc.treeInfo.tree, queue: bAcc.treeInfo.queue }], [],
  );
  const packed = PackedAccounts.newWithSystemAccountsV2(SystemAccountMetaConfig.new(PROGRAM_ID));
  const bidMeta = {
    treeInfo: {
      rootIndex: proof.rootIndices[0], proveByIndex: proof.proveByIndices[0],
      merkleTreePubkeyIndex: packed.insertOrGet(bAcc.treeInfo.tree),
      queuePubkeyIndex: packed.insertOrGet(bAcc.treeInfo.queue),
      leafIndex: bAcc.leafIndex,
    },
    address: bAcc.address, outputStateTreeIndex: packed.insertOrGet(bAcc.treeInfo.queue),
  };
  const remainingAccounts = packed.toAccountMetas().remainingAccounts;

  const data = Buffer.concat([
    disc('withdraw'),
    encValidityProof(proof.compressedProof),
    u64(pid),
    Buffer.from(bidderPubkey.toBytes()),
    encMeta(bidMeta),
    u64(bState.amount),
  ]);
  const keys = [
    m(ctx.config, false, false),
    m(ctx.auction, false, false),
    m(ctx.vault, false, true),
    m(bidderToken, false, true),
    m(ctx.mint, false, false),
    m(ctx.payer.publicKey, true, true),
    m(TOKEN_PROGRAM_ID, false, false),
    ...remainingAccounts,
  ];
  const ix = new TransactionInstruction({ programId: PROGRAM_ID, keys, data });
  return { ix, bidAmount: bState.amount };
}
