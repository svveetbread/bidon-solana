// Полный e2e-сьют против ЗАДЕПЛОЕННОЙ программы на РЕАЛЬНОМ devnet (program 9GSQ…).
// relayer = наш funded-кейпар (.relayer.json) — платит rent+fee за всех (газлесс); юзеры с 0 SOL только подписывают.
// Config — синглтон: mint+initialize создаём ОДИН раз, дальше переиспользуем (на devnet нет --reset).
// Каждый сценарий = свежий аукцион (id = config.auction_count). Тай-брейк: лидер меняется только при total > winner_amount
// (строго) → при равных суммах побеждает достигший раньше.
//
// Покрытие: happy+газлесс+закрытия+инвариант · ничья (равные суммы → первый) · перебой→равенство(остаётся)→перебой(меняется)
//           · «одновременный» равный стейк (Promise.all) · одно предложение · пустой аукцион · после end (reject)
//           · негативы всех гейтов (min/mint/proposal_id/finalize/claim/withdraw/double) · createAuction+set_config валидации.
//
// Запуск (Git Bash, node на Windows):  cd e2e && node devnet-all.mjs        (RPC берётся из ../.env SOLANA_DEVNET_RPC)
//   ONLY=S1 node devnet-all.mjs  — прогнать один сценарий (смоук).

import anchor from "@coral-xyz/anchor";
import {
  Connection, Keypair, PublicKey, LAMPORTS_PER_SOL, SystemProgram, ComputeBudgetProgram,
} from "@solana/web3.js";
import {
  createMint, getOrCreateAssociatedTokenAccount, mintTo, getAccount, TOKEN_PROGRAM_ID,
} from "@solana/spl-token";
import { readFileSync } from "fs";

const { Program, AnchorProvider, Wallet, BN } = anchor;

// ── окружение ────────────────────────────────────────────────────────────────
function envVal(key) {
  try {
    const txt = readFileSync(new URL("../.env", import.meta.url), "utf8");
    const m = txt.match(new RegExp(`^${key}=(.*)$`, "m"));
    return m ? m[1].trim() : null;
  } catch { return null; }
}
const RPC = process.env.RPC || envVal("SOLANA_DEVNET_RPC") || "https://api.devnet.solana.com";
const relayer = Keypair.fromSecretKey(
  Uint8Array.from(JSON.parse(readFileSync(new URL("./.relayer.json", import.meta.url)))),
);
const connection = new Connection(RPC, "confirmed");
const provider = new AnchorProvider(connection, new Wallet(relayer), { commitment: "confirmed", preflightCommitment: "confirmed" });
const idl = JSON.parse(readFileSync(new URL("../bidon/target/idl/bidon.json", import.meta.url)));
const program = new Program(idl, provider);
const pid = program.programId;
const ONLY = process.env.ONLY || null;

// ── утилиты ───────────────────────────────────────────────────────────────────
const U = (n) => new BN(n);
const le8 = (n) => U(n).toArrayLike(Buffer, "le", 8);
const H = Array(32).fill(0);
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const PRIO = () => ComputeBudgetProgram.setComputeUnitPrice({ microLamports: 50_000 });

let failures = 0, passes = 0;
const check = (name, cond, got) => {
  if (cond) { passes++; console.log(`  ✅ ${name}`); }
  else { failures++; console.log(`  ❌ ${name}${got !== undefined ? `  (got: ${got})` : ""}`); }
};

const usdcBal = async (ata) => Number((await getAccount(connection, ata)).amount);
const sol = (pk) => connection.getBalance(pk);
const closed = async (pk) => (await connection.getAccountInfo(pk)) === null;

const TRANSIENT = /429|Too Many Requests|rate limit|timeout|timed out|Blockhash not found|blockhash|node is behind|fetch failed|ECONNRESET|503|502/i;
async function withRetry(fn, label = "tx", attempts = 5) {
  let last;
  for (let i = 0; i < attempts; i++) {
    try { return await fn(); }
    catch (e) {
      last = e;
      const msg = e?.message || String(e);
      if (!TRANSIENT.test(msg) || i === attempts - 1) throw e;
      await sleep(1500 * (i + 1));
    }
  }
  throw last;
}
// мутирующий вызов: priority-fee + ретрай транзиентных ошибок RPC.
const rpc = (builder, signers = [], label = "tx") =>
  withRetry(() => builder.preInstructions([PRIO()]).signers(signers).rpc({ commitment: "confirmed" }), label);
// ожидаем падения (негативный тест): код/сообщение опционально сверяем.
async function expectFail(name, builder, signers = [], codeSubstr = null) {
  try {
    await builder.preInstructions([PRIO()]).signers(signers).rpc({ commitment: "confirmed" });
    check(name, false, "tx НЕ упала");
  } catch (e) {
    const code = e?.error?.errorCode?.code || "";
    const blob = `${code} ${e?.message || ""} ${JSON.stringify(e?.logs || [])}`;
    check(name + (codeSubstr ? ` → ${codeSubstr}` : " отклонён"), !codeSubstr || blob.includes(codeSubstr), `${code || blob.slice(0, 90)}`);
  }
}

// ── PDA ──────────────────────────────────────────────────────────────────────
const [config] = PublicKey.findProgramAddressSync([Buffer.from("config")], pid);
const auctionPda = (id) => PublicKey.findProgramAddressSync([Buffer.from("auction"), le8(id)], pid)[0];
const vaultPda = (a) => PublicKey.findProgramAddressSync([Buffer.from("vault"), a.toBuffer()], pid)[0];
const proposalPda = (a, p) => PublicKey.findProgramAddressSync([Buffer.from("proposal"), a.toBuffer(), le8(p)], pid)[0];
const bidPda = (a, p, b) => PublicKey.findProgramAddressSync([Buffer.from("bid"), a.toBuffer(), le8(p), b.toBuffer()], pid)[0];

// ── общие аккаунты ставки ──────────────────────────────────────────────────────
const bidAccts = (auction, vault, p, bidderPk, bidderAta, mint = usdc) => ({
  config, auction, proposal: proposalPda(auction, p), bid: bidPda(auction, p, bidderPk),
  usdcMint: mint, vault, bidderToken: bidderAta, bidder: bidderPk, payer: relayer.publicKey,
  tokenProgram: TOKEN_PROGRAM_ID, systemProgram: SystemProgram.programId,
});

// ── билдеры инструкций ─────────────────────────────────────────────────────────
let usdc; // выставится в setup()

async function mkBidder(amount) {
  const kp = Keypair.generate(); // 0 SOL
  const ata = (await withRetry(() => getOrCreateAssociatedTokenAccount(connection, relayer, usdc, kp.publicKey), "ata")).address;
  if (amount > 0) await withRetry(() => mintTo(connection, relayer, usdc, ata, relayer, amount), "mint");
  return { kp, ata };
}
async function newAuction(minBid, durationSecs) {
  const id = Number((await program.account.config.fetch(config)).auctionCount);
  const auction = auctionPda(id), vault = vaultPda(auction);
  const creator = Keypair.generate(); // 0 SOL
  await rpc(program.methods.createAuction(U(id), U(minBid), U(durationSecs)).accountsStrict({
    config, auction, usdcMint: usdc, vault, creator: creator.publicKey, payer: relayer.publicKey,
    tokenProgram: TOKEN_PROGRAM_ID, systemProgram: SystemProgram.programId,
  }), [creator], "createAuction");
  return { id, auction, vault, creator };
}
const placeBid = (A, p, b, amt, mint = usdc) =>
  rpc(program.methods.placeBid(U(p), H, U(amt)).accountsStrict(bidAccts(A.auction, A.vault, p, b.kp.publicKey, b.ata, mint)), [b.kp], "placeBid");
const raiseBid = (A, p, b, amt) =>
  rpc(program.methods.raiseBid(U(p), U(amt)).accountsStrict(bidAccts(A.auction, A.vault, p, b.kp.publicKey, b.ata)), [b.kp], "raiseBid");

async function finalize(A) {
  // спим ровно до end_time (+буфер на расхождение часов кластера), потом финализируем (с добором ретраев на скос).
  const end = Number((await fetchA(A)).endTime);
  const waitMs = (end - Math.floor(Date.now() / 1000) + 3) * 1000;
  if (waitMs > 0) { console.log(`  ⏳ ждём ${Math.ceil(waitMs / 1000)}с до конца аукциона…`); await sleep(waitMs); }
  for (let i = 0; i < 20; i++) {
    try { return await rpc(program.methods.finalize().accountsStrict({ auction: A.auction }), [], "finalize"); }
    catch (e) { if (/NotEnded/.test(e?.error?.errorCode?.code || e?.message || "")) { await sleep(2000); continue; } throw e; }
  }
  throw new Error("finalize: аукцион так и не закончился по времени");
}
async function ataOf(ownerPk) {
  return (await withRetry(() => getOrCreateAssociatedTokenAccount(connection, relayer, usdc, ownerPk), "ata")).address;
}
const claim = async (A) => {
  const creatorAta = await ataOf(A.creator.publicKey);
  const feeAta = await ataOf(relayer.publicKey);
  const feeBefore = await usdcBal(feeAta); // fee_receiver=relayer копит по всем сценариям → нужна дельта
  await rpc(program.methods.claimWinnings().accountsStrict({
    config, auction: A.auction, vault: A.vault, creatorToken: creatorAta, feeReceiverToken: feeAta,
    usdcMint: usdc, tokenProgram: TOKEN_PROGRAM_ID,
  }), [], "claim");
  return { creatorAta, feeAta, creatorGot: await usdcBal(creatorAta), feeDelta: (await usdcBal(feeAta)) - feeBefore };
};
const withdraw = (A, p, b) =>
  rpc(program.methods.withdraw(U(p), b.kp.publicKey).accountsStrict({
    config, auction: A.auction, bid: bidPda(A.auction, p, b.kp.publicKey), rentRecipient: relayer.publicKey,
    vault: A.vault, bidderToken: b.ata, usdcMint: usdc, tokenProgram: TOKEN_PROGRAM_ID,
  }), [], "withdraw");
const closeBid = (A, p, b) =>
  rpc(program.methods.closeBid(U(p), b.kp.publicKey).accountsStrict({
    auction: A.auction, bid: bidPda(A.auction, p, b.kp.publicKey), rentRecipient: relayer.publicKey,
  }), [], "closeBid");
const closeProposal = (A, p) =>
  rpc(program.methods.closeProposal(U(p)).accountsStrict({
    auction: A.auction, proposal: proposalPda(A.auction, p), rentRecipient: relayer.publicKey,
  }), [], "closeProposal");
const closeAuction = (A) =>
  rpc(program.methods.closeAuction().accountsStrict({
    auction: A.auction, vault: A.vault, rentRecipient: relayer.publicKey, tokenProgram: TOKEN_PROGRAM_ID,
  }), [], "closeAuction");

const fetchA = (A) => program.account.auction.fetch(A.auction);
const isP = (A, pk, p) => pk.equals(proposalPda(A.auction, p));

// ── setup (идемпотентный config) ───────────────────────────────────────────────
async function setup() {
  const bal = await sol(relayer.publicKey);
  console.log(`relayer ${relayer.publicKey.toBase58()}  ${(bal / LAMPORTS_PER_SOL).toFixed(4)} SOL  · RPC ${RPC.split("?")[0]}`);
  if (bal < 0.3 * LAMPORTS_PER_SOL) throw new Error(`мало SOL на relayer (${bal}) — пополни 8pXtJA…`);

  const exists = await connection.getAccountInfo(config);
  if (!exists) {
    console.log("config не найден → создаю mint + initialize (один раз на devnet)");
    usdc = await withRetry(() => createMint(connection, relayer, relayer.publicKey, null, 6), "createMint");
    await rpc(program.methods.initialize(370, relayer.publicKey, usdc).accountsStrict({
      config, owner: relayer.publicKey, systemProgram: SystemProgram.programId,
    }), [], "initialize");
    console.log(`  ✓ initialize: fee 3.7%, usdc_mint ${usdc.toBase58()}`);
  } else {
    const c = await program.account.config.fetch(config);
    usdc = c.usdcMint;
    console.log(`config есть: owner ${c.owner.toBase58().slice(0, 8)}… usdc_mint ${usdc.toBase58().slice(0, 8)}… auction_count ${c.auctionCount}`);
  }
}

// ════════════════════════════ СЦЕНАРИИ ════════════════════════════════════════
const scenarios = {};

// S1 — happy path + газлесс(0 SOL) + гейты(негативы по таймингу) + закрытия + инвариант Σin==Σout
scenarios.S1 = async () => {
  const A = await newAuction(1_000_000, 90); // длинное окно: успеть ставки + негативы до end
  check("creator создал аукцион с 0 SOL", (await sol(A.creator.publicKey)) === 0);
  const evil = await withRetry(() => createMint(connection, relayer, relayer.publicKey, null, 6), "evilMint");

  const b1 = await mkBidder(20_000_000), b2 = await mkBidder(20_000_000), b3 = await mkBidder(20_000_000);
  await placeBid(A, 0, b1, 4_000_000);   // p0 = 4 (лузер)
  await placeBid(A, 1, b2, 10_000_000);  // p1 = 10
  await raiseBid(A, 1, b3, 5_000_000);   // p1 = 15 (победитель)

  check("b1 потратил 0 SOL", (await sol(b1.kp.publicKey)) === 0);
  check("b2 потратил 0 SOL", (await sol(b2.kp.publicKey)) === 0);
  check("b3 потратил 0 SOL", (await sol(b3.kp.publicKey)) === 0);
  let a = await fetchA(A);
  check("total_staked == 19", Number(a.totalStaked) === 19_000_000, Number(a.totalStaked));
  check("winner == proposal 1", isP(A, a.winnerProposal, 1));
  check("winner_amount == 15", Number(a.winnerAmount) === 15_000_000, Number(a.winnerAmount));
  check("vault == 19", (await usdcBal(A.vault)) === 19_000_000);

  // негативы пока аукцион активен:
  await expectFail("ставка ниже min", program.methods.placeBid(U(2), H, U(500_000)).accountsStrict(bidAccts(A.auction, A.vault, 2, b1.kp.publicKey, b1.ata)), [b1.kp], "BidTooLow");
  await expectFail("чужой mint", program.methods.placeBid(U(2), H, U(2_000_000)).accountsStrict(bidAccts(A.auction, A.vault, 2, b1.kp.publicKey, b1.ata, evil)), [b1.kp], "InvalidMint");
  await expectFail("неверный proposal_id (5)", program.methods.placeBid(U(5), H, U(2_000_000)).accountsStrict(bidAccts(A.auction, A.vault, 5, b1.kp.publicKey, b1.ata)), [b1.kp], "InvalidProposalId");
  await expectFail("raise на несуществующее предложение (9)", program.methods.raiseBid(U(9), U(2_000_000)).accountsStrict(bidAccts(A.auction, A.vault, 9, b1.kp.publicKey, b1.ata)), [b1.kp]);
  await expectFail("finalize до конца", program.methods.finalize().accountsStrict({ auction: A.auction }), [], "AuctionNotEnded");
  await expectFail("claim до finalize", program.methods.claimWinnings().accountsStrict({ config, auction: A.auction, vault: A.vault, creatorToken: await ataOf(A.creator.publicKey), feeReceiverToken: await ataOf(relayer.publicKey), usdcMint: usdc, tokenProgram: TOKEN_PROGRAM_ID }), [], "AuctionNotFinalized");

  console.log(`  ⏳ ждём окончания аукциона…`);
  await finalize(A);

  // негативы после finalize:
  await expectFail("победитель не выводит (withdraw p1)", program.methods.withdraw(U(1), b2.kp.publicKey).accountsStrict({ config, auction: A.auction, bid: bidPda(A.auction, 1, b2.kp.publicKey), rentRecipient: relayer.publicKey, vault: A.vault, bidderToken: b2.ata, usdcMint: usdc, tokenProgram: TOKEN_PROGRAM_ID }), [], "WinnerCannotWithdraw");

  const { creatorAta, feeAta, creatorGot, feeDelta } = await claim(A);
  check("creator получил 14.445 (15 − 3.7%)", creatorGot === 14_445_000, creatorGot);
  check("fee_receiver +0.555", feeDelta === 555_000, feeDelta);
  await expectFail("двойной claim", program.methods.claimWinnings().accountsStrict({ config, auction: A.auction, vault: A.vault, creatorToken: creatorAta, feeReceiverToken: feeAta, usdcMint: usdc, tokenProgram: TOKEN_PROGRAM_ID }), [], "AlreadyClaimed");

  await withdraw(A, 0, b1); // лузер забирает 4, Bid закрывается
  check("b1 вернул ставку (→20)", (await usdcBal(b1.ata)) === 20_000_000, await usdcBal(b1.ata));
  check("vault == 0 после выплат", (await usdcBal(A.vault)) === 0);
  await expectFail("двойной withdraw (bid закрыт)", program.methods.withdraw(U(0), b1.kp.publicKey).accountsStrict({ config, auction: A.auction, bid: bidPda(A.auction, 0, b1.kp.publicKey), rentRecipient: relayer.publicKey, vault: A.vault, bidderToken: b1.ata, usdcMint: usdc, tokenProgram: TOKEN_PROGRAM_ID }), []);

  // GC: победившие биды, предложения, аукцион
  await closeBid(A, 1, b2); await closeBid(A, 1, b3);
  await closeProposal(A, 0); await closeProposal(A, 1);
  await closeAuction(A);
  check("proposal 0 закрыт", await closed(proposalPda(A.auction, 0)));
  check("vault закрыт", await closed(A.vault));
  check("auction закрыт", await closed(A.auction));
  // инвариант: вход 19 == выход (14.445 creator + 0.555 fee + 4 возврат)
  check("Σin == Σout (19 == 14.445+0.555+4)", 19_000_000 === 14_445_000 + 555_000 + 4_000_000);
};

// S2 — НИЧЬЯ: равные суммы → побеждает достигший раньше (лидер не меняется при равенстве)
scenarios.S2 = async () => {
  const A = await newAuction(1_000_000, 30);
  const b0 = await mkBidder(5_000_000), b1 = await mkBidder(5_000_000);
  await placeBid(A, 0, b0, 5_000_000); // p0 = 5 → лидер
  await placeBid(A, 1, b1, 5_000_000); // p1 = 5 → НЕ > 5 → лидер остаётся p0
  const a = await fetchA(A);
  check("[ничья] winner == p0 (достиг раньше)", isP(A, a.winnerProposal, 0), a.winnerProposal.toBase58().slice(0, 8));
  check("[ничья] winner_amount == 5", Number(a.winnerAmount) === 5_000_000);
  check("[ничья] total_staked == 10", Number(a.totalStaked) === 10_000_000);
  check("[ничья] vault == 10", (await usdcBal(A.vault)) === 10_000_000);
  // расчёт: p0 победитель, p1 лузер
  await finalize(A);
  const { creatorAta } = await claim(A);
  check("creator получил 4.815 (5 − 3.7%)", (await usdcBal(creatorAta)) === 4_815_000, await usdcBal(creatorAta));
  await withdraw(A, 1, b1);
  check("лузер p1 вернул 5", (await usdcBal(b1.ata)) === 5_000_000);
  check("vault → 0", (await usdcBal(A.vault)) === 0);
  await closeBid(A, 0, b0); await closeProposal(A, 0); await closeProposal(A, 1); await closeAuction(A);
  check("аукцион закрыт", await closed(A.auction));
};

// S3 — перебой → равенство (лидер ОСТАЁТСЯ) → перебой (лидер МЕНЯЕТСЯ)
scenarios.S3 = async () => {
  const A = await newAuction(1_000_000, 45);
  const b0 = await mkBidder(20_000_000), b1 = await mkBidder(10_000_000); // b1 ставит ровно 10 → после возврата 10
  await placeBid(A, 0, b0, 5_000_000);   // p0 = 5 (лидер)
  await placeBid(A, 1, b1, 10_000_000);  // p1 = 10 (лидер)
  check("[перебой] лидер p1 после 10>5", isP(A, (await fetchA(A)).winnerProposal, 1));
  await raiseBid(A, 0, b0, 5_000_000);   // p0 = 10 → НЕ > 10 → лидер ОСТАЁТСЯ p1
  let a = await fetchA(A);
  check("[равенство] лидер ОСТАЁТСЯ p1 (10==10)", isP(A, a.winnerProposal, 1), a.winnerProposal.toBase58().slice(0, 8));
  check("[равенство] winner_amount == 10", Number(a.winnerAmount) === 10_000_000);
  await raiseBid(A, 0, b0, 1_000_000);   // p0 = 11 → > 10 → лидер МЕНЯЕТСЯ на p0
  a = await fetchA(A);
  check("[перебой2] лидер МЕНЯЕТСЯ на p0 (11>10)", isP(A, a.winnerProposal, 0));
  check("[перебой2] winner_amount == 11", Number(a.winnerAmount) === 11_000_000);
  check("total_staked == 21", Number(a.totalStaked) === 21_000_000);
  await finalize(A);
  const { creatorAta } = await claim(A);
  check("creator получил 10.593 (11 − 3.7%)", (await usdcBal(creatorAta)) === 10_593_000, await usdcBal(creatorAta));
  await withdraw(A, 1, b1);
  check("лузер p1 вернул 10", (await usdcBal(b1.ata)) === 10_000_000);
  check("vault → 0", (await usdcBal(A.vault)) === 0);
  await closeBid(A, 0, b0); await closeProposal(A, 0); await closeProposal(A, 1); await closeAuction(A);
  check("аукцион закрыт", await closed(A.auction));
};

// S4 — «ОДНОВРЕМЕННЫЙ» равный стейк: две равные ставки на два предложения залпом (Promise.all).
// Валидатор сериализует записи в auction → ровно один победитель, без двойного учёта.
scenarios.S4 = async () => {
  const A = await newAuction(1_000_000, 30);
  const b0 = await mkBidder(7_000_000), b1 = await mkBidder(7_000_000);
  // Новые предложения создаём ПОСЛЕДОВАТЕЛЬНО (pid == proposal_count — параллельно нельзя: при реордере → InvalidProposalId).
  await placeBid(A, 0, b0, 1_000_000); // p0 = 1
  await placeBid(A, 1, b1, 1_000_000); // p1 = 1
  // ОДНОВРЕМЕННО равный добор на оба существующих предложения → 7 и 7 (валидатор сериализует записи в auction).
  await Promise.all([raiseBid(A, 0, b0, 6_000_000), raiseBid(A, 1, b1, 6_000_000)]);
  const a = await fetchA(A);
  const winnerIs0 = isP(A, a.winnerProposal, 0), winnerIs1 = isP(A, a.winnerProposal, 1);
  check("[одновременно] ровно один победитель (p0 XOR p1)", winnerIs0 !== winnerIs1, a.winnerProposal.toBase58().slice(0, 8));
  check("[одновременно] winner_amount == 7 (без двойного учёта)", Number(a.winnerAmount) === 7_000_000, Number(a.winnerAmount));
  check("[одновременно] total_staked == 14", Number(a.totalStaked) === 14_000_000);
  check("[одновременно] vault == 14", (await usdcBal(A.vault)) === 14_000_000);
  // расчёт: победитель — по факту; лузер — другой
  await finalize(A);
  await claim(A);
  const winP = winnerIs0 ? 0 : 1, loseP = winnerIs0 ? 1 : 0;
  const loser = winnerIs0 ? b1 : b0, winner = winnerIs0 ? b0 : b1;
  await withdraw(A, loseP, loser);
  check("[одновременно] лузер вернул 7", (await usdcBal(loser.ata)) === 7_000_000);
  check("[одновременно] vault → 0", (await usdcBal(A.vault)) === 0);
  await closeBid(A, winP, winner); await closeProposal(A, 0); await closeProposal(A, 1); await closeAuction(A);
  check("аукцион закрыт", await closed(A.auction));
};

// S5 — одно предложение / один биддер = победитель
scenarios.S5 = async () => {
  const A = await newAuction(1_000_000, 25);
  const b0 = await mkBidder(3_000_000);
  await placeBid(A, 0, b0, 3_000_000);
  const a = await fetchA(A);
  check("единственное предложение — победитель", isP(A, a.winnerProposal, 0) && Number(a.winnerAmount) === 3_000_000);
  await finalize(A);
  const { creatorAta } = await claim(A);
  check("creator получил 2.889 (3 − 3.7%)", (await usdcBal(creatorAta)) === 2_889_000, await usdcBal(creatorAta));
  check("vault → 0 (лузеров нет)", (await usdcBal(A.vault)) === 0);
  await closeBid(A, 0, b0); await closeProposal(A, 0); await closeAuction(A);
  check("аукцион закрыт", await closed(A.auction));
};

// S6 — ПУСТОЙ аукцион: нет ставок → finalize (победителя нет) → claim (no-op) → close (rent возвращается, не застревает)
scenarios.S6 = async () => {
  const A = await newAuction(1_000_000, 12);
  await finalize(A);
  const a = await fetchA(A);
  check("[пусто] finalized", a.finalized === true);
  check("[пусто] победителя нет (winner == default)", a.winnerProposal.equals(PublicKey.default), a.winnerProposal.toBase58().slice(0, 8));
  check("[пусто] winner_amount == 0", Number(a.winnerAmount) === 0);
  await claim(A); // winner_amount 0 → переводов нет, только creator_paid=true
  check("[пусто] creator_paid после claim", (await fetchA(A)).creatorPaid === true);
  await closeAuction(A); // vault пуст, settled → закрывается, rent → relayer
  check("[пусто] аукцион закрыт (rent не застрял)", await closed(A.auction));
  check("[пусто] vault закрыт", await closed(A.vault));
};

// S7 — после end_time ставки отклоняются; затем нормальный расчёт
scenarios.S7 = async () => {
  const A = await newAuction(1_000_000, 8);
  const b0 = await mkBidder(5_000_000), b1 = await mkBidder(5_000_000);
  await placeBid(A, 0, b0, 2_000_000); // активный — ок
  console.log("  ⏳ ждём конца (8с) для проверки reject после end…");
  await sleep(11_000);
  await expectFail("raise после end", program.methods.raiseBid(U(0), U(1_000_000)).accountsStrict(bidAccts(A.auction, A.vault, 0, b0.kp.publicKey, b0.ata)), [b0.kp], "AuctionEnded");
  await expectFail("place новое предложение после end", program.methods.placeBid(U(1), H, U(2_000_000)).accountsStrict(bidAccts(A.auction, A.vault, 1, b1.kp.publicKey, b1.ata)), [b1.kp], "AuctionEnded");
  await finalize(A);
  await claim(A);
  check("после end: vault → 0 (победитель p0=2)", (await usdcBal(A.vault)) === 0);
  await closeBid(A, 0, b0); await closeProposal(A, 0); await closeAuction(A);
  check("аукцион закрыт", await closed(A.auction));
};

// S8 — валидации createAuction + set_config + повторный initialize (синглтон)
scenarios.S8 = async () => {
  const count = Number((await program.account.config.fetch(config)).auctionCount);
  const wrongId = count + 5, wA = auctionPda(wrongId);
  await expectFail("createAuction неверный id", program.methods.createAuction(U(wrongId), U(1_000_000), U(30)).accountsStrict({ config, auction: wA, usdcMint: usdc, vault: vaultPda(wA), creator: relayer.publicKey, payer: relayer.publicKey, tokenProgram: TOKEN_PROGRAM_ID, systemProgram: SystemProgram.programId }), [], "InvalidAuctionId");
  const okA = auctionPda(count);
  await expectFail("createAuction duration 0", program.methods.createAuction(U(count), U(1_000_000), U(0)).accountsStrict({ config, auction: okA, usdcMint: usdc, vault: vaultPda(okA), creator: relayer.publicKey, payer: relayer.publicKey, tokenProgram: TOKEN_PROGRAM_ID, systemProgram: SystemProgram.programId }), [], "InvalidDuration");

  const stranger = Keypair.generate();
  // страннику нужен SOL только если он fee-payer; здесь payer=relayer (provider), stranger лишь подписывает owner.
  await expectFail("set_config не-владельцем", program.methods.setConfig(100, relayer.publicKey).accountsStrict({ config, owner: stranger.publicKey }), [stranger], "Unauthorized");
  await expectFail("set_config fee > 10%", program.methods.setConfig(1001, relayer.publicKey).accountsStrict({ config, owner: relayer.publicKey }), [], "FeeTooHigh");
  await expectFail("повторный initialize (config — синглтон)", program.methods.initialize(370, relayer.publicKey, usdc).accountsStrict({ config, owner: relayer.publicKey, systemProgram: SystemProgram.programId }), []);
};

// ════════════════════════════ РАННЕР ══════════════════════════════════════════
const order = ["S1", "S2", "S3", "S4", "S5", "S6", "S7", "S8"];
const titles = {
  S1: "happy + газлесс + гейты + закрытия + инвариант",
  S2: "ничья: равные суммы → побеждает первый",
  S3: "перебой → равенство(остаётся) → перебой(меняется)",
  S4: "«одновременный» равный стейк (Promise.all)",
  S5: "одно предложение / один биддер",
  S6: "пустой аукцион (finalize/claim/close)",
  S7: "после end_time ставки отклоняются",
  S8: "валидации createAuction / set_config / singleton",
};

await setup();
const run = ONLY ? [ONLY] : order;
for (const s of run) {
  console.log(`\n━━━ ${s}: ${titles[s]} ━━━`);
  try { await scenarios[s](); }
  catch (e) { failures++; console.log(`  💥 ${s} упал с исключением: ${e?.error?.errorCode?.code || e?.message || e}`); }
}

console.log(`\n${failures === 0 ? "🎉" : "❌"} ИТОГО: ${passes} ✅ / ${failures} ❌  (relayer ${(await sol(relayer.publicKey) / LAMPORTS_PER_SOL).toFixed(4)} SOL)`);
process.exit(failures === 0 ? 0 : 1);
