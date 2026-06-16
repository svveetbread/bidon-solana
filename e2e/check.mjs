// Минимальная проверка: клиент ↔ IDL ↔ задеплоенная программа (initialize + чтение config).
import anchor from "@coral-xyz/anchor";
import { Connection, Keypair, PublicKey, LAMPORTS_PER_SOL, SystemProgram } from "@solana/web3.js";
import { createMint } from "@solana/spl-token";
import { readFileSync } from "fs";

const { Program, AnchorProvider, Wallet } = anchor;
const RPC = process.env.RPC || "http://127.0.0.1:8899";

const idl = JSON.parse(readFileSync(new URL("../bidon/target/idl/bidon.json", import.meta.url)));
const connection = new Connection(RPC, "confirmed");

const payer = Keypair.generate();
const sig = await connection.requestAirdrop(payer.publicKey, 100 * LAMPORTS_PER_SOL);
await connection.confirmTransaction(sig, "confirmed");
console.log("payer:", payer.publicKey.toBase58(), "balance:", (await connection.getBalance(payer.publicKey)) / LAMPORTS_PER_SOL);

const provider = new AnchorProvider(connection, new Wallet(payer), { commitment: "confirmed" });
const program = new Program(idl, provider);
console.log("programId:", program.programId.toBase58());

const usdc = await createMint(connection, payer, payer.publicKey, null, 6);
console.log("usdc mint:", usdc.toBase58());

const [config] = PublicKey.findProgramAddressSync([Buffer.from("config")], program.programId);
await program.methods
  .initialize(370, payer.publicKey, usdc)
  .accounts({ config, owner: payer.publicKey, systemProgram: SystemProgram.programId })
  .rpc();

const cfg = await program.account.config.fetch(config);
console.log("config.feeBps:", cfg.feeBps, "owner:", cfg.owner.toBase58(), "usdcMint:", cfg.usdcMint.toBase58());
console.log("✅ initialize прошёл на задеплоенной программе");
