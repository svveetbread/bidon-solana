// Газлесс e2e против ЗАДЕПЛОЕННОЙ программы: relayer (провайдер) платит rent+fee,
// creator/биддеры с НУЛЁМ SOL только подписывают как authority. Полный цикл + закрытия (возврат rent).
// Сценарий: p0=4 USDC (b1, лузер) · p1=10(b2)+5(b3)=15 USDC (победитель). Итого 19. fee=0.555, creator=14.445.
import anchor from "@coral-xyz/anchor";
import { Connection, Keypair, PublicKey, LAMPORTS_PER_SOL, SystemProgram } from "@solana/web3.js";
import {
  createMint, getOrCreateAssociatedTokenAccount, mintTo, getAccount, TOKEN_PROGRAM_ID,
} from "@solana/spl-token";
import { readFileSync } from "fs";

const { Program, AnchorProvider, Wallet, BN } = anchor;
const RPC = process.env.RPC || "http://127.0.0.1:8899";
const idl = JSON.parse(readFileSync(new URL("../bidon/target/idl/bidon.json", import.meta.url)));
const connection = new Connection(RPC, "confirmed");

const U = (n) => new BN(n);
const le8 = (n) => U(n).toArrayLike(Buffer, "le", 8);
let failures = 0;
const check = (name, cond, got) => { console.log(`${cond ? "✅" : "❌"} ${name}${cond ? "" : `  (got: ${got})`}`); if (!cond) failures++; };
const airdrop = async (pk, sol) => { const s = await connection.requestAirdrop(pk, sol * LAMPORTS_PER_SOL); await connection.confirmTransaction(s, "confirmed"); };
const usdcBal = async (ata) => Number((await getAccount(connection, ata)).amount);
const sol = async (pk) => connection.getBalance(pk);
const closed = async (pk) => (await connection.getAccountInfo(pk)) === null;

// relayer = провайдер (платит rent+fee за всех). У него SOL.
const relayer = Keypair.generate();
await airdrop(relayer.publicKey, 100);
const provider = new AnchorProvider(connection, new Wallet(relayer), { commitment: "confirmed" });
const program = new Program(idl, provider);
const pid = program.programId;

const [config] = PublicKey.findProgramAddressSync([Buffer.from("config")], pid);
const auctionPda = (id) => PublicKey.findProgramAddressSync([Buffer.from("auction"), le8(id)], pid)[0];
const vaultPda = (a) => PublicKey.findProgramAddressSync([Buffer.from("vault"), a.toBuffer()], pid)[0];
const proposalPda = (a, p) => PublicKey.findProgramAddressSync([Buffer.from("proposal"), a.toBuffer(), le8(p)], pid)[0];
const bidPda = (a, p, b) => PublicKey.findProgramAddressSync([Buffer.from("bid"), a.toBuffer(), le8(p), b.toBuffer()], pid)[0];

// USDC mint (relayer — авторитет/плательщик), fee_receiver = relayer
const usdc = await createMint(connection, relayer, relayer.publicKey, null, 6);
await program.methods.initialize(370, relayer.publicKey, usdc)
  .accountsStrict({ config, owner: relayer.publicKey, systemProgram: SystemProgram.programId }).rpc();

// creator БЕЗ SOL
const creator = Keypair.generate();
const DURATION = 8;
const auction = auctionPda(0);
const vault = vaultPda(auction);
await program.methods.createAuction(U(0), U(1_000_000), U(DURATION))
  .accountsStrict({ config, auction, usdcMint: usdc, vault, creator: creator.publicKey, payer: relayer.publicKey, tokenProgram: TOKEN_PROGRAM_ID, systemProgram: SystemProgram.programId })
  .signers([creator]).rpc();
check("creator с 0 SOL создал аукцион", (await sol(creator.publicKey)) === 0);

// биддер БЕЗ SOL, USDC даёт relayer
async function mkBidder(amount) {
  const kp = Keypair.generate();
  const ata = (await getOrCreateAssociatedTokenAccount(connection, relayer, usdc, kp.publicKey)).address;
  await mintTo(connection, relayer, usdc, ata, relayer, amount);
  return { kp, ata };
}
const common = (p, b) => ({ config, auction, proposal: proposalPda(auction, p), bid: bidPda(auction, p, b.kp.publicKey), usdcMint: usdc, vault, bidderToken: b.ata, bidder: b.kp.publicKey, payer: relayer.publicKey, tokenProgram: TOKEN_PROGRAM_ID, systemProgram: SystemProgram.programId });
const placeBid = (b, p, amt) => program.methods.placeBid(U(p), Array(32).fill(0), U(amt)).accountsStrict(common(p, b)).signers([b.kp]).rpc();
const raiseBid = (b, p, amt) => program.methods.raiseBid(U(p), U(amt)).accountsStrict(common(p, b)).signers([b.kp]).rpc();

const b1 = await mkBidder(20_000_000);
const b2 = await mkBidder(20_000_000);
const b3 = await mkBidder(20_000_000);
await placeBid(b1, 0, 4_000_000);   // p0 лузер (relayer платит rent, b1 подписывает)
await placeBid(b2, 1, 10_000_000);  // p1
await raiseBid(b3, 1, 5_000_000);   // p1 += 5 → 15 (победитель)

// ГЛАВНОЕ: юзеры не потратили SOL
check("b1 потратил 0 SOL", (await sol(b1.kp.publicKey)) === 0);
check("b2 потратил 0 SOL", (await sol(b2.kp.publicKey)) === 0);
check("b3 потратил 0 SOL", (await sol(b3.kp.publicKey)) === 0);

let a = await program.account.auction.fetch(auction);
check("total_staked == 19", Number(a.totalStaked) === 19_000_000, a.totalStaked.toString());
check("winner == proposal 1", a.winnerProposal.equals(proposalPda(auction, 1)));
check("vault == 19", (await usdcBal(vault)) === 19_000_000);

const relayerLow = await sol(relayer.publicKey); // rent максимально заморожен

console.log(`⏳ ждём окончания аукциона (${DURATION}s)...`);
await new Promise((r) => setTimeout(r, (DURATION + 3) * 1000));

// расчёт (permissionless — relayer крэнкает, но мог бы кто угодно)
await program.methods.finalize().accountsStrict({ auction }).rpc();
const creatorAta = (await getOrCreateAssociatedTokenAccount(connection, relayer, usdc, creator.publicKey)).address;
const feeAta = (await getOrCreateAssociatedTokenAccount(connection, relayer, usdc, relayer.publicKey)).address;
await program.methods.claimWinnings()
  .accountsStrict({ config, auction, vault, creatorToken: creatorAta, feeReceiverToken: feeAta, usdcMint: usdc, tokenProgram: TOKEN_PROGRAM_ID }).rpc();
check("creator получил 14.445 USDC", (await usdcBal(creatorAta)) === 14_445_000);
check("fee_receiver получил 0.555 USDC", (await usdcBal(feeAta)) === 555_000);

// лузер b1 забирает ставку (Bid закрывается, rent → relayer)
await program.methods.withdraw(U(0), b1.kp.publicKey)
  .accountsStrict({ config, auction, bid: bidPda(auction, 0, b1.kp.publicKey), rentRecipient: relayer.publicKey, vault, bidderToken: b1.ata, usdcMint: usdc, tokenProgram: TOKEN_PROGRAM_ID }).rpc();
check("b1 вернул ставку (→20 USDC)", (await usdcBal(b1.ata)) === 20_000_000);
check("vault == 0", (await usdcBal(vault)) === 0);

// GC: победившие биды (b2,b3 на p1), оба предложения, аукцион → rent relayer'у
const closeBid = (p, b) => program.methods.closeBid(U(p), b.kp.publicKey).accountsStrict({ auction, bid: bidPda(auction, p, b.kp.publicKey), rentRecipient: relayer.publicKey }).rpc();
await closeBid(1, b2);
await closeBid(1, b3);
await program.methods.closeProposal(U(0)).accountsStrict({ auction, proposal: proposalPda(auction, 0), rentRecipient: relayer.publicKey }).rpc();
await program.methods.closeProposal(U(1)).accountsStrict({ auction, proposal: proposalPda(auction, 1), rentRecipient: relayer.publicKey }).rpc();
await program.methods.closeAuction().accountsStrict({ auction, vault, rentRecipient: relayer.publicKey, tokenProgram: TOKEN_PROGRAM_ID }).rpc();

check("b1 bid закрыт", await closed(bidPda(auction, 0, b1.kp.publicKey)));
check("b2 bid закрыт", await closed(bidPda(auction, 1, b2.kp.publicKey)));
check("proposal 0 закрыт", await closed(proposalPda(auction, 0)));
check("vault закрыт", await closed(vault));
check("auction закрыт", await closed(auction));
check("relayer вернул rent (баланс вырос после закрытий)", (await sol(relayer.publicKey)) > relayerLow);

console.log(failures === 0 ? "\n🎉 ГАЗЛЕСС E2E ЗЕЛЁНЫЙ: юзеры 0 SOL, relayer платит и забирает rent" : `\n❌ упало проверок: ${failures}`);
process.exit(failures === 0 ? 0 : 1);
