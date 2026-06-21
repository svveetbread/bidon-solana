// keeper.mjs — авто-резолв завершённых аукционов: claim_winnings (победителю) + withdraw (каждому
// проигравшему) + close_auction. Операции permissionless; релейер подписывает+платит SOL напрямую
// (без Kora-fee). Бидеры берутся из ленты bidon-store. Идемпотентно: всё в try/catch, ретрай каждый цикл.
//
// Запуск: node keeper.mjs [--once]   (--once = один проход и выход, для теста)
// Render: Web Service (Node), rootDir bidon-zk/client, startCommand `node keeper.mjs`.
//   Секреты: KORA_PRIVATE_KEY (base58 релейер), STORE_URL, HELIUS_RPC (для withdraw-proof).
import './load-env.mjs';
import { Keypair, PublicKey } from '@solana/web3.js';
import { getOrCreateAssociatedTokenAccount } from '@solana/spl-token';
import {
  RPC_URL, HELIUS_RPC, configPda, auctionPda, vaultPda,
  ixClaimWinnings, ixCloseAuction, decodeConfig, decodeAuction,
  loadKeypair, connection, sendIx, cuLimit,
} from './lib.mjs';
import { lightRpc, buildWithdraw } from './light.mjs';

const STORE_URL = process.env.STORE_URL || 'http://127.0.0.1:8091';
const INTERVAL_MS = Number(process.env.KEEPER_INTERVAL_MS || 60_000);
const ONCE = process.argv.includes('--once');

let relayer;
if (process.env.KORA_PRIVATE_KEY) {
  const bs58 = (await import('bs58')).default;
  relayer = Keypair.fromSecretKey(bs58.decode(process.env.KORA_PRIVATE_KEY));
} else {
  relayer = loadKeypair(process.env.RELAYER_KEYPAIR || './.relayer.json');
}

const conn = connection();
const lrpc = lightRpc(HELIUS_RPC);

/** Уникальные (proposalId, bidder) ставки аукциона из персистентной ленты стора. */
async function feedBidders(auctionId) {
  try {
    const r = await fetch(`${STORE_URL}/feed?auctionId=${auctionId}`);
    if (!r.ok) return [];
    const feed = await r.json();
    const seen = new Set(), out = [];
    for (const e of feed) {
      if (!e || !e.proposalId || !e.bidder) continue;
      const k = `${e.proposalId}:${e.bidder}`;
      if (seen.has(k)) continue;
      seen.add(k);
      out.push({ pid: BigInt(e.proposalId), bidder: e.bidder });
    }
    return out;
  } catch {
    return [];
  }
}

async function resolveAuction(cfg, a) {
  const id = a.id;
  const auction = auctionPda(id);
  const vault = vaultPda(auction);
  const ctx = { config: configPda(), auction, vault, mint: cfg.usdcMint, payer: relayer };
  const winnerPids = new Set(
    a.winners.slice(0, a.winnersFilled).filter((w) => w.total > 0n).map((w) => w.proposalId.toString()),
  );

  // 1) выплата победителю (idempotent — повторный вызов упадёт, ловим)
  try {
    const creatorToken = (await getOrCreateAssociatedTokenAccount(conn, relayer, cfg.usdcMint, a.creator)).address;
    const feeReceiverToken = (await getOrCreateAssociatedTokenAccount(conn, relayer, cfg.usdcMint, cfg.feeReceiver)).address;
    await sendIx(conn, ixClaimWinnings({ id, vault, creatorToken, feeReceiverToken, usdcMint: cfg.usdcMint }), relayer, [], `claim`);
    console.log(`[#${id}] ✓ выплата победителю`);
  } catch (e) {
    if (!/already|insufficient|0x|custom/i.test(e.message || '')) console.log(`[#${id}] claim skip: ${e.message}`);
  }

  // 2) возврат каждому проигравшему (бидеры из ленты)
  const bidders = await feedBidders(id.toString());
  for (const { pid, bidder } of bidders) {
    if (winnerPids.has(pid.toString())) continue; // победный лот — не возвращаем (ушёл автору)
    try {
      const bidderPk = new PublicKey(bidder);
      const bidderToken = (await getOrCreateAssociatedTokenAccount(conn, relayer, cfg.usdcMint, bidderPk)).address;
      const wd = await buildWithdraw(lrpc, ctx, bidderPk, bidderToken, pid);
      await sendIx(conn, [cuLimit(400_000), wd.ix], relayer, [], `withdraw`);
      console.log(`[#${id}] ✓ возврат pid${pid} ${bidder.slice(0, 8)}…`);
    } catch (e) {
      // ставка уже возвращена / Bid не найден / гонка proof — пропускаем, ретрай в след. цикле
    }
  }

  // 3) закрытие — ПО УМОЛЧАНИЮ НЕ закрываем: close_auction убирает аккаунт с чейна → аукцион исчезает
  //    из списка («Аукцион не найден»). Для демо/UX аукцион должен оставаться видимым как «завершён».
  //    Рента релейера остаётся залочена до close. Включить чистку: KEEPER_CLOSE=true (для прод-масштаба).
  if (process.env.KEEPER_CLOSE === 'true') {
    try {
      await sendIx(conn, ixCloseAuction({ id, rentRecipient: relayer.publicKey }), relayer, [], `close`);
      console.log(`[#${id}] ✓ закрыт`);
    } catch (e) {
      /* ещё не готов (остались возвраты) — следующий цикл */
    }
  }
}

async function tick() {
  try {
    const cfgInfo = await conn.getAccountInfo(configPda());
    if (!cfgInfo) return;
    const cfg = decodeConfig(cfgInfo.data);
    const now = Math.floor(Date.now() / 1000);
    const count = Number(cfg.auctionCount);
    let processed = 0;
    for (let id = 0; id < count; id++) {
      const info = await conn.getAccountInfo(auctionPda(BigInt(id)));
      if (!info) continue; // закрыт/не существует
      const a = decodeAuction(info.data);
      if (Number(a.endTime) > now) continue; // ещё идёт
      if (a.schemaVersion !== 1) continue; // старый (заморожен, до winner_count) — гейты fail-closed
      await resolveAuction(cfg, a);
      processed++;
    }
    console.log(`tick: обработано ${processed} завершённых (из ${count})`);
  } catch (e) {
    console.error('tick error:', e.message);
  }
}

console.log(`bidon keeper · RPC=${RPC_URL} · store=${STORE_URL} · relayer=${relayer.publicKey.toBase58()} · ${ONCE ? 'once' : `loop ${INTERVAL_MS}ms`}`);
await tick();
if (!ONCE) {
  setInterval(() => void tick(), INTERVAL_MS);
  // HTTP для Render (web service нужен порт) + keep-alive: GET /health, GET /resolve (триггер прохода).
  // Render даёт $PORT; cron-job.org пингует /health раз в 14 мин → сервис не спит → loop крутится.
  if (process.env.PORT) {
    const http = await import('node:http');
    http
      .createServer((req, res) => {
        if (req.url === '/resolve') void tick();
        res.writeHead(200, { 'Content-Type': 'application/json' });
        res.end(req.url === '/resolve' ? '{"triggered":true}' : '{"ok":true}');
      })
      .listen(Number(process.env.PORT), '0.0.0.0', () => console.log(`keeper health on :${process.env.PORT}`));
  }
}
