// Probe whether Helius Photon serves a fresh validity proof, even though the RPC
// getSlot node is stuck. If proof comes back with a usable root, Light bids are
// possible (proof from Helius Photon + tx send via public RPC).
import './load-env.mjs';
import { createRpc, getDefaultAddressTreeInfo, deriveAddressSeedV2, deriveAddressV2 } from '@lightprotocol/stateless.js';
import { Keypair, PublicKey } from '@solana/web3.js';
import BN from 'bn.js';
import { HELIUS_RPC as HELIUS } from './lib.mjs';

const PROGRAM_ID = new PublicKey('4Pfc1jdDXX4EMFoe7FxNGMfQmSgZSegJn7DCHkxbnfXz');

async function main() {
  const rpc = createRpc(HELIUS, HELIUS, 'https://prover.helius.dev');
  const addr = getDefaultAddressTreeInfo();
  console.log('getDefaultAddressTreeInfo tree:', addr.tree.toBase58(), 'queue:', addr.queue.toBase58());
  const v2 = await rpc.getAddressTreeInfoV2();
  console.log('getAddressTreeInfoV2     tree:', v2.tree.toBase58(), 'queue:', v2.queue.toBase58(), 'type:', v2.treeType);
  const sti = await rpc.getStateTreeInfos();
  console.log('stateTreeInfos[0] tree:', sti[0].tree.toBase58(), 'queue:', sti[0].queue.toBase58(), 'type:', sti[0].treeType);

  // a valid (field-element) fresh address via V2 derivation
  const seed = deriveAddressSeedV2([Buffer.from('probe'), Keypair.generate().publicKey.toBuffer()]);
  const randomAddr = deriveAddressV2(seed, addr.tree, PROGRAM_ID);
  console.log('probing getValidityProofV0 for a fresh address...');
  const t0 = Date.now();
  const proof = await rpc.getValidityProofV0([], [
    { address: new BN(randomAddr.toBytes()), tree: addr.tree, queue: addr.queue },
  ]);
  console.log('Photon responded in', Date.now() - t0, 'ms');
  console.log('  rootIndices:', proof.rootIndices);
  console.log('  roots:', (proof.roots || []).map((r) => r.toString()).slice(0, 2));
  console.log('  hasCompressedProof:', !!proof.compressedProof);
  console.log('PHOTON OK');
}

main().catch((e) => { console.error('PHOTON FAIL:', e.message || e); process.exit(1); });
