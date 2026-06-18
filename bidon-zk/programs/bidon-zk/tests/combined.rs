#![cfg(feature = "test-sbf")]

use anchor_lang::{AnchorDeserialize, InstructionData, ToAccountMetas};
use bidon_zk::{Bid, ProposalTotal};
use light_program_test::{
    program_test::LightProgramTest, utils::simulate_cu, AddressWithTree, Indexer, ProgramTestConfig,
    Rpc,
};
use light_sdk::{
    address::v2::derive_address,
    instruction::{PackedAccounts, SystemAccountMetaConfig},
};
use solana_sdk::{instruction::Instruction, signature::Signer, transaction::Transaction};

const CONTENT: [u8; 32] = [9u8; 32];

/// De-risk the full place_bid path: create BOTH compressed accounts (ProposalTotal
/// + Bid) in ONE transaction under a single combined validity proof. Measures CU and
/// tx wire size — the open question for the full place_bid (limit: 1232-byte packet,
/// ~200k CU default per ix). Verifies both accounts land.
#[tokio::test]
async fn test_place_first_bid_combined() {
    let config = ProgramTestConfig::new(true, Some(vec![("bidon_zk", bidon_zk::ID)]));
    let mut rpc = LightProgramTest::new(config).await.unwrap();
    let payer = rpc.get_payer().insecure_clone();

    let address_tree_info = rpc.get_address_tree_v2();
    let proposal_id: u64 = 3;
    let (proposal_addr, _) = derive_address(
        &[b"proposal", &proposal_id.to_le_bytes()],
        &address_tree_info.tree,
        &bidon_zk::ID,
    );
    let (bid_addr, _) = derive_address(
        &[b"bid", &proposal_id.to_le_bytes(), payer.pubkey().as_ref()],
        &address_tree_info.tree,
        &bidon_zk::ID,
    );

    // Combined proof over BOTH new addresses.
    let mut remaining_accounts = PackedAccounts::default();
    remaining_accounts
        .add_system_accounts_v2(SystemAccountMetaConfig::new(bidon_zk::ID))
        .unwrap();
    let rpc_result = rpc
        .get_validity_proof(
            vec![],
            vec![
                AddressWithTree {
                    tree: address_tree_info.tree,
                    address: proposal_addr,
                },
                AddressWithTree {
                    tree: address_tree_info.tree,
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
        .pack_output_tree_index(&mut remaining_accounts)
        .unwrap();
    let packed = rpc_result.pack_tree_infos(&mut remaining_accounts);
    let proposal_address_tree_info = packed.address_trees[0];
    let bid_address_tree_info = packed.address_trees[1];

    let data = bidon_zk::instruction::PlaceFirstBid {
        proof: rpc_result.proof,
        proposal_address_tree_info,
        bid_address_tree_info,
        output_state_tree_index,
        proposal_id,
        content_hash: CONTENT,
        amount: 100,
    }
    .data();
    let accounts = bidon_zk::accounts::GenericAnchorAccounts {
        signer: payer.pubkey(),
    };
    let (remaining_metas, _, _) = remaining_accounts.to_account_metas();
    let ix = Instruction {
        program_id: bidon_zk::ID,
        accounts: [accounts.to_account_metas(Some(true)), remaining_metas].concat(),
        data,
    };

    // Measure CU and tx wire size before sending.
    let cu = simulate_cu(&mut rpc, &payer, &ix).await;
    let blockhash = rpc.get_latest_blockhash().await.unwrap().0;
    let tx = Transaction::new_signed_with_payer(
        std::slice::from_ref(&ix),
        Some(&payer.pubkey()),
        &[&payer],
        blockhash,
    );
    // Legacy tx wire size: compact-u16 sig count (1 byte for <128) + 64*sigs + message.
    let tx_size = 1 + tx.signatures.len() * 64 + tx.message.serialize().len();
    println!(
        "COMBINED place_first_bid: CU={} tx_size={} bytes (packet limit 1232, CU default 200k/ix)",
        cu, tx_size
    );

    // Send and verify BOTH compressed accounts were created.
    rpc.create_and_send_transaction(&[ix], &payer.pubkey(), &[&payer])
        .await
        .unwrap();

    let p = rpc
        .get_compressed_account(proposal_addr, None)
        .await
        .unwrap()
        .value
        .unwrap();
    let proposal = ProposalTotal::deserialize(&mut &p.data.as_ref().unwrap().data[..]).unwrap();
    assert_eq!(proposal.total, 100);
    assert_eq!(proposal.creator, payer.pubkey());
    assert_eq!(proposal.content_hash, CONTENT);

    let b = rpc
        .get_compressed_account(bid_addr, None)
        .await
        .unwrap()
        .value
        .unwrap();
    let bid = Bid::deserialize(&mut &b.data.as_ref().unwrap().data[..]).unwrap();
    assert_eq!(bid.amount, 100);
    assert_eq!(bid.proposal, proposal_id);
    assert_eq!(bid.bidder, payer.pubkey());
}
