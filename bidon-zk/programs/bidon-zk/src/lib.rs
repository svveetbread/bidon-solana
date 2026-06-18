#![allow(unexpected_cfgs)]
#![allow(deprecated)]

use anchor_lang::prelude::*;
use light_sdk::{
    account::LightAccount,
    address::v2::derive_address,
    cpi::{v2::CpiAccounts, CpiSigner},
    derive_light_cpi_signer,
    instruction::{account_meta::CompressedAccountMeta, PackedAddressTreeInfo, ValidityProof},
    LightDiscriminator, LightHasher, PackedAddressTreeInfoExt,
};
use light_sdk::constants::ADDRESS_TREE_V2;

declare_id!("6mS4dhHdapQbfiAj9U8k6W9eJAshdA2SRjEi2tXuuAvx");

pub const LIGHT_CPI_SIGNER: CpiSigner =
    derive_light_cpi_signer!("6mS4dhHdapQbfiAj9U8k6W9eJAshdA2SRjEi2tXuuAvx");

// Spike core: a rent-free compressed Bid (per proposal_id, bidder).
// create (place_bid) -> update (raise_bid, own top-up, no contention)
// -> close (withdraw_bid, loser refund; closing the compressed account is the
// double-refund guard). USDC vault / leader / end_time gates land in later layers.
#[program]
pub mod bidon_zk {

    use super::*;
    use light_sdk::cpi::{v2::LightSystemProgramCpi, InvokeLightSystemProgram, LightCpiInstruction};

    /// Place a new bid on a proposal: creates a rent-free compressed Bid account.
    pub fn place_bid<'info>(
        ctx: Context<'_, '_, '_, 'info, GenericAnchorAccounts<'info>>,
        proof: ValidityProof,
        address_tree_info: PackedAddressTreeInfo,
        output_state_tree_index: u8,
        proposal: u64,
        amount: u64,
    ) -> Result<()> {
        require!(amount > 0, BidonError::InvalidAmount);

        let light_cpi_accounts = CpiAccounts::new(
            ctx.accounts.signer.as_ref(),
            ctx.remaining_accounts,
            crate::LIGHT_CPI_SIGNER,
        );

        let address_tree_pubkey = address_tree_info
            .get_tree_pubkey(&light_cpi_accounts)
            .map_err(|_| ErrorCode::AccountNotEnoughKeys)?;

        if address_tree_pubkey.to_bytes() != ADDRESS_TREE_V2 {
            msg!("Invalid address tree");
            return Err(ProgramError::InvalidAccountData.into());
        }

        let proposal_le = proposal.to_le_bytes();
        let (address, address_seed) = derive_address(
            &[b"bid", proposal_le.as_ref(), ctx.accounts.signer.key().as_ref()],
            &address_tree_pubkey,
            &crate::ID,
        );

        let new_address_params =
            address_tree_info.into_new_address_params_assigned_packed(address_seed, Some(0));

        let mut bid =
            LightAccount::<Bid>::new_init(&crate::ID, Some(address), output_state_tree_index);

        bid.bidder = ctx.accounts.signer.key();
        bid.proposal = proposal;
        bid.amount = amount;

        LightSystemProgramCpi::new_cpi(LIGHT_CPI_SIGNER, proof)
            .with_light_account(bid)?
            .with_new_addresses(&[new_address_params])
            .invoke(light_cpi_accounts)?;

        Ok(())
    }

    /// Raise an existing bid (top-up own position): updates the compressed Bid.
    pub fn raise_bid<'info>(
        ctx: Context<'_, '_, '_, 'info, GenericAnchorAccounts<'info>>,
        proof: ValidityProof,
        account_meta: CompressedAccountMeta,
        proposal: u64,
        current_amount: u64,
        add_amount: u64,
    ) -> Result<()> {
        require!(add_amount > 0, BidonError::InvalidAmount);

        // new_mut hashes the supplied current state as input; it must match on-chain data exactly.
        let mut bid = LightAccount::<Bid>::new_mut(
            &crate::ID,
            &account_meta,
            Bid {
                bidder: ctx.accounts.signer.key(),
                proposal,
                amount: current_amount,
            },
        )?;

        bid.amount = bid.amount.checked_add(add_amount).ok_or(BidonError::Overflow)?;

        let light_cpi_accounts = CpiAccounts::new(
            ctx.accounts.signer.as_ref(),
            ctx.remaining_accounts,
            crate::LIGHT_CPI_SIGNER,
        );

        LightSystemProgramCpi::new_cpi(LIGHT_CPI_SIGNER, proof)
            .with_light_account(bid)?
            .invoke(light_cpi_accounts)?;

        Ok(())
    }

    /// Withdraw a losing bid: closes the compressed Bid. A closed compressed
    /// account cannot be reused -> double-refund guard.
    pub fn withdraw_bid<'info>(
        ctx: Context<'_, '_, '_, 'info, GenericAnchorAccounts<'info>>,
        proof: ValidityProof,
        account_meta: CompressedAccountMeta,
        proposal: u64,
        current_amount: u64,
    ) -> Result<()> {
        let bid = LightAccount::<Bid>::new_close(
            &crate::ID,
            &account_meta,
            Bid {
                bidder: ctx.accounts.signer.key(),
                proposal,
                amount: current_amount,
            },
        )?;

        let light_cpi_accounts = CpiAccounts::new(
            ctx.accounts.signer.as_ref(),
            ctx.remaining_accounts,
            crate::LIGHT_CPI_SIGNER,
        );

        LightSystemProgramCpi::new_cpi(LIGHT_CPI_SIGNER, proof)
            .with_light_account(bid)?
            .invoke(light_cpi_accounts)?;

        Ok(())
    }

    /// Create a per-proposal compressed aggregate (running total + content hash).
    /// In the full model this is created together with the first Bid inside place_bid;
    /// kept standalone here to validate concurrency on the hot per-proposal account.
    /// content_hash is the off-chain text hashed client-side (32 bytes, fixed size —
    /// cost does not depend on text length).
    pub fn create_proposal<'info>(
        ctx: Context<'_, '_, '_, 'info, GenericAnchorAccounts<'info>>,
        proof: ValidityProof,
        address_tree_info: PackedAddressTreeInfo,
        output_state_tree_index: u8,
        proposal_id: u64,
        content_hash: [u8; 32],
        amount: u64,
    ) -> Result<()> {
        require!(amount > 0, BidonError::InvalidAmount);

        let light_cpi_accounts = CpiAccounts::new(
            ctx.accounts.signer.as_ref(),
            ctx.remaining_accounts,
            crate::LIGHT_CPI_SIGNER,
        );

        let address_tree_pubkey = address_tree_info
            .get_tree_pubkey(&light_cpi_accounts)
            .map_err(|_| ErrorCode::AccountNotEnoughKeys)?;
        if address_tree_pubkey.to_bytes() != ADDRESS_TREE_V2 {
            msg!("Invalid address tree");
            return Err(ProgramError::InvalidAccountData.into());
        }

        let proposal_le = proposal_id.to_le_bytes();
        let (address, address_seed) = derive_address(
            &[b"proposal", proposal_le.as_ref()],
            &address_tree_pubkey,
            &crate::ID,
        );
        let new_address_params =
            address_tree_info.into_new_address_params_assigned_packed(address_seed, Some(0));

        let mut proposal = LightAccount::<ProposalTotal>::new_init(
            &crate::ID,
            Some(address),
            output_state_tree_index,
        );
        proposal.creator = ctx.accounts.signer.key();
        proposal.content_hash = content_hash;
        proposal.total = amount;

        LightSystemProgramCpi::new_cpi(LIGHT_CPI_SIGNER, proof)
            .with_light_account(proposal)?
            .with_new_addresses(&[new_address_params])
            .invoke(light_cpi_accounts)?;

        Ok(())
    }

    /// Add to a proposal's running total (every bid on this proposal updates it).
    /// Concurrent adds to the SAME proposal contend: a validity proof is only valid
    /// against the tree root it was fetched for. Once one add consumes the account
    /// hash (nullifier model), a second add carrying the stale proof is rejected,
    /// and the client must retry with a fresh proof. This is latency on a viral
    /// proposal, not a cap.
    pub fn add_to_proposal<'info>(
        ctx: Context<'_, '_, '_, 'info, GenericAnchorAccounts<'info>>,
        proof: ValidityProof,
        account_meta: CompressedAccountMeta,
        content_hash: [u8; 32],
        creator: Pubkey,
        current_total: u64,
        amount: u64,
    ) -> Result<()> {
        require!(amount > 0, BidonError::InvalidAmount);

        let mut proposal = LightAccount::<ProposalTotal>::new_mut(
            &crate::ID,
            &account_meta,
            ProposalTotal {
                creator,
                content_hash,
                total: current_total,
            },
        )?;
        proposal.total = proposal
            .total
            .checked_add(amount)
            .ok_or(BidonError::Overflow)?;

        let light_cpi_accounts = CpiAccounts::new(
            ctx.accounts.signer.as_ref(),
            ctx.remaining_accounts,
            crate::LIGHT_CPI_SIGNER,
        );
        LightSystemProgramCpi::new_cpi(LIGHT_CPI_SIGNER, proof)
            .with_light_account(proposal)?
            .invoke(light_cpi_accounts)?;

        Ok(())
    }

    /// Open a NEW proposal together with its first bid in ONE transaction: creates
    /// BOTH compressed accounts (ProposalTotal + Bid) under a SINGLE combined validity
    /// proof. This is the core of the full place_bid (which will also transfer USDC to
    /// the vault and bump the Auction leader). De-risks CU and tx size for two
    /// compressed creates sharing one proof.
    #[allow(clippy::too_many_arguments)]
    pub fn place_first_bid<'info>(
        ctx: Context<'_, '_, '_, 'info, GenericAnchorAccounts<'info>>,
        proof: ValidityProof,
        proposal_address_tree_info: PackedAddressTreeInfo,
        bid_address_tree_info: PackedAddressTreeInfo,
        output_state_tree_index: u8,
        proposal_id: u64,
        content_hash: [u8; 32],
        amount: u64,
    ) -> Result<()> {
        require!(amount > 0, BidonError::InvalidAmount);

        let light_cpi_accounts = CpiAccounts::new(
            ctx.accounts.signer.as_ref(),
            ctx.remaining_accounts,
            crate::LIGHT_CPI_SIGNER,
        );

        // Both addresses live in the same V2 address tree; validate once.
        let address_tree_pubkey = proposal_address_tree_info
            .get_tree_pubkey(&light_cpi_accounts)
            .map_err(|_| ErrorCode::AccountNotEnoughKeys)?;
        if address_tree_pubkey.to_bytes() != ADDRESS_TREE_V2 {
            msg!("Invalid address tree");
            return Err(ProgramError::InvalidAccountData.into());
        }

        let pid_le = proposal_id.to_le_bytes();
        let (proposal_address, proposal_seed) =
            derive_address(&[b"proposal", pid_le.as_ref()], &address_tree_pubkey, &crate::ID);
        let (bid_address, bid_seed) = derive_address(
            &[b"bid", pid_le.as_ref(), ctx.accounts.signer.key().as_ref()],
            &address_tree_pubkey,
            &crate::ID,
        );

        // Assign each new address to its LightAccount by index (0 = proposal, 1 = bid).
        let proposal_params =
            proposal_address_tree_info.into_new_address_params_assigned_packed(proposal_seed, Some(0));
        let bid_params =
            bid_address_tree_info.into_new_address_params_assigned_packed(bid_seed, Some(1));

        let mut proposal = LightAccount::<ProposalTotal>::new_init(
            &crate::ID,
            Some(proposal_address),
            output_state_tree_index,
        );
        proposal.creator = ctx.accounts.signer.key();
        proposal.content_hash = content_hash;
        proposal.total = amount;

        let mut bid =
            LightAccount::<Bid>::new_init(&crate::ID, Some(bid_address), output_state_tree_index);
        bid.bidder = ctx.accounts.signer.key();
        bid.proposal = proposal_id;
        bid.amount = amount;

        LightSystemProgramCpi::new_cpi(LIGHT_CPI_SIGNER, proof)
            .with_light_account(proposal)?
            .with_light_account(bid)?
            .with_new_addresses(&[proposal_params, bid_params])
            .invoke(light_cpi_accounts)?;

        Ok(())
    }
}

#[error_code]
pub enum BidonError {
    #[msg("Bid amount must be greater than zero")]
    InvalidAmount,
    #[msg("Bid amount overflow")]
    Overflow,
}

#[derive(Accounts)]
pub struct GenericAnchorAccounts<'info> {
    #[account(mut)]
    pub signer: Signer<'info>,
}

// #[event] makes the struct part of the Anchor IDL for client decoding.
#[event]
#[derive(Clone, Debug, Default, LightDiscriminator, LightHasher)]
pub struct Bid {
    #[hash]
    pub bidder: Pubkey,
    pub proposal: u64,
    pub amount: u64,
}

// Hot per-proposal aggregate. content_hash anchors the off-chain text (32 bytes,
// fixed regardless of text length); total is the running parimutuel sum.
#[event]
#[derive(Clone, Debug, Default, LightDiscriminator, LightHasher)]
pub struct ProposalTotal {
    #[hash]
    pub creator: Pubkey,
    #[hash]
    pub content_hash: [u8; 32],
    pub total: u64,
}
