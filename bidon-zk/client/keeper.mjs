// keeper.mjs — авто-резолв завершённых аукционов: claim_winnings (победителю) + withdraw (каждому
// проигравшему) + close_auction. Операции permissionless; кипер СВОИМ ключом (#9, НЕ Kora-релеер)
// подписывает+платит SOL напрямую (без Kora-fee). Бидеры-лузеры берутся ON-CHAIN (Photon
// getCompressedAccountsByOwner) — НЕ из
// неаутентифицированной ленты стора (см. #13: убираем последнюю денежную зависимость от стора; лента
// теперь чисто UI). Идемпотентно: всё в try/catch, ретрай каждый цикл. Nullified (снятые) Bid не
// возвращаются enumeration'ом → перечисление даёт ровно ОТКРЫТЫЕ ставки (естественная идемпотентность).
//
// Запуск: node keeper.mjs [--once]   (--once = один проход и выход, для теста)
// Render: Web Service (Node), rootDir bidon-zk/client, startCommand `node keeper.mjs`.
//   Секреты: KEEPER_PRIVATE_KEY (base58 своего ключа кипера, #9), STORE_URL, HELIUS_RPC (withdraw-proof + enumeration).
import './load-env.mjs';
import BN from 'bn.js';
import { Keypair, PublicKey } from '@solana/web3.js';
import { getOrCreateAssociatedTokenAccount, getAssociatedTokenAddressSync } from '@solana/spl-token';
import bs58 from 'bs58';
import { createPrivateKey, sign as edSign } from 'crypto';
import {
  RPC_URL, HELIUS_RPC, PROGRAM_ID, configPda, auctionPda, vaultPda,
  ixClaimWinnings, ixCloseAuction, decodeConfig, decodeAuction,
  loadKeypair, connection, sendIx, cuLimit,
} from './lib.mjs';
import { lightRpc, buildWithdraw, bidAddress, decodeBid } from './light.mjs';
import { tgEnabled, tgSend, tgReply, tgGetUpdates } from './tg.mjs';

const STORE_URL = process.env.STORE_URL || 'http://127.0.0.1:8091';
const INTERVAL_MS = Number(process.env.KEEPER_INTERVAL_MS || 60_000);
const ONCE = process.argv.includes('--once');
const SITE_URL = process.env.SITE_URL || 'https://bidon.live';
// Kora-релеер (публичный ключ, НЕ секрет): следим за его SOL, т.к. на нём весь газлесс create/bid.
const KORA_PUBKEY = process.env.KORA_PUBKEY ? new PublicKey(process.env.KORA_PUBKEY) : null;
const LOW_SOL = Number(process.env.LOW_SOL_ALERT || 0.3); // порог алерта низкого баланса (SOL)

// #9: кипер использует СВОЙ отдельный ключ (газ keeper-tx + ed25519-подпись стора /resolved), НЕ
// Kora-релеер. Утечка ключа кипера больше НЕ даёт Kora-релеер / upgrade-authority. rent_recipient в
// close остаётся auction.rent_payer (=релеер), а не ключ кипера (см. resolveAuction). Fallback на
// KORA_PRIVATE_KEY / .keeper.json — для обратной совместимости / локального прогона.
let keeper;
if (process.env.KEEPER_PRIVATE_KEY) {
  keeper = Keypair.fromSecretKey(bs58.decode(process.env.KEEPER_PRIVATE_KEY));
} else if (process.env.KORA_PRIVATE_KEY) {
  keeper = Keypair.fromSecretKey(bs58.decode(process.env.KORA_PRIVATE_KEY));
} else {
  keeper = loadKeypair(process.env.KEEPER_KEYPAIR || './.keeper.json');
}

const conn = connection();
const lrpc = lightRpc(HELIUS_RPC);

// id-ы полностью отрезолвленных аукционов — пропускаем БЕЗ единого RPC (не гоняем keeper по всей истории).
const doneAuctions = new Set();
// transient (сеть/RPC/подтверждение) → ретраим. ВАЖНО: список широкий — обычные devnet-сбои
// (blockhash not found, block height exceeded, node is behind, not confirmed, 502/503) НЕ должны
// приниматься за «готово», иначе транзиентная ошибка бросает аук навсегда (см. баг с #53).
const isTransient = (m) =>
  /429|too many|rate.?limit|timeout|timed out|fetch failed|failed to fetch|econn|etimedout|eai_again|getaddrinfo|socket hang|network|blockhash not found|block ?height ?exceeded|node is behind|not confirmed|unable to confirm|expired|50[234]|gateway|service unavailable|connection (reset|closed|refused|terminated)/i.test(
    m || "",
  );

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
    key: Buffer.concat([PKCS8_ED, Buffer.from(keeper.secretKey.slice(0, 32))]),
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

// item.address приходит в разных формах (Uint8Array/Buffer/BN/PublicKey/number[]) — нормализуем
// к 32-байтному Buffer для сравнения с re-derived bidAddress. Иначе кросс-типовое сравнение молча врёт.
function toBuf32(v) {
  if (v == null) return null;
  if (Buffer.isBuffer(v)) return v.length === 32 ? v : null;
  if (v instanceof Uint8Array) return v.length === 32 ? Buffer.from(v) : null;
  if (v instanceof PublicKey) return Buffer.from(v.toBytes());
  if (v instanceof BN) { const b = v.toArrayLike(Buffer, 'be', 32); return b.length === 32 ? b : null; }
  if (Array.isArray(v)) return v.length === 32 ? Buffer.from(v) : null;
  if (typeof v?.toBytes === 'function') { const b = Buffer.from(v.toBytes()); return b.length === 32 ? b : null; }
  return null;
}

// addressTree константен — кэшируем один раз (используется в re-derive bidAddress для каждой ставки).
let _addressTree = null;
async function addressTree(rpc) {
  if (!_addressTree) _addressTree = (await rpc.getAddressTreeInfoV2()).tree;
  return _addressTree;
}

/**
 * Перечислить ВСЕ открытые (не-nullified) Bid-аккаунты программы ON-CHAIN через Photon.
 * getCompressedAccountsByOwner возвращает { items, cursor }; листаем по курсору до конца.
 * Оставляем только 48-байтовые data (Bid; 72=ProposalTotal, 0=прочее — скип), декодим decodeBid.
 * Бросает при сбое RPC → вызывающий трактует как transient (ретрай в след. тик, НЕ помечаем done).
 */
async function fetchProgramBids(rpc) {
  const out = [];
  let cursor;
  do {
    const page = await rpc.getCompressedAccountsByOwner(PROGRAM_ID, { cursor });
    const items = page?.items || [];
    for (const it of items) {
      const raw = it?.data?.data;
      if (!raw || raw.length !== 48) continue; // 48 = Bid; остальное (72=ProposalTotal, 0=прочее) — не наш случай
      const addr = toBuf32(it.address);
      if (!addr) continue; // адрес неожиданной формы/длины — пропускаем (не денежная потеря: withdraw permissionless)
      const bid = decodeBid(Buffer.from(raw));
      out.push({ address: addr, bidder: bid.bidder, pid: bid.proposal, amount: bid.amount });
    }
    cursor = page?.cursor ?? null; // null/undefined → страниц больше нет
  } while (cursor);
  return out;
}

/**
 * Уникальные (pid, bidder) ОТКРЫТЫЕ ставки конкретного аукциона — из предзагруженного allBids.
 * Bid принадлежит аукциону id, если re-derived bidAddress(tree, auctionPda(id), pid, bidder) совпадает
 * с адресом компресс-аккаунта (адрес встраивает auction+pid+bidder). Дедуп по `pid:bidder`.
 */
async function chainBidders(rpc, auctionId, allBids) {
  const tree = await addressTree(rpc);
  const auction = auctionPda(auctionId);
  const seen = new Set(), out = [];
  for (const b of allBids) {
    const derived = bidAddress(tree, auction, b.pid, b.bidder); // PublicKey
    if (!Buffer.from(derived.toBytes()).equals(b.address)) continue; // чужой аукцион
    const bidderB58 = b.bidder.toBase58();
    const k = `${b.pid}:${bidderB58}`;
    if (seen.has(k)) continue;
    seen.add(k);
    out.push({ pid: b.pid, bidder: bidderB58 });
  }
  return out;
}

async function resolveAuction(cfg, a, allBids) {
  const id = a.id;
  const auction = auctionPda(id);
  const vault = vaultPda(auction);
  const ctx = { config: configPda(), auction, vault, mint: cfg.usdcMint, payer: keeper };
  const winnerPids = new Set(
    a.winners.slice(0, a.winnersFilled).filter((w) => w.total > 0n).map((w) => w.proposalId.toString()),
  );
  // есть ли вообще победный пул (кому делать creator-выплату). Нет ставок → платить некому → шаг выполнен.
  const hasWinner = a.winners.slice(0, a.winnersFilled).some((w) => w.total > 0n);

  // 1) выплата победителю. Источник истины — on-chain creator_paid, НЕ текст ошибки.
  //    НИКОГДА не помечаем «выплачено» вслепую: при любом сбое перечитываем флаг с чейна
  //    (tx могла пройти, но отвалиться на подтверждении) — иначе транзиент бросает аук навсегда.
  let paid = a.creatorPaid === true || !hasWinner;
  if (!paid) {
    try {
      const creatorToken = (await getOrCreateAssociatedTokenAccount(conn, keeper, cfg.usdcMint, a.creator)).address;
      const feeReceiverToken = (await getOrCreateAssociatedTokenAccount(conn, keeper, cfg.usdcMint, cfg.feeReceiver)).address;
      await sendIx(conn, ixClaimWinnings({ id, vault, creatorToken, feeReceiverToken, usdcMint: cfg.usdcMint }), keeper, [], `claim`);
      console.log(`[#${id}] ✓ выплата победителю`);
      paid = true;
    } catch (e) {
      const fresh = await conn.getAccountInfo(auction).catch(() => null); // перечитать флаг с чейна
      paid = fresh ? decodeAuction(fresh.data).creatorPaid === true : false;
      if (!paid) console.log(`[#${id}] claim retry: ${e.message}`);
    }
  }

  // 2) возврат каждому проигравшему. Бидеры ON-CHAIN (Photon enumeration) — НЕ из ленты стора (#13):
  //    allBids уже перечислены в tick() ОДИН раз за проход; здесь фильтруем ставки этого аукциона.
  //    Nullified (уже снятые) Bid не возвращаются enumeration'ом → это ровно ещё-открытые ставки.
  const bidders = await chainBidders(lrpc, id, allBids);
  let allRefunded = true;
  let attempted = 0; // сколько лузеров реально пытались вернуть (для сверки с он-чейн-остатком волта)
  for (const { pid, bidder } of bidders) {
    if (winnerPids.has(pid.toString())) continue; // победный лот — не возвращаем (ушёл автору)
    attempted++;
    try {
      const bidderPk = new PublicKey(bidder);
      // АУДИТ N-1: НЕ создаём ATA лузеру здесь. Прежний getOrCreateAssociatedTokenAccount платил ренту
      // с релеера за КАЖДЫЙ адрес из НЕаутентифицированной ленты стора — атакующий инъектил фейковых
      // «лузеров» со своими pubkey (POST /bid без подписи) и сливал SOL релеера (возвращая ренту себе
      // закрытием ATA). Теперь только ВЫЧИСЛЯЕМ адрес: у легит-лузера ATA есть с момента ставки → возврат
      // проходит; для фейка buildWithdraw упадёт на отсутствующем Bid (ATA не создаётся, tx не шлётся).
      const bidderToken = getAssociatedTokenAddressSync(cfg.usdcMint, bidderPk);
      const wd = await buildWithdraw(lrpc, ctx, bidderPk, bidderToken, pid);
      await sendIx(conn, [cuLimit(400_000), wd.ix], keeper, [], `withdraw`);
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
      // rent_recipient строго = on-chain auction.rent_payer (=Kora-релеер, платил ренту в create),
      // а НЕ ключ кипера (#9) — иначе close_auction ревертит (constraint has_one/rent_payer).
      await sendIx(conn, ixCloseAuction({ id, rentRecipient: a.rentPayer }), keeper, [], `close`);
      console.log(`[#${id}] ✓ закрыт`);
    } catch (e) {
      /* ещё не готов (остались возвраты) — следующий цикл */
    }
  }

  // он-чейн-сверка перед done (H4/M2 аудита): пока в волте лежит USDC — расчёты НЕ закончены.
  // Schema 3: per-auction vault после расчёта держит ТОЛЬКО невыведенные ставки ЛУЗЕРОВ (депозит
  // создателя — в отдельном GLOBAL deposit vault, победный пул ушёл на claim). Источник лузеров теперь
  // сам чейн (authoritative), не feed → attempted==0 при vault>0 НЕ означает сбой стора:
  //   • перечислимые лузеры ещё остались (chainBidders дал >0, но withdraw был transient) → ретрай;
  //   • либо лузеры без ATA — withdraw упал (ATA нет), эти заберут сами (permissionless) → warn + done.
  let vaultRemaining = -1n;
  try {
    vaultRemaining = BigInt((await conn.getTokenAccountBalance(vault)).value.amount);
  } catch (e) {
    if (isTransient(e.message)) return { done: false }; // не смогли проверить баланс → перепроверим в след. тик
  }
  if (vaultRemaining > 0n) {
    // Пока chainBidders перечисляет ещё-открытые ставки (allRefunded=false от transient или просто есть
    // enumerable-лузеры на след. тик) — НЕ помечаем done: чейн говорит, что легит-лузеры ещё есть.
    if (bidders.some((x) => !winnerPids.has(x.pid.toString()))) {
      if (allRefunded) console.log(`[#${id}] vault=${vaultRemaining}, лузеры ещё перечислимы он-чейн → ретрай`);
    } else {
      // Перечислимых лузеров этого аукциона нет, а деньги есть → остаток у лузеров без ATA (withdraw не прошёл).
      console.warn(`[#${id}] ⚠ остаток ${vaultRemaining} в волте — лузеры без ATA; заберут сами (withdraw permissionless)`);
    }
  }

  // готов = победителю выплачено, легит-возвраты прошли, и vault проверен он-чейн → больше не трогаем
  return { done: paid && allRefunded };
}

// ───────────────────────── Telegram ops-хаб (#10, best-effort) ─────────────────────────
// Порядковый id → 5-символьный «код комнаты» (та же биекция, что фронт auctionCode.ts; НЕ секрет) —
// чтобы ссылки в алертах открывали /auction/<code>, а не сырой номер (который не резолвится).
const CODE_ALPHABET = '0123456789ABCDEFGHJKMNPQRSTVWXYZ';
const CODE_MASK = (1 << 25) - 1;
const CODE_MUL = 0x1d2a53 | 1;
const CODE_ADD = 0x0c0ffe & CODE_MASK;
function idToCode(id) {
  if (!Number.isFinite(id) || id < 0 || id > CODE_MASK) return '';
  let n = (((id & CODE_MASK) * CODE_MUL) + CODE_ADD) & CODE_MASK;
  let s = '';
  for (let i = 0; i < 5; i++) { s = CODE_ALPHABET[n & 31] + s; n >>>= 5; }
  return s;
}
const auctionUrl = (id) => `${SITE_URL}/auction/${idToCode(Number(id))}`;
const fmtSol = (lamports) => (lamports / 1e9).toFixed(3);
const fmtUsdc = (units) => (Number(units) / 1e6).toFixed(2);
const short = (pk) => { const s = pk.toBase58 ? pk.toBase58() : String(pk); return `${s.slice(0, 4)}…${s.slice(-4)}`; };

// состояние наблюдаемости (in-memory; на рестарте база = текущий count → без бэклог-спама «созданий»)
let lastAuctionCount = -1;
const balAlerted = { kora: false, keeper: false }; // edge-triggered: алерт при переходе healthy→low
let lastTickAt = 0;
let tickCounter = 0;
const startedAt = Date.now();

// новый аукцион (count вырос) → «создан кем-то» с деталями и ссылкой. Best-effort.
async function alertNewAuction(id) {
  try {
    const info = await conn.getAccountInfo(auctionPda(BigInt(id)));
    if (!info) return;
    const a = decodeAuction(info.data);
    const ends = new Date(Number(a.endTime) * 1000).toISOString().slice(0, 16).replace('T', ' ');
    await tgSend(
      `🆕 <b>Аукцион #${id} создан</b>\n` +
      `Создатель: <code>${a.creator.toBase58()}</code>\n` +
      `Мин. ставка: <b>${fmtUsdc(a.minBid)} USDC</b> · победителей: ${a.winnerCount} · schema ${a.schemaVersion}\n` +
      `Конец: ${ends} UTC\n${auctionUrl(id)}`,
    );
  } catch { /* best-effort */ }
}

// аукцион только что рассчитан кипером (свежий, <2ч после конца → без бэклог-спама).
async function alertResolved(a) {
  try {
    await tgSend(`✅ <b>Аукцион #${a.id} рассчитан</b> кипером.\n${auctionUrl(a.id)}`);
  } catch { /* best-effort */ }
}

// баланс SOL кипера + Kora-релеера; edge-triggered алерт при падении ниже порога и при восстановлении.
async function checkBalances() {
  const probe = async (key, pk, label) => {
    try {
      const bal = (await conn.getBalance(pk)) / 1e9;
      if (bal < LOW_SOL && !balAlerted[key]) {
        balAlerted[key] = true;
        await tgSend(`⚠️ <b>Низкий баланс SOL</b>\n${label}: <b>${bal.toFixed(3)} SOL</b> (порог ${LOW_SOL}). Пополни, иначе операции встанут.`);
      } else if (bal >= LOW_SOL && balAlerted[key]) {
        balAlerted[key] = false;
        await tgSend(`✅ Баланс восстановлен — ${label}: ${bal.toFixed(3)} SOL.`);
      }
    } catch { /* RPC-сбой — проверим на след. проходе */ }
  };
  await probe('keeper', keeper.publicKey, `кипер <code>${short(keeper.publicKey)}</code>`);
  if (KORA_PUBKEY) await probe('kora', KORA_PUBKEY, `Kora-релеер <code>${short(KORA_PUBKEY)}</code>`);
}

// сводка-хаб для /status.
async function statusText() {
  let count = '?', keepBal = '?', koraBal = '?';
  try { count = Number(decodeConfig((await conn.getAccountInfo(configPda())).data).auctionCount); } catch { /* rpc */ }
  try { keepBal = fmtSol(await conn.getBalance(keeper.publicKey)); } catch { /* rpc */ }
  if (KORA_PUBKEY) { try { koraBal = fmtSol(await conn.getBalance(KORA_PUBKEY)); } catch { /* rpc */ } }
  const pending = typeof count === 'number' ? count - doneAuctions.size : '?';
  const upMin = Math.round((Date.now() - startedAt) / 60000);
  const lastTick = lastTickAt ? `${new Date(lastTickAt).toISOString().slice(11, 19)} UTC` : '—';
  return [
    `📊 <b>bidon keeper</b> · devnet`,
    `Программа: <code>${PROGRAM_ID.toBase58()}</code>`,
    ``,
    `Аукционов всего: <b>${count}</b>`,
    `Отрезолвлено: ${doneAuctions.size} · ожидают/идут: ${pending}`,
    ``,
    `Кипер: <b>${keepBal} SOL</b> <code>${short(keeper.publicKey)}</code>`,
    `Kora-релеер: <b>${koraBal} SOL</b> ${KORA_PUBKEY ? `<code>${short(KORA_PUBKEY)}</code>` : '(KORA_PUBKEY не задан)'}`,
    ``,
    `Аптайм: ${upMin} мин · интервал ${INTERVAL_MS / 1000}s · посл. тик: ${lastTick}`,
    `🌐 ${SITE_URL}`,
  ].join('\n');
}

function helpText() {
  return [
    `🤖 <b>bidon ops-бот</b>`,
    `/status — сводка: аукционы, балансы Kora/кипера, аптайм`,
    `/help — эта справка`,
    ``,
    `Пуш-алерты: 🆕 новый аукцион · ⚠️ низкий баланс SOL · ✅ рассчитан.`,
  ].join('\n');
}

async function handleCommand(chatId, text) {
  const cmd = text.trim().split(/\s+/)[0].toLowerCase().replace(/@.*$/, '');
  if (cmd === '/status' || cmd === '/start') await tgReply(chatId, await statusText());
  else if (cmd === '/help') await tgReply(chatId, helpText());
  else await tgReply(chatId, 'Не знаю такой команды. /status или /help');
}

// long-poll команд (отдельный вечный цикл; НЕ блокирует tick). Отвечает ТОЛЬКО оператору (CHAT_ID).
async function runCommandLoop() {
  const CHAT_ID = process.env.TELEGRAM_CHAT_ID;
  // на старте слить бэклог (offset за последним апдейтом), чтобы не отвечать на старые сообщения.
  let { nextOffset } = await tgGetUpdates(0, 0).catch(() => ({ nextOffset: 0 }));
  for (;;) {
    try {
      const r = await tgGetUpdates(nextOffset, 25);
      nextOffset = r.nextOffset;
      for (const u of r.updates) {
        const m = u.message;
        if (!m || !m.text) continue;
        if (String(m.chat.id) !== String(CHAT_ID)) continue; // чужим не отвечаем и не палим инфу
        await handleCommand(m.chat.id, m.text);
      }
    } catch {
      await new Promise((res) => setTimeout(res, 3000)); // сбой long-poll → пауза и продолжаем
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
    // #10: новые аукционы (count вырос с прошлого прохода) → пуш-алерт «создан». Первый проход — только
    // фиксируем базу (без спама бэклогом); на рестарте база = текущий count. Гейт tgEnabled — без бота
    // не тратим лишний getAccountInfo.
    if (tgEnabled()) {
      if (lastAuctionCount < 0) {
        lastAuctionCount = count;
      } else if (count > lastAuctionCount) {
        for (let id = lastAuctionCount; id < count; id++) void alertNewAuction(id);
        lastAuctionCount = count;
      }
    }
    let processed = 0, active = 0;
    // Пасс 1: отбираем завершённые аукционы к резолву (дешёвые getAccountInfo, как раньше).
    const toResolve = [];
    for (let id = 0; id < count; id++) {
      if (doneAuctions.has(id)) continue; // уже отрезолвлен → ни одного RPC
      const info = await conn.getAccountInfo(auctionPda(BigInt(id)));
      if (!info) { markDone(id); continue; } // закрыт/не существует → done
      const a = decodeAuction(info.data);
      if (Number(a.endTime) > now) { active++; continue; } // ещё идёт — перепроверим в след. тик
      if (a.schemaVersion < 1 || a.schemaVersion > 3) { markDone(id); continue; } // legacy v0 (заморожен) → done; 1=топ-N, 2=антиснайп (N-2), 3=депозит
      toResolve.push(a);
    }
    // Пасс 2: если есть что резолвить — ОДИН раз перечисляем все Bid-аккаунты программы он-чейн
    // (не пере-сканируем per-auction). Сбой enumeration = transient → пропускаем тик, ничего не мечаем done.
    if (toResolve.length) {
      let allBids;
      try {
        allBids = await fetchProgramBids(lrpc);
      } catch (e) {
        console.error(`enumeration Bid'ов упала (${e.message}) → ретрай в след. тик`);
        return; // не резолвим без списка лузеров — иначе ложно пометили бы done
      }
      for (const a of toResolve) {
        const { done } = await resolveAuction(cfg, a, allBids);
        processed++;
        if (done) {
          markDone(a.id);
          if (now - Number(a.endTime) < 7200) void alertResolved(a); // алерт только про свежие (<2ч), не бэклог
        }
      }
    }
    console.log(`tick: к-резолву ${processed}, активных ${active}, пропущено ${doneAuctions.size}/${count}`);
    lastTickAt = Date.now();
    // балансы Kora/кипера — раз в ~12 тиков (мин при 5s), чтобы не долбить getBalance каждый проход.
    if (tgEnabled() && tickCounter++ % 12 === 0) void checkBalances();
  } catch (e) {
    console.error('tick error:', e.message);
  }
}

console.log(`bidon keeper · RPC=${RPC_URL} · store=${STORE_URL} · keeper=${keeper.publicKey.toBase58()} · tg=${tgEnabled() ? 'on' : 'off'} · ${ONCE ? 'once' : `loop ${INTERVAL_MS}ms`}`);
if (tgEnabled() && !ONCE) {
  void tgSend(`🚀 <b>keeper запущен</b> · devnet\nСлежу за проектом: 🆕 создания, ⚠️ балансы, ✅ расчёты. /status — сводка.`);
}
await loadResolved();
await tick();
if (!ONCE) {
  setInterval(() => void tick(), INTERVAL_MS);
  if (tgEnabled()) void runCommandLoop(); // команды оператора в отдельном вечном long-poll (не блокирует tick)
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
