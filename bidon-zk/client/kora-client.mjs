// Minimal Kora JSON-RPC client for gasless submission. The user partially signs the
// transaction (their authority part); Kora adds the fee-payer signature and submits.
import { Transaction } from '@solana/web3.js';

export class KoraClient {
  constructor(url) { this.url = url; this._id = 0; }

  async call(method, params = {}) {
    const res = await fetch(this.url, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ jsonrpc: '2.0', id: String(++this._id), method, params }),
    });
    const j = await res.json();
    if (j.error) throw new Error(`Kora ${method} error: ${JSON.stringify(j.error)}`);
    return j.result;
  }

  getPayerSigner() { return this.call('getPayerSigner', {}); }
  estimateTransactionFee(txB64, feeToken) {
    return this.call('estimateTransactionFee', { transaction: txB64, fee_token: feeToken });
  }
  signAndSendTransaction(txB64, respondAfter = 'confirmed') {
    return this.call('signAndSendTransaction', { transaction: txB64, respond_after: respondAfter });
  }
}

// Build a tx, set Kora as fee payer, partial-sign with the user signers, hand to Kora.
export async function sendViaKora(conn, kora, koraPayer, ixs, userSigners, respondAfter = 'confirmed') {
  const tx = new Transaction().add(...(Array.isArray(ixs) ? ixs : [ixs]));
  tx.feePayer = koraPayer;
  tx.recentBlockhash = (await conn.getLatestBlockhash('confirmed')).blockhash;
  if (userSigners.length) tx.partialSign(...userSigners); // user authority part only
  const b64 = tx.serialize({ requireAllSignatures: false, verifySignatures: false }).toString('base64');
  const r = await kora.signAndSendTransaction(b64, respondAfter);
  return r.signature;
}
