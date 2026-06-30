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
import bs58 from 'bs58';
import { createPrivateKey, sign as edSign } from 'crypto';
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
  relayer = Keypair.fromSecretKey(bs58.decode(process.env.KORA_PRIVATE_KEY));
} else {
  relayer = loadKeypair(process.env.RELAYER_KEYPAIR || './.relayer.json');
}

const conn = connection();
const lrpc = lightRpc(HELIUS_RPC);

// id-ы полностью отрезолвленных аукционов — пропускаем БЕЗ единого RPC (не гоняем keeper по всей истории).
const doneAuctions = new Set();
// transient (429/сеть) → ретраим; всё прочее («уже сделано» / программный отказ) → считаем выполненным.
const isTransient = (m) =>
  /429|too many|timeout|timed out|fetch failed|econn|etimedout|rate limit|socket hang|getaddrinfo|network/i.test(m || "");

// Персистентный кэш отрезолвленных аукционов в сторе → keeper НЕ пересканирует историю после рестарта.
async function loadResolved() {
  try {
    const r = await fetch(`${STORE_URL}/resolved`);
    if (!r.ok) return;
    for (const id of await r.json()) doneAuctions.add(Number(id));
    console.log(`загружено отрезолвленных из стора: ${doneAuctions.size}`);
  } catch {
    /* стор недоступен — деградируем до in-memory */
  }
}
// ed25519-подпись через node:crypto (без внешних зависимостей; ed25519-seed = первые 32 байта secretKey).
const PKCS8_ED = Buffer.from("302e020100300506032b657004220420", "hex");
const signEd = (msgBytes) =>
  edSign(null, Buffer.from(msgBytes), createPrivateKey({
    key: Buffer.concat([PKCS8_ED, Buffer.from(relayer.secretKey.slice(0, 32))]),
    format: "der",
    type: "pkcs8",
  }));
async function markResolvedInStore(id) {
  try {
    const sig = bs58.encode(signEd(new TextEncoder().encode(`bidon-resolved:${id}`)));
    await fetch(`${STORE_URL}/resolved`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ auctionId: id.toString(), sig }),
    });
  } catch {
    /* best-effort — на след. тике/рестарте перезапишется */
  }
}
const markDone = (id) => {
  doneAuctions.add(id);
  void markResolvedInStore(id);
};

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

  // 1) выплата победителю — пропускаем, если creator_paid уже true (читаем с чейна)
  let paid = a.creatorPaid === true;
  if (!paid) {
    try {
      const creatorToken = (await getOrCreateAssociatedTokenAccount(conn, relayer, cfg.usdcMint, a.creator)).address;
      const feeReceiverToken = (await getOrCreateAssociatedTokenAccount(conn, relayer, cfg.usdcMint, cfg.feeReceiver)).address;
      await sendIx(conn, ixClaimWinnings({ id, vault, creatorToken, feeReceiverToken, usdcMint: cfg.usdcMint }), relayer, [], `claim`);
      console.log(`[#${id}] ✓ выплата победителю`);
      paid = true;
    } catch (e) {
      if (isTransient(e.message)) console.log(`[#${id}] claim retry: ${e.message}`);
      else paid = true; // already paid / программный отказ — считаем сделанным
    }
  }

  // 2) возврат каждому проигравшему (бидеры из ленты)
  const bidders = await feedBidders(id.toString());
  let allRefunded = true;
  for (const { pid, bidder } of bidders) {
    if (winnerPids.has(pid.toString())) continue; // победный лот — не возвращаем (ушёл автору)
    try {
      const bidderPk = new PublicKey(bidder);
      const bidderToken = (await getOrCreateAssociatedTokenAccount(conn, relayer, cfg.usdcMint, bidderPk)).address;
      const wd = await buildWithdraw(lrpc, ctx, bidderPk, bidderToken, pid);
      await sendIx(conn, [cuLimit(400_000), wd.ix], relayer, [], `withdraw`);
      console.log(`[#${id}] ✓ возврат pid${pid} ${bidder.slice(0, 8)}…`);
    } catch (e) {
      if (isTransient(e.message)) allRefunded = false; // ретрай в след. цикле; иначе уже возвращено / Bid нет
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

  // готов = победителю выплачено И легит-возвраты прошли без транзиентных ошибок → больше не трогаем
  return { done: paid && allRefunded };
}

async function tick() {
  try {
    const cfgInfo = await conn.getAccountInfo(configPda());
    if (!cfgInfo) return;
    const cfg = decodeConfig(cfgInfo.data);
    const now = Math.floor(Date.now() / 1000);
    const count = Number(cfg.auctionCount);
    let processed = 0, active = 0;
    for (let id = 0; id < count; id++) {
      if (doneAuctions.has(id)) continue; // уже отрезолвлен → ни одного RPC
      const info = await conn.getAccountInfo(auctionPda(BigInt(id)));
      if (!info) { markDone(id); continue; } // закрыт/не существует → done
      const a = decodeAuction(info.data);
      if (Number(a.endTime) > now) { active++; continue; } // ещё идёт — перепроверим в след. тик
      if (a.schemaVersion !== 1) { markDone(id); continue; } // legacy (заморожен) → done
      const { done } = await resolveAuction(cfg, a);
      processed++;
      if (done) markDone(id);
    }
    console.log(`tick: к-резолву ${processed}, активных ${active}, пропущено ${doneAuctions.size}/${count}`);
  } catch (e) {
    console.error('tick error:', e.message);
  }
}

console.log(`bidon keeper · RPC=${RPC_URL} · store=${STORE_URL} · relayer=${relayer.publicKey.toBase58()} · ${ONCE ? 'once' : `loop ${INTERVAL_MS}ms`}`);
await loadResolved();
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
