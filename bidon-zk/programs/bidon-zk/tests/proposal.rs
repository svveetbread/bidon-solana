#![cfg(feature = "test-sbf")]

use anchor_lang::{AnchorDeserialize, InstructionData, ToAccountMetas};
use bidon_zk::ProposalTotal;
use light_client::indexer::{CompressedAccount, TreeInfo};
use light_program_test::{
    program_test::LightProgramTest, AddressWithTree, Indexer, ProgramTestConfig, Rpc,
};
use light_sdk::{
    address::v2::derive_address,
    instruction::{account_meta::CompressedAccountMeta, PackedAccounts, SystemAccountMetaConfig},
};
use solana_sdk::{
    instruction::Instruction,
    signature::{Keypair, Signer},
};

const CONTENT: [u8; 32] = [7u8; 32];

/// Validates the core concurrency risk of the per-proposal compressed aggregate.
/// Two adds to the SAME proposal are built against the SAME tree root (both see
/// total=100). The first add consumes the account hash (nullifier) and advances
/// the tree. The second add still carries the stale proof against the now-spent
/// hash and MUST be rejected on-chain. A client retry with a fresh proof then
/// succeeds. This is latency on a viral proposal, not a cap.
#[tokio::test]
async fn test_proposal_concurrency_nullifier() {
    let config = ProgramTestConfig::new(true, Some(vec![("bidon_zk", bidon_zk::ID)]));
    let mut rpc = LightProgramTest::new(config).await.unwrap();
    let payer = rpc.get_payer().insecure_clone();

    let address_tree_info = rpc.get_address_tree_v2();
    let proposal_id: u64 = 1;
    let (address, _) = derive_address(
        &[b"proposal", &proposal_id.to_le_bytes()],
        &address_tree_info.tree,
        &bidon_zk::ID,
    );

    // Create the proposal with running total = 100.
    let ix = build_create_proposal_ix(
        &mut rpc,
        &payer,
        &address,
        address_tree_info,
        proposal_id,
        CONTENT,
        100,
    )
    .await;
    send(&mut rpc, &payer, ix).await.unwrap();

    let acc0 = compressed(&mut rpc, address).await;
    assert_eq!(decode(&acc0).total, 100);

    // Two concurrent adds, both proven against the SAME root (both see total=100).
    let ix_a = build_add_ix(&mut rpc, &payer, &acc0, CONTENT, payer.pubkey(), 100, 50).await;
    let ix_b = build_add_ix(&mut rpc, &payer, &acc0, CONTENT, payer.pubkey(), 100, 30).await;

    // First add wins: total -> 150, acc0 hash consumed (nullifier), tree advances.
    send(&mut rpc, &payer, ix_a).await.unwrap();
    let acc1 = compressed(&mut rpc, address).await;
    assert_eq!(decode(&acc1).total, 150);

    // Second add carries the STALE proof (against the now-nullified hash). It MUST
    // be rejected on-chain — this is exactly the contention we accept by design.
    let res = send(&mut rpc, &payer, ix_b).await;
    assert!(
        res.is_err(),
        "stale-proof add must be rejected by the nullifier, but it succeeded"
    );

    // Client retries with a FRESH proof against the new root: total -> 180.
    let ix_b_retry = build_add_ix(&mut rpc, &payer, &acc1, CONTENT, payer.pubkey(), 150, 30).await;
    send(&mut rpc, &payer, ix_b_retry).await.unwrap();
    let acc2 = compressed(&mut rpc, address).await;
    assert_eq!(decode(&acc2).total, 180);
}

// ---- helpers ----

async fn compressed<R: Rpc + Indexer>(rpc: &mut R, address: [u8; 32]) -> CompressedAccount {
    rpc.get_compressed_account(address, None)
        .await
        .unwrap()
        .value
        .unwrap()
}

fn decode(acc: &CompressedAccount) -> ProposalTotal {
    ProposalTotal::deserialize(&mut &acc.data.as_ref().unwrap().data[..]).unwrap()
}

async fn send<R: Rpc + Indexer>(
    rpc: &mut R,
    payer: &Keypair,
    ix: Instruction,
) -> Result<solana_sdk::signature::Signature, light_program_test::RpcError> {
    rpc.create_and_send_transaction(&[ix], &payer.pubkey(), &[payer])
        .await
}

async fn build_create_proposal_ix<R: Rpc + Indexer>(
    rpc: &mut R,
    payer: &Keypair,
    address: &[u8; 32],
    address_tree_info: TreeInfo,
    proposal_id: u64,
    content_hash: [u8; 32],
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

    let data = bidon_zk::instruction::CreateProposal {
        proof: rpc_result.proof,
        address_tree_info: packed_address_tree_info,
        output_state_tree_index,
        proposal_id,
        content_hash,
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

#[allow(clippy::too_many_arguments)]
async fn build_add_ix<R: Rpc + Indexer>(
    rpc: &mut R,
    payer: &Keypair,
    compressed_account: &CompressedAccount,
    content_hash: [u8; 32],
    creator: solana_sdk::pubkey::Pubkey,
    current_total: u64,
    amount: u64,
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

    let data = bidon_zk::instruction::AddToProposal {
        proof: rpc_result.proof,
        account_meta,
        content_hash,
        creator,
        current_total,
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
