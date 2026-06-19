#![allow(dead_code, deprecated)]

use anchor_lang::{AccountDeserialize, AnchorDeserialize, InstructionData, ToAccountMetas};
use anchor_spl::token::spl_token;
use bidon_zk::{Auction, Bid, ProposalTotal};
use light_client::indexer::CompressedAccount;
use light_program_test::{
    program_test::LightProgramTest, AddressWithTree, Indexer, ProgramTestConfig, Rpc,
};
use light_sdk::{
    address::v2::derive_address,
    instruction::{account_meta::CompressedAccountMeta, PackedAccounts, SystemAccountMetaConfig},
};
use solana_sdk::{
    clock::Clock,
    compute_budget::ComputeBudgetInstruction,
    instruction::Instruction,
    pubkey::Pubkey,
    signature::{Keypair, Signer},
    system_instruction, system_program,
};

pub const MIN_BID: u64 = 100_000; // 0.1 USDC (6 decimals)

/// A live environment: Config initialized, USDC mint created, one auction + vault live.
pub struct Ctx {
    pub payer: Keypair, // relayer: fee payer + rent payer + mint authority
    pub owner: Keypair,
    pub creator: Keypair,
    pub mint: Pubkey,
    pub config_pda: Pubkey,
    pub auction_pda: Pubkey,
    pub vault_pda: Pubkey,
}

pub async fn new_rpc() -> LightProgramTest {
    let cfg = ProgramTestConfig::new(true, Some(vec![("bidon_zk", bidon_zk::ID)]));
    LightProgramTest::new(cfg).await.unwrap()
}

pub async fn setup(rpc: &mut LightProgramTest, min_bid: u64) -> Ctx {
    let payer = rpc.get_payer().insecure_clone();
    let owner = Keypair::new();
    rpc.airdrop_lamports(&owner.pubkey(), 5_000_000_000)
        .await
        .unwrap();

    let mint = create_mint(rpc, &payer).await;
    let (config_pda, _) = Pubkey::find_program_address(&[b"config"], &bidon_zk::ID);
    initialize_config(rpc, &owner, config_pda, mint).await;

    let creator = Keypair::new();
    let id = 0u64;
    let (auction_pda, _) =
        Pubkey::find_program_address(&[b"auction", &id.to_le_bytes()], &bidon_zk::ID);
    let (vault_pda, _) =
        Pubkey::find_program_address(&[b"vault", auction_pda.as_ref()], &bidon_zk::ID);
    create_auction(
        rpc, &payer, &creator, config_pda, auction_pda, vault_pda, mint, id, min_bid,
    )
    .await;

    Ctx {
        payer,
        owner,
        creator,
        mint,
        config_pda,
        auction_pda,
        vault_pda,
    }
}

pub async fn funded_bidder(rpc: &mut LightProgramTest, ctx: &Ctx, amount: u64) -> (Keypair, Pubkey) {
    let bidder = Keypair::new();
    let token = funded_token_account(rpc, &ctx.payer, ctx.mint, &bidder.pubkey(), amount).await;
    (bidder, token)
}

// ---- address derivation ----

pub fn proposal_address(rpc: &mut LightProgramTest, auction: Pubkey, pid: u64) -> [u8; 32] {
    derive_address(
        &[b"proposal", auction.as_ref(), &pid.to_le_bytes()],
        &rpc.get_address_tree_v2().tree,
        &bidon_zk::ID,
    )
    .0
}

pub fn bid_address(
    rpc: &mut LightProgramTest,
    auction: Pubkey,
    pid: u64,
    bidder: Pubkey,
) -> [u8; 32] {
    derive_address(
        &[b"bid", auction.as_ref(), &pid.to_le_bytes(), bidder.as_ref()],
        &rpc.get_address_tree_v2().tree,
        &bidon_zk::ID,
    )
    .0
}

// ---- bid actions (build + send) ----

/// place_bid on a NEW proposal. Returns (proposal_addr, bid_addr).
pub async fn do_place_bid(
    rpc: &mut LightProgramTest,
    ctx: &Ctx,
    bidder: &Keypair,
    bidder_token: Pubkey,
    pid: u64,
    content_hash: [u8; 32],
    amount: u64,
) -> ([u8; 32], [u8; 32]) {
    let address_tree = rpc.get_address_tree_v2().tree;
    let p_addr = proposal_address(rpc, ctx.auction_pda, pid);
    let b_addr = bid_address(rpc, ctx.auction_pda, pid, bidder.pubkey());

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
                    address: p_addr,
                },
                AddressWithTree {
                    tree: address_tree,
                    address: b_addr,
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
        content_hash,
        amount,
    }
    .data();
    let (rem, _, _) = remaining.to_account_metas();
    send_bid_ix(rpc, ctx, bidder, bidder_token, rem, data).await;
    (p_addr, b_addr)
}

/// raise_bid as a NEW backer on an EXISTING proposal. Returns bid_addr.
pub async fn do_raise_bid(
    rpc: &mut LightProgramTest,
    ctx: &Ctx,
    bidder: &Keypair,
    bidder_token: Pubkey,
    pid: u64,
    amount: u64,
) -> [u8; 32] {
    let address_tree = rpc.get_address_tree_v2().tree;
    let p_addr = proposal_address(rpc, ctx.auction_pda, pid);
    let b_addr = bid_address(rpc, ctx.auction_pda, pid, bidder.pubkey());
    let p_acc = compressed(rpc, p_addr).await;
    let p_state = decode::<ProposalTotal>(&p_acc);

    let mut remaining = PackedAccounts::default();
    remaining
        .add_system_accounts_v2(SystemAccountMetaConfig::new(bidon_zk::ID))
        .unwrap();
    let rpc_result = rpc
        .get_validity_proof(
            vec![p_acc.hash],
            vec![AddressWithTree {
                tree: address_tree,
                address: b_addr,
            }],
            None,
        )
        .await
        .unwrap()
        .value;
    let packed = rpc_result.pack_tree_infos(&mut remaining);
    let state = packed.state_trees.unwrap();
    // Proposal update output and the new Bid output must share ONE state tree.
    let output_state_tree_index = state.output_tree_index;
    let proposal_meta = CompressedAccountMeta {
        tree_info: state.packed_tree_infos[0],
        address: p_acc.address.unwrap(),
        output_state_tree_index,
    };

    let data = bidon_zk::instruction::RaiseBid {
        proof: rpc_result.proof,
        proposal_id: pid,
        proposal_meta,
        proposal_creator: p_state.creator,
        proposal_content_hash: p_state.content_hash,
        proposal_current_total: p_state.total,
        bid_address_tree_info: packed.address_trees[0],
        output_state_tree_index,
        amount,
    }
    .data();
    let (rem, _, _) = remaining.to_account_metas();
    send_bid_ix(rpc, ctx, bidder, bidder_token, rem, data).await;
    b_addr
}

/// top_up_bid on an EXISTING own Bid.
pub async fn do_top_up_bid(
    rpc: &mut LightProgramTest,
    ctx: &Ctx,
    bidder: &Keypair,
    bidder_token: Pubkey,
    pid: u64,
    amount: u64,
) {
    let p_addr = proposal_address(rpc, ctx.auction_pda, pid);
    let b_addr = bid_address(rpc, ctx.auction_pda, pid, bidder.pubkey());
    let p_acc = compressed(rpc, p_addr).await;
    let b_acc = compressed(rpc, b_addr).await;
    let p_state = decode::<ProposalTotal>(&p_acc);
    let b_state = decode::<Bid>(&b_acc);

    let mut remaining = PackedAccounts::default();
    remaining
        .add_system_accounts_v2(SystemAccountMetaConfig::new(bidon_zk::ID))
        .unwrap();
    let rpc_result = rpc
        .get_validity_proof(vec![p_acc.hash, b_acc.hash], vec![], None)
        .await
        .unwrap()
        .value;
    let packed = rpc_result.pack_tree_infos(&mut remaining);
    let state = packed.state_trees.unwrap();
    let proposal_meta = CompressedAccountMeta {
        tree_info: state.packed_tree_infos[0],
        address: p_acc.address.unwrap(),
        output_state_tree_index: state.output_tree_index,
    };
    let bid_meta = CompressedAccountMeta {
        tree_info: state.packed_tree_infos[1],
        address: b_acc.address.unwrap(),
        output_state_tree_index: state.output_tree_index,
    };

    let data = bidon_zk::instruction::TopUpBid {
        proof: rpc_result.proof,
        proposal_id: pid,
        proposal_meta,
        proposal_creator: p_state.creator,
        proposal_content_hash: p_state.content_hash,
        proposal_current_total: p_state.total,
        bid_meta,
        bid_current_amount: b_state.amount,
        amount,
    }
    .data();
    let (rem, _, _) = remaining.to_account_metas();
    send_bid_ix(rpc, ctx, bidder, bidder_token, rem, data).await;
}

/// Named accounts for place_bid / raise_bid / top_up_bid (identical layouts).
fn bid_accounts(
    ctx: &Ctx,
    bidder: Pubkey,
    bidder_token: Pubkey,
) -> Vec<solana_sdk::instruction::AccountMeta> {
    bidon_zk::accounts::RaiseBid {
        config: ctx.config_pda,
        auction: ctx.auction_pda,
        vault: ctx.vault_pda,
        usdc_mint: ctx.mint,
        bidder_token,
        bidder,
        payer: ctx.payer.pubkey(),
        token_program: spl_token::ID,
    }
    .to_account_metas(None)
}

/// Assemble (named accounts + Light remaining accounts) and send with a ComputeBudget
/// bump (place/raise need ~190k CU). bidder + relayer (payer) co-sign.
async fn send_bid_ix(
    rpc: &mut LightProgramTest,
    ctx: &Ctx,
    bidder: &Keypair,
    bidder_token: Pubkey,
    remaining_metas: Vec<solana_sdk::instruction::AccountMeta>,
    data: Vec<u8>,
) {
    let mut accounts = bid_accounts(ctx, bidder.pubkey(), bidder_token);
    accounts.extend(remaining_metas);
    let ix = Instruction {
        program_id: bidon_zk::ID,
        accounts,
        data,
    };
    let cu = ComputeBudgetInstruction::set_compute_unit_limit(400_000);
    rpc.create_and_send_transaction(&[cu, ix], &ctx.payer.pubkey(), &[&ctx.payer, bidder])
        .await
        .unwrap();
}

// ---- regular-account setup helpers ----

pub async fn create_mint(rpc: &mut LightProgramTest, payer: &Keypair) -> Pubkey {
    let mint = Keypair::new();
    let rent = rpc.get_minimum_balance_for_rent_exemption(82).await.unwrap();
    let create =
        system_instruction::create_account(&payer.pubkey(), &mint.pubkey(), rent, 82, &spl_token::ID);
    let init = spl_token::instruction::initialize_mint(
        &spl_token::ID,
        &mint.pubkey(),
        &payer.pubkey(),
        None,
        6,
    )
    .unwrap();
    rpc.create_and_send_transaction(&[create, init], &payer.pubkey(), &[payer, &mint])
        .await
        .unwrap();
    mint.pubkey()
}

pub async fn funded_token_account(
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

pub async fn initialize_config(
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
pub async fn create_auction(
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

// ---- reads ----

pub async fn get_auction(rpc: &mut LightProgramTest, pda: Pubkey) -> Auction {
    let acc = rpc.get_account(pda).await.unwrap().unwrap();
    Auction::try_deserialize(&mut acc.data.as_slice()).unwrap()
}

/// SPL token account amount is at offset 64 (mint 32 + owner 32).
pub async fn token_amount(rpc: &mut LightProgramTest, pda: Pubkey) -> u64 {
    let acc = rpc.get_account(pda).await.unwrap().unwrap();
    u64::from_le_bytes(acc.data[64..72].try_into().unwrap())
}

pub async fn compressed(rpc: &mut LightProgramTest, address: [u8; 32]) -> CompressedAccount {
    rpc.get_compressed_account(address, None)
        .await
        .unwrap()
        .value
        .unwrap()
}

pub fn decode<T: AnchorDeserialize>(acc: &CompressedAccount) -> T {
    T::deserialize(&mut &acc.data.as_ref().unwrap().data[..]).unwrap()
}

pub async fn proposal_total(rpc: &mut LightProgramTest, address: [u8; 32]) -> ProposalTotal {
    decode(&compressed(rpc, address).await)
}

pub async fn bid_state(rpc: &mut LightProgramTest, address: [u8; 32]) -> Bid {
    decode(&compressed(rpc, address).await)
}

/// Advance the Clock's unix_timestamp by `secs` (to cross end_time gates).
pub fn warp_past(rpc: &mut LightProgramTest, secs: i64) {
    let mut clock = rpc.context.get_sysvar::<Clock>();
    clock.unix_timestamp += secs;
    rpc.context.set_sysvar(&clock);
}

/// Create an empty USDC token account owned by `owner`.
pub async fn token_account(
    rpc: &mut LightProgramTest,
    payer: &Keypair,
    mint: Pubkey,
    owner: &Pubkey,
) -> Pubkey {
    let acc = Keypair::new();
    let rent = rpc.get_minimum_balance_for_rent_exemption(165).await.unwrap();
    let create =
        system_instruction::create_account(&payer.pubkey(), &acc.pubkey(), rent, 165, &spl_token::ID);
    let init =
        spl_token::instruction::initialize_account3(&spl_token::ID, &acc.pubkey(), &mint, owner)
            .unwrap();
    rpc.create_and_send_transaction(&[create, init], &payer.pubkey(), &[payer, &acc])
        .await
        .unwrap();
    acc.pubkey()
}

/// claim_winnings (permissionless; relayer pays the tx fee).
pub async fn do_claim(
    rpc: &mut LightProgramTest,
    ctx: &Ctx,
    creator_token: Pubkey,
    fee_receiver_token: Pubkey,
) {
    let ix = Instruction {
        program_id: bidon_zk::ID,
        accounts: bidon_zk::accounts::ClaimWinnings {
            config: ctx.config_pda,
            auction: ctx.auction_pda,
            vault: ctx.vault_pda,
            creator_token,
            fee_receiver_token,
            usdc_mint: ctx.mint,
            token_program: spl_token::ID,
        }
        .to_account_metas(None),
        data: bidon_zk::instruction::ClaimWinnings {}.data(),
    };
    rpc.create_and_send_transaction(&[ix], &ctx.payer.pubkey(), &[&ctx.payer])
        .await
        .unwrap();
}

/// withdraw a losing bid (permissionless): refund USDC + close the compressed Bid.
pub async fn do_withdraw(
    rpc: &mut LightProgramTest,
    ctx: &Ctx,
    bidder: Pubkey,
    bidder_token: Pubkey,
    pid: u64,
    current_amount: u64,
) {
    let b_addr = bid_address(rpc, ctx.auction_pda, pid, bidder);
    let b_acc = compressed(rpc, b_addr).await;

    let mut remaining = PackedAccounts::default();
    remaining
        .add_system_accounts_v2(SystemAccountMetaConfig::new(bidon_zk::ID))
        .unwrap();
    let rpc_result = rpc
        .get_validity_proof(vec![b_acc.hash], vec![], None)
        .await
        .unwrap()
        .value;
    let packed = rpc_result.pack_tree_infos(&mut remaining);
    let state = packed.state_trees.unwrap();
    let bid_meta = CompressedAccountMeta {
        tree_info: state.packed_tree_infos[0],
        address: b_acc.address.unwrap(),
        output_state_tree_index: state.output_tree_index,
    };

    let data = bidon_zk::instruction::Withdraw {
        proof: rpc_result.proof,
        proposal_id: pid,
        bidder,
        bid_meta,
        bid_current_amount: current_amount,
    }
    .data();
    let mut metas = bidon_zk::accounts::Withdraw {
        config: ctx.config_pda,
        auction: ctx.auction_pda,
        vault: ctx.vault_pda,
        bidder_token,
        usdc_mint: ctx.mint,
        payer: ctx.payer.pubkey(),
        token_program: spl_token::ID,
    }
    .to_account_metas(None);
    let (rem, _, _) = remaining.to_account_metas();
    metas.extend(rem);

    let cu = ComputeBudgetInstruction::set_compute_unit_limit(400_000);
    let ix = Instruction {
        program_id: bidon_zk::ID,
        accounts: metas,
        data,
    };
    rpc.create_and_send_transaction(&[cu, ix], &ctx.payer.pubkey(), &[&ctx.payer])
        .await
        .unwrap();
}

/// close_auction (permissionless GC): close vault + Auction, rent -> relayer (rent_payer).
pub async fn do_close_auction(rpc: &mut LightProgramTest, ctx: &Ctx) {
    let ix = Instruction {
        program_id: bidon_zk::ID,
        accounts: bidon_zk::accounts::CloseAuction {
            auction: ctx.auction_pda,
            vault: ctx.vault_pda,
            rent_recipient: ctx.payer.pubkey(),
            token_program: spl_token::ID,
        }
        .to_account_metas(None),
        data: bidon_zk::instruction::CloseAuction {}.data(),
    };
    rpc.create_and_send_transaction(&[ix], &ctx.payer.pubkey(), &[&ctx.payer])
        .await
        .unwrap();
}
