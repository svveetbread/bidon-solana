// Полный e2e против ЗАДЕПЛОЕННОЙ программы: создание → ставки+добор → финал → claim → withdraw.
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
const bal = async (ata) => Number((await getAccount(connection, ata)).amount);

const payer = Keypair.generate(); // он же fee_receiver
await airdrop(payer.publicKey, 100);
const provider = new AnchorProvider(connection, new Wallet(payer), { commitment: "confirmed" });
const program = new Program(idl, provider);
const pid = program.programId;

const [config] = PublicKey.findProgramAddressSync([Buffer.from("config")], pid);
const auctionPda = (id) => PublicKey.findProgramAddressSync([Buffer.from("auction"), le8(id)], pid)[0];
const vaultPda = (a) => PublicKey.findProgramAddressSync([Buffer.from("vault"), a.toBuffer()], pid)[0];
const proposalPda = (a, p) => PublicKey.findProgramAddressSync([Buffer.from("proposal"), a.toBuffer(), le8(p)], pid)[0];
const bidPda = (a, p, b) => PublicKey.findProgramAddressSync([Buffer.from("bid"), a.toBuffer(), le8(p), b.toBuffer()], pid)[0];

const usdc = await createMint(connection, payer, payer.publicKey, null, 6);
const creator = Keypair.generate();
await airdrop(creator.publicKey, 2);

// initialize
await program.methods.initialize(370, payer.publicKey, usdc)
  .accountsStrict({ config, owner: payer.publicKey, systemProgram: SystemProgram.programId }).rpc();

// create_auction (id=0, min 1 USDC, длительность 8с)
const DURATION = 8;
const auction = auctionPda(0);
const vault = vaultPda(auction);
await program.methods.createAuction(U(0), U(1_000_000), U(DURATION))
  .accountsStrict({ config, auction, usdcMint: usdc, vault, creator: creator.publicKey, tokenProgram: TOKEN_PROGRAM_ID, systemProgram: SystemProgram.programId })
  .signers([creator]).rpc();

async function mkBidder(amount) {
  const kp = Keypair.generate();
  await airdrop(kp.publicKey, 2);
  const ata = (await getOrCreateAssociatedTokenAccount(connection, payer, usdc, kp.publicKey)).address;
  await mintTo(connection, payer, usdc, ata, payer, amount);
  return { kp, ata };
}
const common = (p, b) => ({ config, auction, proposal: proposalPda(auction, p), bid: bidPda(auction, p, b.kp.publicKey), usdcMint: usdc, vault, bidderToken: b.ata, bidder: b.kp.publicKey, tokenProgram: TOKEN_PROGRAM_ID, systemProgram: SystemProgram.programId });
const placeBid = (b, p, amt) => program.methods.placeBid(U(p), Array(32).fill(0), U(amt)).accountsStrict(common(p, b)).signers([b.kp]).rpc();
const raiseBid = (b, p, amt) => program.methods.raiseBid(U(p), U(amt)).accountsStrict(common(p, b)).signers([b.kp]).rpc();

const b1 = await mkBidder(20_000_000);
const b2 = await mkBidder(20_000_000);
const b3 = await mkBidder(20_000_000);

await placeBid(b1, 0, 4_000_000);  // p0 лузер
await placeBid(b2, 1, 10_000_000); // p1
await raiseBid(b3, 1, 5_000_000);  // p1 += 5 → 15 (победитель)

let a = await program.account.auction.fetch(auction);
check("total_staked == 19 USDC", Number(a.totalStaked) === 19_000_000, a.totalStaked.toString());
check("winner == proposal 1", a.winnerProposal.equals(proposalPda(auction, 1)));
check("winner_amount == 15 USDC", Number(a.winnerAmount) === 15_000_000, a.winnerAmount.toString());
check("vault == 19 USDC", (await bal(vault)) === 19_000_000);

console.log(`⏳ ждём окончания аукциона (${DURATION}s)...`);
await new Promise((r) => setTimeout(r, (DURATION + 3) * 1000));

// finalize (permissionless)
await program.methods.finalize().accountsStrict({ auction }).rpc();
a = await program.account.auction.fetch(auction);
check("finalized", a.finalized === true);

// claim_winnings
const creatorAta = (await getOrCreateAssociatedTokenAccount(connection, payer, usdc, creator.publicKey)).address;
const feeAta = (await getOrCreateAssociatedTokenAccount(connection, payer, usdc, payer.publicKey)).address;
await program.methods.claimWinnings()
  .accountsStrict({ config, auction, vault, creatorToken: creatorAta, feeReceiverToken: feeAta, usdcMint: usdc, tokenProgram: TOKEN_PROGRAM_ID }).rpc();
check("creator получил 14.445 USDC", (await bal(creatorAta)) === 14_445_000);
check("fee_receiver получил 0.555 USDC", (await bal(feeAta)) === 555_000);
check("vault == 4 USDC (остался лузер)", (await bal(vault)) === 4_000_000);

// победитель НЕ может вывести
let winnerBlocked = false;
try {
  await program.methods.withdraw(U(1), b2.kp.publicKey)
    .accountsStrict({ config, auction, bid: bidPda(auction, 1, b2.kp.publicKey), vault, bidderToken: b2.ata, usdcMint: usdc, tokenProgram: TOKEN_PROGRAM_ID }).rpc();
} catch { winnerBlocked = true; }
check("победитель НЕ может withdraw", winnerBlocked);

// лузер забирает ставку → vault опустошается
await program.methods.withdraw(U(0), b1.kp.publicKey)
  .accountsStrict({ config, auction, bid: bidPda(auction, 0, b1.kp.publicKey), vault, bidderToken: b1.ata, usdcMint: usdc, tokenProgram: TOKEN_PROGRAM_ID }).rpc();
check("vault == 0 (всё роздано)", (await bal(vault)) === 0);
check("b1 вернул ставку (→20 USDC)", (await bal(b1.ata)) === 20_000_000);
check("b2 победитель НЕ вернул (10 USDC)", (await bal(b2.ata)) === 10_000_000);

console.log(failures === 0 ? "\n🎉 E2E ПОЛНОСТЬЮ ЗЕЛЁНЫЙ на задеплоенной программе" : `\n❌ упало проверок: ${failures}`);
process.exit(failures === 0 ? 0 : 1);
