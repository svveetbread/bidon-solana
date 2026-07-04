#![cfg(feature = "test-sbf")]
#![allow(deprecated)]

use anchor_lang::{AccountDeserialize, InstructionData, ToAccountMetas};
use anchor_spl::token::spl_token;
use bidon_zk::{Auction, Config};
use light_program_test::{program_test::LightProgramTest, ProgramTestConfig, Rpc};
use solana_sdk::{
    instruction::Instruction,
    pubkey::Pubkey,
    signature::{Keypair, Signer},
    system_instruction, system_program,
};

/// Foundation: the regular (uncompressed) accounts and their instructions, the same
/// shape as the old 9GSQ program. Validates that anchor-spl (Config/Auction/Vault)
/// coexists with light-sdk in one program. Gasless: creator signs with 0 SOL, the
/// relayer (payer) fronts all rent.
#[tokio::test]
async fn test_foundation() {
    let config_cfg = ProgramTestConfig::new(true, Some(vec![("bidon_zk", bidon_zk::ID)]));
    let mut rpc = LightProgramTest::new(config_cfg).await.unwrap();
    let payer = rpc.get_payer().insecure_clone(); // relayer, pre-funded

    let owner = Keypair::new();
    rpc.airdrop_lamports(&owner.pubkey(), 5_000_000_000)
        .await
        .unwrap();

    // Create the platform USDC mint (6 decimals, authority = payer).
    let mint = Keypair::new();
    let mint_len = 82usize; // spl_token::state::Mint packed size
    let mint_rent = rpc
        .get_minimum_balance_for_rent_exemption(mint_len)
        .await
        .unwrap();
    let create_mint = system_instruction::create_account(
        &payer.pubkey(),
        &mint.pubkey(),
        mint_rent,
        mint_len as u64,
        &spl_token::ID,
    );
    let init_mint = spl_token::instruction::initialize_mint(
        &spl_token::ID,
        &mint.pubkey(),
        &payer.pubkey(),
        None,
        6,
    )
    .unwrap();
    rpc.create_and_send_transaction(&[create_mint, init_mint], &payer.pubkey(), &[&payer, &mint])
        .await
        .unwrap();

    // 1. initialize — owner pays its own Config rent.
    let (config_pda, _) = Pubkey::find_program_address(&[b"config"], &bidon_zk::ID);
    let ix = Instruction {
        program_id: bidon_zk::ID,
        accounts: bidon_zk::accounts::Initialize {
            config: config_pda,
            owner: owner.pubkey(),
            system_program: system_program::ID,
        }
        .to_account_metas(None),
        data: bidon_zk::instruction::Initialize {
            fee_bps: 370,
            fee_receiver: owner.pubkey(),
            usdc_mint: mint.pubkey(),
        }
        .data(),
    };
    rpc.create_and_send_transaction(&[ix], &owner.pubkey(), &[&owner])
        .await
        .unwrap();

    let cfg = get_config(&mut rpc, config_pda).await;
    assert_eq!(cfg.owner, owner.pubkey());
    assert_eq!(cfg.fee_bps, 370);
    assert_eq!(cfg.usdc_mint, mint.pubkey());
    assert_eq!(cfg.auction_count, 0);

    // 2. set_config — owner-only fee update.
    let ix = Instruction {
        program_id: bidon_zk::ID,
        accounts: bidon_zk::accounts::SetConfig {
            config: config_pda,
            owner: owner.pubkey(),
        }
        .to_account_metas(None),
        data: bidon_zk::instruction::SetConfig {
            fee_bps: 500,
            fee_receiver: owner.pubkey(),
        }
        .data(),
    };
    rpc.create_and_send_transaction(&[ix], &owner.pubkey(), &[&owner])
        .await
        .unwrap();
    assert_eq!(get_config(&mut rpc, config_pda).await.fee_bps, 500);

    // 3a. init_deposit_vault (schema 3): create the GLOBAL creator-deposit vault (relayer fronts its rent).
    let (deposit_vault_pda, _) =
        Pubkey::find_program_address(&[b"deposit_vault"], &bidon_zk::ID);
    let ix = Instruction {
        program_id: bidon_zk::ID,
        accounts: bidon_zk::accounts::InitDepositVault {
            config: config_pda,
            usdc_mint: mint.pubkey(),
            deposit_vault: deposit_vault_pda,
            payer: payer.pubkey(),
            token_program: spl_token::ID,
            system_program: system_program::ID,
        }
        .to_account_metas(None),
        data: bidon_zk::instruction::InitDepositVault {}.data(),
    };
    rpc.create_and_send_transaction(&[ix], &payer.pubkey(), &[&payer])
        .await
        .unwrap();
    assert_eq!(token_amount(&mut rpc, deposit_vault_pda).await, 0);

    // 3b. Fund the creator's USDC account (source of the schema-3 deposit). Creator still holds 0 SOL.
    let creator = Keypair::new();
    let creator_funding = 10_000_000u64; // 10 USDC
    let creator_token = new_token_account(
        &mut rpc,
        &payer,
        mint.pubkey(),
        &creator.pubkey(),
        creator_funding,
    )
    .await;

    // 3c. create_auction — gasless: creator has 0 SOL, relayer (payer) fronts rent. Pulls CREATOR_DEPOSIT
    // from the creator into the GLOBAL deposit vault (NOT the per-auction vault, which stays empty).
    let id = 0u64;
    let (auction_pda, _) =
        Pubkey::find_program_address(&[b"auction", &id.to_le_bytes()], &bidon_zk::ID);
    let (vault_pda, _) =
        Pubkey::find_program_address(&[b"vault", auction_pda.as_ref()], &bidon_zk::ID);
    let (auction_ext_pda, _) =
        Pubkey::find_program_address(&[b"auction_ext", &id.to_le_bytes()], &bidon_zk::ID); // §7 компаньон
    let ix = Instruction {
        program_id: bidon_zk::ID,
        accounts: bidon_zk::accounts::CreateAuction {
            config: config_pda,
            auction: auction_pda,
            usdc_mint: mint.pubkey(),
            vault: vault_pda,
            deposit_vault: deposit_vault_pda,
            auction_ext: auction_ext_pda,
            creator: creator.pubkey(),
            creator_token,
            payer: payer.pubkey(),
            token_program: spl_token::ID,
            system_program: system_program::ID,
        }
        .to_account_metas(None),
        data: bidon_zk::instruction::CreateAuction {
            id,
            min_bid: 100_000, // 0.1 USDC
            duration_secs: 3600,
            winner_count: 1,
            max_extension_secs: 600, // §7 антиснайп (в границах 60..3600)
        }
        .data(),
    };
    rpc.create_and_send_transaction(&[ix], &payer.pubkey(), &[&payer, &creator])
        .await
        .unwrap();

    let auction = get_auction(&mut rpc, auction_pda).await;
    assert_eq!(auction.id, 0);
    assert_eq!(auction.creator, creator.pubkey());
    assert_eq!(auction.min_bid, 100_000);
    assert_eq!(auction.fee_bps, 500); // snapshot of current config fee
    assert_eq!(auction.winner_amount, 0);
    assert_eq!(auction.rent_payer, payer.pubkey()); // relayer gets rent back on close
    assert!(!auction.creator_paid);
    assert_eq!(auction.schema_version, 3); // global-vault deposit schema

    // Schema-3 deposit conservation: the GLOBAL deposit vault holds exactly CREATOR_DEPOSIT; the creator
    // was debited by CREATOR_DEPOSIT; the per-auction vault holds NOTHING (deposit is global, not per-auction).
    assert_eq!(
        token_amount(&mut rpc, deposit_vault_pda).await,
        bidon_zk::CREATOR_DEPOSIT
    );
    assert_eq!(
        token_amount(&mut rpc, creator_token).await,
        creator_funding - bidon_zk::CREATOR_DEPOSIT
    );
    assert_eq!(token_amount(&mut rpc, vault_pda).await, 0);

    // creator stayed accountless = 0 SOL (gasless), relayer paid all rent.
    assert!(
        rpc.get_account(creator.pubkey()).await.unwrap().is_none(),
        "creator must stay at 0 SOL (gasless)"
    );

    // auction_count advanced.
    assert_eq!(get_config(&mut rpc, config_pda).await.auction_count, 1);
}

/// SPL token account amount is at offset 64 (mint 32 + owner 32).
async fn token_amount(rpc: &mut LightProgramTest, pda: Pubkey) -> u64 {
    let acc = rpc.get_account(pda).await.unwrap().unwrap();
    u64::from_le_bytes(acc.data[64..72].try_into().unwrap())
}

/// Create a USDC token account owned by `owner`, funded with `amount` (mint authority = payer).
async fn new_token_account(
    rpc: &mut LightProgramTest,
    payer: &Keypair,
    mint: Pubkey,
    owner: &Pubkey,
    amount: u64,
) -> Pubkey {
    let acc = Keypair::new();
    let rent = rpc.get_minimum_balance_for_rent_exemption(165).await.unwrap();
    let create =
        system_instruction::create_account(&payer.pubkey(), &acc.pubkey(), rent, 165, &spl_token::ID);
    let init =
        spl_token::instruction::initialize_account3(&spl_token::ID, &acc.pubkey(), &mint, owner)
            .unwrap();
    let mint_to = spl_token::instruction::mint_to(
        &spl_token::ID,
        &mint,
        &acc.pubkey(),
        &payer.pubkey(),
        &[],
        amount,
    )
    .unwrap();
    rpc.create_and_send_transaction(&[create, init, mint_to], &payer.pubkey(), &[payer, &acc])
        .await
        .unwrap();
    acc.pubkey()
}

async fn get_config(rpc: &mut LightProgramTest, pda: Pubkey) -> Config {
    let acc = rpc.get_account(pda).await.unwrap().unwrap();
    Config::try_deserialize(&mut acc.data.as_slice()).unwrap()
}

async fn get_auction(rpc: &mut LightProgramTest, pda: Pubkey) -> Auction {
    let acc = rpc.get_account(pda).await.unwrap().unwrap();
    Auction::try_deserialize(&mut acc.data.as_slice()).unwrap()
}
