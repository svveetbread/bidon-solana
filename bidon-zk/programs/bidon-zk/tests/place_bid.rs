#![cfg(feature = "test-sbf")]
#![allow(deprecated)]

use anchor_lang::{AccountDeserialize, AnchorDeserialize, InstructionData, ToAccountMetas};
use anchor_spl::token::spl_token;
use bidon_zk::{Auction, Bid, ProposalTotal};
use light_program_test::{
    program_test::LightProgramTest, AddressWithTree, Indexer, ProgramTestConfig, Rpc,
};
use light_sdk::{
    address::v2::derive_address,
    instruction::{PackedAccounts, SystemAccountMetaConfig},
};
use solana_sdk::{
    compute_budget::ComputeBudgetInstruction,
    instruction::Instruction,
    pubkey::Pubkey,
    signature::{Keypair, Signer},
    system_instruction, system_program,
};

const CONTENT: [u8; 32] = [3u8; 32];

/// Full place_bid on a new proposal: pulls USDC into the vault, creates ProposalTotal +
/// Bid (compressed, combined proof), bumps the auction leader. Gasless: bidder has 0 SOL,
/// relayer (payer) is the Light fee payer.
#[tokio::test]
async fn test_place_bid() {
    let cfg = ProgramTestConfig::new(true, Some(vec![("bidon_zk", bidon_zk::ID)]));
    let mut rpc = LightProgramTest::new(cfg).await.unwrap();
    let payer = rpc.get_payer().insecure_clone();

    let owner = Keypair::new();
    rpc.airdrop_lamports(&owner.pubkey(), 5_000_000_000)
        .await
        .unwrap();

    let mint = create_mint(&mut rpc, &payer).await;
    let (config_pda, _) = Pubkey::find_program_address(&[b"config"], &bidon_zk::ID);
    initialize_config(&mut rpc, &owner, config_pda, mint).await;

    let creator = Keypair::new();
    let id = 0u64;
    let (auction_pda, _) =
        Pubkey::find_program_address(&[b"auction", &id.to_le_bytes()], &bidon_zk::ID);
    let (vault_pda, _) =
        Pubkey::find_program_address(&[b"vault", auction_pda.as_ref()], &bidon_zk::ID);
    create_auction(
        &mut rpc, &payer, &creator, config_pda, auction_pda, vault_pda, mint, id, 100_000,
    )
    .await;

    // Bidder funded with 1 USDC.
    let bidder = Keypair::new();
    let bidder_token = funded_token_account(&mut rpc, &payer, mint, &bidder.pubkey(), 1_000_000).await;

    // place_bid: proposal 0, amount 0.5 USDC.
    let amount = 500_000u64;
    let pid = 0u64;
    let address_tree = rpc.get_address_tree_v2().tree;
    let (proposal_addr, _) = derive_address(
        &[b"proposal", auction_pda.as_ref(), &pid.to_le_bytes()],
        &address_tree,
        &bidon_zk::ID,
    );
    let (bid_addr, _) = derive_address(
        &[
            b"bid",
            auction_pda.as_ref(),
            &pid.to_le_bytes(),
            bidder.pubkey().as_ref(),
        ],
        &address_tree,
        &bidon_zk::ID,
    );

    let mut remaining = PackedAccounts::default();
    remaining
        .add_system_accounts_v2(SystemAccountMetaConfig::new(bidon_zk::ID))
        .unwrap();
    let rpc_result = rpc
        .get_validity_proof(
            vec![],
            vec![
                AddressWithTree {
                    tree: address_tree,
                    address: proposal_addr,
                },
                AddressWithTree {
                    tree: address_tree,
                    address: bid_addr,
                },
            ],
            None,
        )
        .await
        .unwrap()
        .value;
    let output_state_tree_index = rpc
        .get_random_state_tree_info()
        .unwrap()
        .pack_output_tree_index(&mut remaining)
        .unwrap();
    let packed = rpc_result.pack_tree_infos(&mut remaining);

    let data = bidon_zk::instruction::PlaceBid {
        proof: rpc_result.proof,
        proposal_address_tree_info: packed.address_trees[0],
        bid_address_tree_info: packed.address_trees[1],
        output_state_tree_index,
        content_hash: CONTENT,
        amount,
    }
    .data();
    let accounts = bidon_zk::accounts::PlaceBid {
        config: config_pda,
        auction: auction_pda,
        vault: vault_pda,
        usdc_mint: mint,
        bidder_token,
        bidder: bidder.pubkey(),
        payer: payer.pubkey(),
        token_program: spl_token::ID,
    };
    let (rem, _, _) = remaining.to_account_metas();
    let ix = Instruction {
        program_id: bidon_zk::ID,
        accounts: [accounts.to_account_metas(None), rem].concat(),
        data,
    };
    let cu = ComputeBudgetInstruction::set_compute_unit_limit(400_000);
    rpc.create_and_send_transaction(&[cu, ix], &payer.pubkey(), &[&payer, &bidder])
        .await
        .unwrap();

    // ProposalTotal + Bid created.
    let p = rpc
        .get_compressed_account(proposal_addr, None)
        .await
        .unwrap()
        .value
        .unwrap();
    let proposal = ProposalTotal::deserialize(&mut &p.data.as_ref().unwrap().data[..]).unwrap();
    assert_eq!(proposal.total, amount);
    assert_eq!(proposal.creator, bidder.pubkey());
    assert_eq!(proposal.content_hash, CONTENT);

    let b = rpc
        .get_compressed_account(bid_addr, None)
        .await
        .unwrap()
        .value
        .unwrap();
    let bid = Bid::deserialize(&mut &b.data.as_ref().unwrap().data[..]).unwrap();
    assert_eq!(bid.amount, amount);
    assert_eq!(bid.proposal, pid);
    assert_eq!(bid.bidder, bidder.pubkey());

    // USDC moved into the vault.
    assert_eq!(token_amount(&mut rpc, vault_pda).await, amount);
    assert_eq!(token_amount(&mut rpc, bidder_token).await, 1_000_000 - amount);

    // Auction leader + counters.
    let auction = get_auction(&mut rpc, auction_pda).await;
    assert_eq!(auction.winner_proposal, pid);
    assert_eq!(auction.winner_amount, amount);
    assert_eq!(auction.total_staked, amount);
    assert_eq!(auction.proposal_count, 1);
}

// ---- helpers ----

async fn create_mint(rpc: &mut LightProgramTest, payer: &Keypair) -> Pubkey {
    let mint = Keypair::new();
    let rent = rpc.get_minimum_balance_for_rent_exemption(82).await.unwrap();
    let create = system_instruction::create_account(
        &payer.pubkey(),
        &mint.pubkey(),
        rent,
        82,
        &spl_token::ID,
    );
    let init = spl_token::instruction::initialize_mint(
        &spl_token::ID,
        &mint.pubkey(),
        &payer.pubkey(), // mint authority
        None,
        6,
    )
    .unwrap();
    rpc.create_and_send_transaction(&[create, init], &payer.pubkey(), &[payer, &mint])
        .await
        .unwrap();
    mint.pubkey()
}

async fn funded_token_account(
    rpc: &mut LightProgramTest,
    payer: &Keypair,
    mint: Pubkey,
    owner: &Pubkey,
    amount: u64,
) -> Pubkey {
    let acc = Keypair::new();
    let rent = rpc.get_minimum_balance_for_rent_exemption(165).await.unwrap();
    let create = system_instruction::create_account(
        &payer.pubkey(),
        &acc.pubkey(),
        rent,
        165,
        &spl_token::ID,
    );
    let init =
        spl_token::instruction::initialize_account3(&spl_token::ID, &acc.pubkey(), &mint, owner)
            .unwrap();
    let mint_to = spl_token::instruction::mint_to(
        &spl_token::ID,
        &mint,
        &acc.pubkey(),
        &payer.pubkey(), // mint authority
        &[],
        amount,
    )
    .unwrap();
    rpc.create_and_send_transaction(&[create, init, mint_to], &payer.pubkey(), &[payer, &acc])
        .await
        .unwrap();
    acc.pubkey()
}

async fn initialize_config(
    rpc: &mut LightProgramTest,
    owner: &Keypair,
    config_pda: Pubkey,
    mint: Pubkey,
) {
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
            usdc_mint: mint,
        }
        .data(),
    };
    rpc.create_and_send_transaction(&[ix], &owner.pubkey(), &[owner])
        .await
        .unwrap();
}

#[allow(clippy::too_many_arguments)]
async fn create_auction(
    rpc: &mut LightProgramTest,
    payer: &Keypair,
    creator: &Keypair,
    config_pda: Pubkey,
    auction_pda: Pubkey,
    vault_pda: Pubkey,
    mint: Pubkey,
    id: u64,
    min_bid: u64,
) {
    let ix = Instruction {
        program_id: bidon_zk::ID,
        accounts: bidon_zk::accounts::CreateAuction {
            config: config_pda,
            auction: auction_pda,
            usdc_mint: mint,
            vault: vault_pda,
            creator: creator.pubkey(),
            payer: payer.pubkey(),
            token_program: spl_token::ID,
            system_program: system_program::ID,
        }
        .to_account_metas(None),
        data: bidon_zk::instruction::CreateAuction {
            id,
            min_bid,
            duration_secs: 3600,
        }
        .data(),
    };
    rpc.create_and_send_transaction(&[ix], &payer.pubkey(), &[payer, creator])
        .await
        .unwrap();
}

async fn get_auction(rpc: &mut LightProgramTest, pda: Pubkey) -> Auction {
    let acc = rpc.get_account(pda).await.unwrap().unwrap();
    Auction::try_deserialize(&mut acc.data.as_slice()).unwrap()
}

/// SPL token account amount lives at offset 64 (mint 32 + owner 32).
async fn token_amount(rpc: &mut LightProgramTest, pda: Pubkey) -> u64 {
    let acc = rpc.get_account(pda).await.unwrap().unwrap();
    u64::from_le_bytes(acc.data[64..72].try_into().unwrap())
}
