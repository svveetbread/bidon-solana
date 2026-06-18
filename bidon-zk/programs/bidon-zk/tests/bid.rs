#![cfg(feature = "test-sbf")]

use anchor_lang::{AnchorDeserialize, InstructionData, ToAccountMetas};
use bidon_zk::Bid;
use light_client::indexer::{CompressedAccount, TreeInfo};
use light_program_test::{
    program_test::LightProgramTest, utils::simulate_cu, AddressWithTree, Indexer, ProgramTestConfig,
    Rpc,
};
use light_sdk::{
    address::v2::derive_address,
    instruction::{account_meta::CompressedAccountMeta, PackedAccounts, SystemAccountMetaConfig},
};
use solana_sdk::{
    instruction::Instruction,
    signature::{Keypair, Signer},
};

/// Spike: full lifecycle of a rent-free compressed Bid.
/// place (100) -> raise (+50 -> 150) -> raise (+25 -> 175) -> withdraw (close).
#[tokio::test]
async fn test_bid_lifecycle() {
    let config = ProgramTestConfig::new(true, Some(vec![("bidon_zk", bidon_zk::ID)]));
    let mut rpc = LightProgramTest::new(config).await.unwrap();
    let payer = rpc.get_payer().insecure_clone();

    let address_tree_info = rpc.get_address_tree_v2();
    let proposal: u64 = 7;

    let (address, _) = derive_address(
        &[b"bid", &proposal.to_le_bytes(), payer.pubkey().as_ref()],
        &address_tree_info.tree,
        &bidon_zk::ID,
    );

    // 1. Place a new bid of 100.
    let ix = build_place_bid_ix(&mut rpc, &payer, &address, address_tree_info, proposal, 100).await;
    send(&mut rpc, &payer, ix).await;
    let acc = compressed(&mut rpc, address).await;
    assert_eq!(acc.leaf_index, 0);
    let bid = decode(&acc);
    assert_eq!(bid.bidder, payer.pubkey());
    assert_eq!(bid.proposal, proposal);
    assert_eq!(bid.amount, 100);

    // 2. Raise own position by 50 -> 150.
    let ix = build_raise_bid_ix(&mut rpc, &payer, &acc, proposal, 50).await;
    send(&mut rpc, &payer, ix).await;
    let acc = compressed(&mut rpc, address).await;
    assert_eq!(acc.leaf_index, 1);
    assert_eq!(decode(&acc).amount, 150);

    // 3. Raise again by 25 -> 175.
    let ix = build_raise_bid_ix(&mut rpc, &payer, &acc, proposal, 25).await;
    send(&mut rpc, &payer, ix).await;
    let acc = compressed(&mut rpc, address).await;
    assert_eq!(acc.leaf_index, 2);
    assert_eq!(decode(&acc).amount, 175);

    // 4. Withdraw -> close. Closed compressed account holds default (zero) data.
    let ix = build_withdraw_bid_ix(&mut rpc, &payer, &acc, proposal).await;
    send(&mut rpc, &payer, ix).await;
    let acc = compressed(&mut rpc, address).await;
    assert_eq!(acc.data, Some(Default::default()));
}

/// Measure the real cost of each compressed-Bid action: compute units (simulate_cu)
/// and lamports actually spent by the payer (balance delta). Proves rent-free:
/// a regular ~80-byte account costs ~1_000_000+ lamports of *frozen* rent; a
/// compressed Bid only *spends* a few thousand lamports and freezes nothing.
#[tokio::test]
async fn test_bid_cost() {
    let config = ProgramTestConfig::new(true, Some(vec![("bidon_zk", bidon_zk::ID)]));
    let mut rpc = LightProgramTest::new(config).await.unwrap();
    let payer = rpc.get_payer().insecure_clone();

    let address_tree_info = rpc.get_address_tree_v2();
    let proposal: u64 = 42;
    let (address, _) = derive_address(
        &[b"bid", &proposal.to_le_bytes(), payer.pubkey().as_ref()],
        &address_tree_info.tree,
        &bidon_zk::ID,
    );

    // place_bid
    let before = rpc.get_balance(&payer.pubkey()).await.unwrap();
    let ix = build_place_bid_ix(&mut rpc, &payer, &address, address_tree_info, proposal, 100).await;
    let cu = simulate_cu(&mut rpc, &payer, &ix).await;
    send(&mut rpc, &payer, ix).await;
    let after = rpc.get_balance(&payer.pubkey()).await.unwrap();
    println!("COST place_bid    CU={:>7} lamports_spent={}", cu, before - after);

    // raise_bid
    let acc = compressed(&mut rpc, address).await;
    let before = rpc.get_balance(&payer.pubkey()).await.unwrap();
    let ix = build_raise_bid_ix(&mut rpc, &payer, &acc, proposal, 50).await;
    let cu = simulate_cu(&mut rpc, &payer, &ix).await;
    send(&mut rpc, &payer, ix).await;
    let after = rpc.get_balance(&payer.pubkey()).await.unwrap();
    println!("COST raise_bid    CU={:>7} lamports_spent={}", cu, before - after);

    // withdraw_bid (close)
    let acc = compressed(&mut rpc, address).await;
    let before = rpc.get_balance(&payer.pubkey()).await.unwrap();
    let ix = build_withdraw_bid_ix(&mut rpc, &payer, &acc, proposal).await;
    let cu = simulate_cu(&mut rpc, &payer, &ix).await;
    send(&mut rpc, &payer, ix).await;
    let after = rpc.get_balance(&payer.pubkey()).await.unwrap();
    println!("COST withdraw_bid CU={:>7} lamports_spent={}", cu, before - after);
}

// ---- helpers ----

async fn compressed<R: Rpc + Indexer>(rpc: &mut R, address: [u8; 32]) -> CompressedAccount {
    rpc.get_compressed_account(address, None)
        .await
        .unwrap()
        .value
        .unwrap()
}

fn decode(acc: &CompressedAccount) -> Bid {
    Bid::deserialize(&mut &acc.data.as_ref().unwrap().data[..]).unwrap()
}

async fn send<R: Rpc + Indexer>(rpc: &mut R, payer: &Keypair, ix: Instruction) {
    rpc.create_and_send_transaction(&[ix], &payer.pubkey(), &[payer])
        .await
        .unwrap();
}

async fn build_place_bid_ix<R: Rpc + Indexer>(
    rpc: &mut R,
    payer: &Keypair,
    address: &[u8; 32],
    address_tree_info: TreeInfo,
    proposal: u64,
    amount: u64,
) -> Instruction {
    let mut remaining_accounts = PackedAccounts::default();
    remaining_accounts
        .add_system_accounts_v2(SystemAccountMetaConfig::new(bidon_zk::ID))
        .unwrap();

    let rpc_result = rpc
        .get_validity_proof(
            vec![],
            vec![AddressWithTree {
                tree: address_tree_info.tree,
                address: *address,
            }],
            None,
        )
        .await
        .unwrap()
        .value;
    let output_state_tree_index = rpc
        .get_random_state_tree_info()
        .unwrap()
        .pack_output_tree_index(&mut remaining_accounts)
        .unwrap();
    let packed_address_tree_info = rpc_result
        .pack_tree_infos(&mut remaining_accounts)
        .address_trees[0];

    let data = bidon_zk::instruction::PlaceBid {
        proof: rpc_result.proof,
        address_tree_info: packed_address_tree_info,
        output_state_tree_index,
        proposal,
        amount,
    }
    .data();

    let accounts = bidon_zk::accounts::GenericAnchorAccounts {
        signer: payer.pubkey(),
    };
    let (remaining_metas, _, _) = remaining_accounts.to_account_metas();
    Instruction {
        program_id: bidon_zk::ID,
        accounts: [accounts.to_account_metas(Some(true)), remaining_metas].concat(),
        data,
    }
}

async fn build_raise_bid_ix<R: Rpc + Indexer>(
    rpc: &mut R,
    payer: &Keypair,
    compressed_account: &CompressedAccount,
    proposal: u64,
    add_amount: u64,
) -> Instruction {
    let mut remaining_accounts = PackedAccounts::default();
    remaining_accounts
        .add_system_accounts_v2(SystemAccountMetaConfig::new(bidon_zk::ID))
        .unwrap();

    let rpc_result = rpc
        .get_validity_proof(vec![compressed_account.hash], vec![], None)
        .await
        .unwrap()
        .value;
    let packed = rpc_result
        .pack_tree_infos(&mut remaining_accounts)
        .state_trees
        .unwrap();

    let account_meta = CompressedAccountMeta {
        tree_info: packed.packed_tree_infos[0],
        address: compressed_account.address.unwrap(),
        output_state_tree_index: packed.output_tree_index,
    };

    let data = bidon_zk::instruction::RaiseBid {
        proof: rpc_result.proof,
        account_meta,
        proposal,
        current_amount: decode(compressed_account).amount,
        add_amount,
    }
    .data();

    let accounts = bidon_zk::accounts::GenericAnchorAccounts {
        signer: payer.pubkey(),
    };
    let (remaining_metas, _, _) = remaining_accounts.to_account_metas();
    Instruction {
        program_id: bidon_zk::ID,
        accounts: [accounts.to_account_metas(Some(true)), remaining_metas].concat(),
        data,
    }
}

async fn build_withdraw_bid_ix<R: Rpc + Indexer>(
    rpc: &mut R,
    payer: &Keypair,
    compressed_account: &CompressedAccount,
    proposal: u64,
) -> Instruction {
    let mut remaining_accounts = PackedAccounts::default();
    remaining_accounts
        .add_system_accounts_v2(SystemAccountMetaConfig::new(bidon_zk::ID))
        .unwrap();

    let rpc_result = rpc
        .get_validity_proof(vec![compressed_account.hash], vec![], None)
        .await
        .unwrap()
        .value;
    let packed = rpc_result
        .pack_tree_infos(&mut remaining_accounts)
        .state_trees
        .unwrap();

    let account_meta = CompressedAccountMeta {
        tree_info: packed.packed_tree_infos[0],
        address: compressed_account.address.unwrap(),
        output_state_tree_index: packed.output_tree_index,
    };

    let data = bidon_zk::instruction::WithdrawBid {
        proof: rpc_result.proof,
        account_meta,
        proposal,
        current_amount: decode(compressed_account).amount,
    }
    .data();

    let accounts = bidon_zk::accounts::GenericAnchorAccounts {
        signer: payer.pubkey(),
    };
    let (remaining_metas, _, _) = remaining_accounts.to_account_metas();
    Instruction {
        program_id: bidon_zk::ID,
        accounts: [accounts.to_account_metas(Some(true)), remaining_metas].concat(),
        data,
    }
}
