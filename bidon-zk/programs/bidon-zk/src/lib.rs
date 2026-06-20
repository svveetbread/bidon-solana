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

use anchor_spl::token::{
    close_account, transfer_checked, CloseAccount, Mint, Token, TokenAccount, TransferChecked,
};

declare_id!("4Pfc1jdDXX4EMFoe7FxNGMfQmSgZSegJn7DCHkxbnfXz");

pub const LIGHT_CPI_SIGNER: CpiSigner =
    derive_light_cpi_signer!("4Pfc1jdDXX4EMFoe7FxNGMfQmSgZSegJn7DCHkxbnfXz");

// ---- regular-account seeds & limits (Config / Auction / Vault) ----
pub const CONFIG_SEED: &[u8] = b"config";
pub const AUCTION_SEED: &[u8] = b"auction";
pub const VAULT_SEED: &[u8] = b"vault";
pub const PROPOSAL_SEED: &[u8] = b"proposal";
pub const BID_SEED: &[u8] = b"bid";
pub const MAX_FEE_BPS: u16 = 1000;

// bidon parimutuel auctions on ZK Compression. Hybrid model:
//  - Config / Auction / Vault: regular accounts (the hot global leader + the USDC pool).
//  - ProposalTotal / Bid: rent-free compressed accounts (per-proposal aggregate, per-user
//    position). All bid/raise/withdraw actions cost ~$0.001 spent / $0 frozen.
// Gates are time-based (now >= end_time); there is no finalize step.
#[program]
pub mod bidon_zk {

    use super::*;
    use light_sdk::cpi::{v2::LightSystemProgramCpi, InvokeLightSystemProgram, LightCpiInstruction};

    /// Initialize the singleton Config (fee <= 10%).
    pub fn initialize(
        ctx: Context<Initialize>,
        fee_bps: u16,
        fee_receiver: Pubkey,
        usdc_mint: Pubkey,
    ) -> Result<()> {
        require!(fee_bps <= MAX_FEE_BPS, BidonError::FeeTooHigh);
        let config = &mut ctx.accounts.config;
        config.owner = ctx.accounts.owner.key();
        config.fee_bps = fee_bps;
        config.fee_receiver = fee_receiver;
        config.usdc_mint = usdc_mint;
        config.auction_count = 0;
        config.bump = ctx.bumps.config;
        Ok(())
    }

    /// Owner-only fee update.
    pub fn set_config(ctx: Context<SetConfig>, fee_bps: u16, fee_receiver: Pubkey) -> Result<()> {
        require!(fee_bps <= MAX_FEE_BPS, BidonError::FeeTooHigh);
        let config = &mut ctx.accounts.config;
        config.fee_bps = fee_bps;
        config.fee_receiver = fee_receiver;
        Ok(())
    }

    /// Create an auction + its USDC vault — the ONLY rent in the system. The relayer
    /// (`payer`) fronts SOL for rent; the creator signs as authority with 0 SOL.
    /// No upper cap on duration (only > 0). Gates are time-based (finalize removed).
    pub fn create_auction(
        ctx: Context<CreateAuction>,
        id: u64,
        min_bid: u64,
        duration_secs: i64,
    ) -> Result<()> {
        require!(
            id == ctx.accounts.config.auction_count,
            BidonError::InvalidAuctionId
        );
        require!(duration_secs > 0, BidonError::InvalidDuration);

        let now = Clock::get()?.unix_timestamp;
        let fee_bps = ctx.accounts.config.fee_bps;

        let auction = &mut ctx.accounts.auction;
        auction.id = id;
        auction.creator = ctx.accounts.creator.key();
        auction.min_bid = min_bid;
        auction.fee_bps = fee_bps;
        auction.end_time = now
            .checked_add(duration_secs)
            .ok_or(BidonError::MathOverflow)?;
        auction.creator_paid = false;
        auction.total_staked = 0;
        auction.proposal_count = 0;
        auction.winner_proposal = 0;
        auction.winner_amount = 0;
        auction.rent_payer = ctx.accounts.payer.key();
        auction.bump = ctx.bumps.auction;

        ctx.accounts.config.auction_count = ctx
            .accounts
            .config
            .auction_count
            .checked_add(1)
            .ok_or(BidonError::MathOverflow)?;
        Ok(())
    }

    /// Place a bid on a NEW proposal (proposal_id == auction.proposal_count): pulls USDC
    /// into the vault, then creates BOTH compressed accounts — the per-proposal aggregate
    /// (ProposalTotal) and the bidder's position (Bid) — under a single combined proof,
    /// and bumps the auction leader. Gasless: `bidder` signs with 0 SOL, `payer` (relayer)
    /// is the Light fee payer. content_hash is the off-chain text hashed client-side.
    #[allow(clippy::too_many_arguments)]
    pub fn place_bid<'info>(
        ctx: Context<'_, '_, '_, 'info, PlaceBid<'info>>,
        proof: ValidityProof,
        proposal_address_tree_info: PackedAddressTreeInfo,
        bid_address_tree_info: PackedAddressTreeInfo,
        output_state_tree_index: u8,
        content_hash: [u8; 32],
        amount: u64,
    ) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        require!(
            now < ctx.accounts.auction.end_time,
            BidonError::AuctionEnded
        );
        require!(
            amount >= ctx.accounts.auction.min_bid,
            BidonError::BelowMinBid
        );

        let proposal_id = ctx.accounts.auction.proposal_count;
        let auction_key = ctx.accounts.auction.key();
        let bidder_key = ctx.accounts.bidder.key();

        // 1. Pull USDC from the bidder into the auction vault.
        transfer_to_vault(
            &ctx.accounts.token_program,
            &ctx.accounts.bidder_token,
            &ctx.accounts.usdc_mint,
            &ctx.accounts.vault,
            &ctx.accounts.bidder,
            amount,
        )?;

        // 2. Create ProposalTotal + Bid (compressed) under one combined proof.
        let light_cpi_accounts = CpiAccounts::new(
            ctx.accounts.payer.as_ref(),
            ctx.remaining_accounts,
            crate::LIGHT_CPI_SIGNER,
        );
        let address_tree_pubkey = proposal_address_tree_info
            .get_tree_pubkey(&light_cpi_accounts)
            .map_err(|_| ErrorCode::AccountNotEnoughKeys)?;
        if address_tree_pubkey.to_bytes() != ADDRESS_TREE_V2 {
            msg!("Invalid address tree");
            return Err(ProgramError::InvalidAccountData.into());
        }

        let pid_le = proposal_id.to_le_bytes();
        let (proposal_address, proposal_seed) = derive_address(
            &[PROPOSAL_SEED, auction_key.as_ref(), pid_le.as_ref()],
            &address_tree_pubkey,
            &crate::ID,
        );
        let (bid_address, bid_seed) = derive_address(
            &[BID_SEED, auction_key.as_ref(), pid_le.as_ref(), bidder_key.as_ref()],
            &address_tree_pubkey,
            &crate::ID,
        );
        let proposal_params =
            proposal_address_tree_info.into_new_address_params_assigned_packed(proposal_seed, Some(0));
        let bid_params =
            bid_address_tree_info.into_new_address_params_assigned_packed(bid_seed, Some(1));

        let mut proposal = LightAccount::<ProposalTotal>::new_init(
            &crate::ID,
            Some(proposal_address),
            output_state_tree_index,
        );
        proposal.creator = bidder_key;
        proposal.content_hash = content_hash;
        proposal.total = amount;

        let mut bid =
            LightAccount::<Bid>::new_init(&crate::ID, Some(bid_address), output_state_tree_index);
        bid.bidder = bidder_key;
        bid.proposal = proposal_id;
        bid.amount = amount;

        LightSystemProgramCpi::new_cpi(LIGHT_CPI_SIGNER, proof)
            .with_light_account(proposal)?
            .with_light_account(bid)?
            .with_new_addresses(&[proposal_params, bid_params])
            .invoke(light_cpi_accounts)?;

        // 3. Update auction totals + leader, advance the proposal counter.
        let auction = &mut ctx.accounts.auction;
        auction.total_staked = auction
            .total_staked
            .checked_add(amount)
            .ok_or(BidonError::MathOverflow)?;
        auction.update_leader(proposal_id, amount);
        auction.proposal_count = auction
            .proposal_count
            .checked_add(1)
            .ok_or(BidonError::MathOverflow)?;

        Ok(())
    }

    /// Bid on an EXISTING proposal as a NEW backer: pulls USDC, updates the proposal
    /// aggregate (ProposalTotal) and CREATES this backer's Bid, under one combined proof
    /// (1 inclusion + 1 new address), then bumps the leader.
    #[allow(clippy::too_many_arguments)]
    pub fn raise_bid<'info>(
        ctx: Context<'_, '_, '_, 'info, RaiseBid<'info>>,
        proof: ValidityProof,
        proposal_id: u64,
        proposal_meta: CompressedAccountMeta,
        proposal_creator: Pubkey,
        proposal_content_hash: [u8; 32],
        proposal_current_total: u64,
        bid_address_tree_info: PackedAddressTreeInfo,
        output_state_tree_index: u8,
        amount: u64,
    ) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        require!(
            now < ctx.accounts.auction.end_time,
            BidonError::AuctionEnded
        );
        require!(
            amount >= ctx.accounts.auction.min_bid,
            BidonError::BelowMinBid
        );

        let auction_key = ctx.accounts.auction.key();
        let bidder_key = ctx.accounts.bidder.key();

        transfer_to_vault(&ctx.accounts.token_program, &ctx.accounts.bidder_token, &ctx.accounts.usdc_mint, &ctx.accounts.vault, &ctx.accounts.bidder, amount)?;

        let light_cpi_accounts = CpiAccounts::new(
            ctx.accounts.payer.as_ref(),
            ctx.remaining_accounts,
            crate::LIGHT_CPI_SIGNER,
        );

        // Update the proposal aggregate (input/inclusion, LightAccount index 0).
        let mut proposal = LightAccount::<ProposalTotal>::new_mut(
            &crate::ID,
            &proposal_meta,
            ProposalTotal {
                creator: proposal_creator,
                content_hash: proposal_content_hash,
                total: proposal_current_total,
            },
        )?;
        let new_total = proposal
            .total
            .checked_add(amount)
            .ok_or(BidonError::Overflow)?;
        proposal.total = new_total;

        // Create the new Bid (new address, LightAccount index 1).
        let address_tree_pubkey = bid_address_tree_info
            .get_tree_pubkey(&light_cpi_accounts)
            .map_err(|_| ErrorCode::AccountNotEnoughKeys)?;
        if address_tree_pubkey.to_bytes() != ADDRESS_TREE_V2 {
            msg!("Invalid address tree");
            return Err(ProgramError::InvalidAccountData.into());
        }
        let pid_le = proposal_id.to_le_bytes();
        let (bid_address, bid_seed) = derive_address(
            &[BID_SEED, auction_key.as_ref(), pid_le.as_ref(), bidder_key.as_ref()],
            &address_tree_pubkey,
            &crate::ID,
        );
        let bid_params =
            bid_address_tree_info.into_new_address_params_assigned_packed(bid_seed, Some(1));

        let mut bid =
            LightAccount::<Bid>::new_init(&crate::ID, Some(bid_address), output_state_tree_index);
        bid.bidder = bidder_key;
        bid.proposal = proposal_id;
        bid.amount = amount;

        LightSystemProgramCpi::new_cpi(LIGHT_CPI_SIGNER, proof)
            .with_light_account(proposal)?
            .with_light_account(bid)?
            .with_new_addresses(&[bid_params])
            .invoke(light_cpi_accounts)?;

        let auction = &mut ctx.accounts.auction;
        auction.total_staked = auction
            .total_staked
            .checked_add(amount)
            .ok_or(BidonError::MathOverflow)?;
        auction.update_leader(proposal_id, new_total);

        Ok(())
    }

    /// Top up an EXISTING own Bid (no contention on the bidder's own position): pulls USDC,
    /// updates both ProposalTotal and Bid (two inclusions, one combined proof), bumps leader.
    #[allow(clippy::too_many_arguments)]
    pub fn top_up_bid<'info>(
        ctx: Context<'_, '_, '_, 'info, RaiseBid<'info>>,
        proof: ValidityProof,
        proposal_id: u64,
        proposal_meta: CompressedAccountMeta,
        proposal_creator: Pubkey,
        proposal_content_hash: [u8; 32],
        proposal_current_total: u64,
        bid_meta: CompressedAccountMeta,
        bid_current_amount: u64,
        amount: u64,
    ) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        require!(
            now < ctx.accounts.auction.end_time,
            BidonError::AuctionEnded
        );
        require!(
            amount >= ctx.accounts.auction.min_bid,
            BidonError::BelowMinBid
        );

        let bidder_key = ctx.accounts.bidder.key();

        transfer_to_vault(&ctx.accounts.token_program, &ctx.accounts.bidder_token, &ctx.accounts.usdc_mint, &ctx.accounts.vault, &ctx.accounts.bidder, amount)?;

        let light_cpi_accounts = CpiAccounts::new(
            ctx.accounts.payer.as_ref(),
            ctx.remaining_accounts,
            crate::LIGHT_CPI_SIGNER,
        );

        let mut proposal = LightAccount::<ProposalTotal>::new_mut(
            &crate::ID,
            &proposal_meta,
            ProposalTotal {
                creator: proposal_creator,
                content_hash: proposal_content_hash,
                total: proposal_current_total,
            },
        )?;
        let new_total = proposal
            .total
            .checked_add(amount)
            .ok_or(BidonError::Overflow)?;
        proposal.total = new_total;

        let mut bid = LightAccount::<Bid>::new_mut(
            &crate::ID,
            &bid_meta,
            Bid {
                bidder: bidder_key,
                proposal: proposal_id,
                amount: bid_current_amount,
            },
        )?;
        bid.amount = bid.amount.checked_add(amount).ok_or(BidonError::Overflow)?;

        LightSystemProgramCpi::new_cpi(LIGHT_CPI_SIGNER, proof)
            .with_light_account(proposal)?
            .with_light_account(bid)?
            .invoke(light_cpi_accounts)?;

        let auction = &mut ctx.accounts.auction;
        auction.total_staked = auction
            .total_staked
            .checked_add(amount)
            .ok_or(BidonError::MathOverflow)?;
        auction.update_leader(proposal_id, new_total);

        Ok(())
    }

    /// After end_time, pay the winning pool to the creator minus fee. Permissionless —
    /// funds can only flow to the creator's and the fee_receiver's token accounts. Vault
    /// is drained via a PDA signature by the auction authority.
    pub fn claim_winnings(ctx: Context<ClaimWinnings>) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        require!(
            now >= ctx.accounts.auction.end_time,
            BidonError::AuctionNotEnded
        );
        require!(!ctx.accounts.auction.creator_paid, BidonError::AlreadyClaimed);

        let winner_amount = ctx.accounts.auction.winner_amount;
        let fee_bps = ctx.accounts.auction.fee_bps as u64;
        let fee = winner_amount
            .checked_mul(fee_bps)
            .ok_or(BidonError::MathOverflow)?
            .checked_div(10_000)
            .ok_or(BidonError::MathOverflow)?;
        let payout = winner_amount.checked_sub(fee).ok_or(BidonError::MathOverflow)?;

        let decimals = ctx.accounts.usdc_mint.decimals;
        let id_bytes = ctx.accounts.auction.id.to_le_bytes();
        let bump = ctx.accounts.auction.bump;
        let signer_seeds: &[&[&[u8]]] = &[&[AUCTION_SEED, id_bytes.as_ref(), &[bump]]];

        if payout > 0 {
            vault_transfer(
                &ctx.accounts.token_program,
                &ctx.accounts.vault,
                &ctx.accounts.usdc_mint,
                &ctx.accounts.creator_token,
                &ctx.accounts.auction,
                signer_seeds,
                payout,
                decimals,
            )?;
        }
        if fee > 0 {
            vault_transfer(
                &ctx.accounts.token_program,
                &ctx.accounts.vault,
                &ctx.accounts.usdc_mint,
                &ctx.accounts.fee_receiver_token,
                &ctx.accounts.auction,
                signer_seeds,
                fee,
                decimals,
            )?;
        }

        ctx.accounts.auction.creator_paid = true;
        Ok(())
    }

    /// After end_time, a LOSING bidder reclaims their stake: transfer USDC back from the
    /// vault and CLOSE the compressed Bid (a closed compressed account cannot be reused —
    /// double-refund guard). Permissionless; payer (relayer) is the Light fee payer; the
    /// bidder is identified by argument (no bidder signature). Funds go only to the
    /// bidder's token account.
    pub fn withdraw<'info>(
        ctx: Context<'_, '_, '_, 'info, Withdraw<'info>>,
        proof: ValidityProof,
        proposal_id: u64,
        bidder: Pubkey,
        bid_meta: CompressedAccountMeta,
        bid_current_amount: u64,
    ) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        require!(
            now >= ctx.accounts.auction.end_time,
            BidonError::AuctionNotEnded
        );
        require!(
            proposal_id != ctx.accounts.auction.winner_proposal,
            BidonError::WinnerCannotWithdraw
        );
        require!(
            ctx.accounts.bidder_token.owner == bidder,
            BidonError::Unauthorized
        );

        let decimals = ctx.accounts.usdc_mint.decimals;
        let id_bytes = ctx.accounts.auction.id.to_le_bytes();
        let bump = ctx.accounts.auction.bump;
        let signer_seeds: &[&[&[u8]]] = &[&[AUCTION_SEED, id_bytes.as_ref(), &[bump]]];

        if bid_current_amount > 0 {
            vault_transfer(
                &ctx.accounts.token_program,
                &ctx.accounts.vault,
                &ctx.accounts.usdc_mint,
                &ctx.accounts.bidder_token,
                &ctx.accounts.auction,
                signer_seeds,
                bid_current_amount,
                decimals,
            )?;
        }

        // Close the compressed Bid (double-refund guard).
        let light_cpi_accounts = CpiAccounts::new(
            ctx.accounts.payer.as_ref(),
            ctx.remaining_accounts,
            crate::LIGHT_CPI_SIGNER,
        );
        let bid = LightAccount::<Bid>::new_close(
            &crate::ID,
            &bid_meta,
            Bid {
                bidder,
                proposal: proposal_id,
                amount: bid_current_amount,
            },
        )?;
        LightSystemProgramCpi::new_cpi(LIGHT_CPI_SIGNER, proof)
            .with_light_account(bid)?
            .invoke(light_cpi_accounts)?;

        Ok(())
    }

    /// After end_time, once the creator is paid and the vault is drained, close the vault
    /// (SPL) and the Auction, returning all rent to the relayer (rent_payer). Permissionless
    /// GC step — the only rent in the system is recovered here, closing the gasless loop.
    pub fn close_auction(ctx: Context<CloseAuction>) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        require!(
            now >= ctx.accounts.auction.end_time,
            BidonError::AuctionNotEnded
        );
        require!(ctx.accounts.auction.creator_paid, BidonError::NotSettled);
        require!(ctx.accounts.vault.amount == 0, BidonError::NotSettled);

        let id_bytes = ctx.accounts.auction.id.to_le_bytes();
        let bump = ctx.accounts.auction.bump;
        let signer_seeds: &[&[&[u8]]] = &[&[AUCTION_SEED, id_bytes.as_ref(), &[bump]]];
        close_account(CpiContext::new_with_signer(
            ctx.accounts.token_program.to_account_info(),
            CloseAccount {
                account: ctx.accounts.vault.to_account_info(),
                destination: ctx.accounts.rent_recipient.to_account_info(),
                authority: ctx.accounts.auction.to_account_info(),
            },
            signer_seeds,
        ))?;

        // Auction is closed by Anchor (close = rent_recipient).
        Ok(())
    }

    /// Creator cancels an EMPTY auction (proposal_count == 0): closes the vault (SPL) and the
    /// Auction, returning all rent to the relayer (rent_payer). Creator-only, no time gate.
    /// Atomic vs the race: a place_bid landing first makes proposal_count > 0 and this reverts;
    /// if this lands first, the Auction is gone and place_bid reverts.
    pub fn cancel_auction(ctx: Context<CancelAuction>) -> Result<()> {
        require!(
            ctx.accounts.auction.proposal_count == 0,
            BidonError::AuctionNotEmpty
        );

        let id_bytes = ctx.accounts.auction.id.to_le_bytes();
        let bump = ctx.accounts.auction.bump;
        let signer_seeds: &[&[&[u8]]] = &[&[AUCTION_SEED, id_bytes.as_ref(), &[bump]]];
        close_account(CpiContext::new_with_signer(
            ctx.accounts.token_program.to_account_info(),
            CloseAccount {
                account: ctx.accounts.vault.to_account_info(),
                destination: ctx.accounts.rent_recipient.to_account_info(),
                authority: ctx.accounts.auction.to_account_info(),
            },
            signer_seeds,
        ))?;

        // Auction is closed by Anchor (close = rent_recipient).
        Ok(())
    }
}

/// Transfer USDC out of the auction vault via a PDA signature by the auction authority.
#[allow(clippy::too_many_arguments)]
fn vault_transfer<'info>(
    token_program: &Program<'info, Token>,
    vault: &Account<'info, TokenAccount>,
    mint: &Account<'info, Mint>,
    to: &Account<'info, TokenAccount>,
    auction: &Account<'info, Auction>,
    signer_seeds: &[&[&[u8]]],
    amount: u64,
    decimals: u8,
) -> Result<()> {
    transfer_checked(
        CpiContext::new_with_signer(
            token_program.to_account_info(),
            TransferChecked {
                from: vault.to_account_info(),
                mint: mint.to_account_info(),
                to: to.to_account_info(),
                authority: auction.to_account_info(),
            },
            signer_seeds,
        ),
        amount,
        decimals,
    )
}

/// Transfer USDC from the bidder's token account into the auction vault.
fn transfer_to_vault<'info>(
    token_program: &Program<'info, Token>,
    from: &Account<'info, TokenAccount>,
    mint: &Account<'info, Mint>,
    vault: &Account<'info, TokenAccount>,
    authority: &Signer<'info>,
    amount: u64,
) -> Result<()> {
    transfer_checked(
        CpiContext::new(
            token_program.to_account_info(),
            TransferChecked {
                from: from.to_account_info(),
                mint: mint.to_account_info(),
                to: vault.to_account_info(),
                authority: authority.to_account_info(),
            },
        ),
        amount,
        mint.decimals,
    )
}

#[error_code]
pub enum BidonError {
    #[msg("Bid amount must be greater than zero")]
    InvalidAmount,
    #[msg("Bid amount overflow")]
    Overflow,
    #[msg("Fee exceeds maximum (10%)")]
    FeeTooHigh,
    #[msg("Only the config owner may do this")]
    Unauthorized,
    #[msg("Mint does not match the configured USDC mint")]
    InvalidMint,
    #[msg("Auction id must equal the current auction_count")]
    InvalidAuctionId,
    #[msg("Duration must be greater than zero")]
    InvalidDuration,
    #[msg("Math overflow")]
    MathOverflow,
    #[msg("Auction has ended")]
    AuctionEnded,
    #[msg("Bid is below the auction minimum")]
    BelowMinBid,
    #[msg("Auction has not ended yet")]
    AuctionNotEnded,
    #[msg("Winnings already claimed")]
    AlreadyClaimed,
    #[msg("The winning proposal's bids cannot be withdrawn")]
    WinnerCannotWithdraw,
    #[msg("Auction not fully settled (creator unpaid or vault non-empty)")]
    NotSettled,
    #[msg("Auction is not empty (has proposals) — cannot cancel")]
    AuctionNotEmpty,
}

#[derive(Accounts)]
pub struct Initialize<'info> {
    #[account(
        init,
        payer = owner,
        space = 8 + Config::INIT_SPACE,
        seeds = [CONFIG_SEED],
        bump
    )]
    pub config: Account<'info, Config>,
    #[account(mut)]
    pub owner: Signer<'info>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct SetConfig<'info> {
    #[account(
        mut,
        seeds = [CONFIG_SEED],
        bump = config.bump,
        has_one = owner @ BidonError::Unauthorized
    )]
    pub config: Account<'info, Config>,
    pub owner: Signer<'info>,
}

#[derive(Accounts)]
#[instruction(id: u64)]
pub struct CreateAuction<'info> {
    #[account(mut, seeds = [CONFIG_SEED], bump = config.bump)]
    pub config: Account<'info, Config>,
    #[account(
        init,
        payer = payer,
        space = 8 + Auction::INIT_SPACE,
        seeds = [AUCTION_SEED, id.to_le_bytes().as_ref()],
        bump
    )]
    pub auction: Account<'info, Auction>,
    /// Platform USDC mint (from config).
    #[account(constraint = usdc_mint.key() == config.usdc_mint @ BidonError::InvalidMint)]
    pub usdc_mint: Account<'info, Mint>,
    /// Auction USDC vault: PDA token account, authority = the auction itself.
    #[account(
        init,
        payer = payer,
        seeds = [VAULT_SEED, auction.key().as_ref()],
        bump,
        token::mint = usdc_mint,
        token::authority = auction,
    )]
    pub vault: Account<'info, TokenAccount>,
    /// Auction creator — authority (signs), pays NO rent.
    pub creator: Signer<'info>,
    /// Rent + tx-fee payer (relayer/gasless). Rent refunded to it on close.
    #[account(mut)]
    pub payer: Signer<'info>,
    pub token_program: Program<'info, Token>,
    pub system_program: Program<'info, System>,
}

/// Accounts for place_bid. Compressed-account (Light) accounts ride in remaining_accounts.
#[derive(Accounts)]
pub struct PlaceBid<'info> {
    #[account(seeds = [CONFIG_SEED], bump = config.bump)]
    pub config: Account<'info, Config>,
    #[account(
        mut,
        seeds = [AUCTION_SEED, auction.id.to_le_bytes().as_ref()],
        bump = auction.bump
    )]
    pub auction: Account<'info, Auction>,
    #[account(
        mut,
        seeds = [VAULT_SEED, auction.key().as_ref()],
        bump,
        token::mint = config.usdc_mint,
        token::authority = auction,
    )]
    pub vault: Account<'info, TokenAccount>,
    #[account(address = config.usdc_mint @ BidonError::InvalidMint)]
    pub usdc_mint: Account<'info, Mint>,
    /// Bidder's USDC token account (source of funds).
    #[account(mut, token::mint = config.usdc_mint, token::authority = bidder)]
    pub bidder_token: Account<'info, TokenAccount>,
    /// Bidder — authority for the transfer, signs with 0 SOL.
    pub bidder: Signer<'info>,
    /// Relayer — Light fee payer (gasless).
    #[account(mut)]
    pub payer: Signer<'info>,
    pub token_program: Program<'info, Token>,
}

/// Accounts for raise_bid / top_up_bid (same shape as PlaceBid). Compressed accounts
/// ride in remaining_accounts.
#[derive(Accounts)]
pub struct RaiseBid<'info> {
    #[account(seeds = [CONFIG_SEED], bump = config.bump)]
    pub config: Account<'info, Config>,
    #[account(
        mut,
        seeds = [AUCTION_SEED, auction.id.to_le_bytes().as_ref()],
        bump = auction.bump
    )]
    pub auction: Account<'info, Auction>,
    #[account(
        mut,
        seeds = [VAULT_SEED, auction.key().as_ref()],
        bump,
        token::mint = config.usdc_mint,
        token::authority = auction,
    )]
    pub vault: Account<'info, TokenAccount>,
    #[account(address = config.usdc_mint @ BidonError::InvalidMint)]
    pub usdc_mint: Account<'info, Mint>,
    #[account(mut, token::mint = config.usdc_mint, token::authority = bidder)]
    pub bidder_token: Account<'info, TokenAccount>,
    pub bidder: Signer<'info>,
    #[account(mut)]
    pub payer: Signer<'info>,
    pub token_program: Program<'info, Token>,
}

/// Accounts for claim_winnings (regular accounts only; vault drained via PDA signature).
#[derive(Accounts)]
pub struct ClaimWinnings<'info> {
    #[account(seeds = [CONFIG_SEED], bump = config.bump)]
    pub config: Box<Account<'info, Config>>,
    #[account(
        mut,
        seeds = [AUCTION_SEED, auction.id.to_le_bytes().as_ref()],
        bump = auction.bump
    )]
    pub auction: Box<Account<'info, Auction>>,
    #[account(
        mut,
        seeds = [VAULT_SEED, auction.key().as_ref()],
        bump,
        token::mint = usdc_mint,
        token::authority = auction,
    )]
    pub vault: Box<Account<'info, TokenAccount>>,
    #[account(
        mut,
        constraint = creator_token.owner == auction.creator @ BidonError::Unauthorized,
        constraint = creator_token.mint == config.usdc_mint @ BidonError::InvalidMint,
    )]
    pub creator_token: Box<Account<'info, TokenAccount>>,
    #[account(
        mut,
        constraint = fee_receiver_token.owner == config.fee_receiver @ BidonError::Unauthorized,
        constraint = fee_receiver_token.mint == config.usdc_mint @ BidonError::InvalidMint,
    )]
    pub fee_receiver_token: Box<Account<'info, TokenAccount>>,
    #[account(constraint = usdc_mint.key() == config.usdc_mint @ BidonError::InvalidMint)]
    pub usdc_mint: Box<Account<'info, Mint>>,
    pub token_program: Program<'info, Token>,
}

/// Accounts for withdraw (hybrid: regular vault transfer + compressed Bid close).
/// Compressed accounts ride in remaining_accounts. bidder is an argument (permissionless).
#[derive(Accounts)]
pub struct Withdraw<'info> {
    #[account(seeds = [CONFIG_SEED], bump = config.bump)]
    pub config: Box<Account<'info, Config>>,
    #[account(
        seeds = [AUCTION_SEED, auction.id.to_le_bytes().as_ref()],
        bump = auction.bump
    )]
    pub auction: Box<Account<'info, Auction>>,
    #[account(
        mut,
        seeds = [VAULT_SEED, auction.key().as_ref()],
        bump,
        token::mint = usdc_mint,
        token::authority = auction,
    )]
    pub vault: Box<Account<'info, TokenAccount>>,
    #[account(mut, constraint = bidder_token.mint == config.usdc_mint @ BidonError::InvalidMint)]
    pub bidder_token: Box<Account<'info, TokenAccount>>,
    #[account(constraint = usdc_mint.key() == config.usdc_mint @ BidonError::InvalidMint)]
    pub usdc_mint: Box<Account<'info, Mint>>,
    /// Relayer — Light fee payer (gasless, permissionless crank).
    #[account(mut)]
    pub payer: Signer<'info>,
    pub token_program: Program<'info, Token>,
}

/// Accounts for close_auction (permissionless GC): close vault (SPL) + Auction, rent -> relayer.
#[derive(Accounts)]
pub struct CloseAuction<'info> {
    #[account(
        mut,
        seeds = [AUCTION_SEED, auction.id.to_le_bytes().as_ref()],
        bump = auction.bump,
        close = rent_recipient,
    )]
    pub auction: Box<Account<'info, Auction>>,
    #[account(
        mut,
        seeds = [VAULT_SEED, auction.key().as_ref()],
        bump,
        token::authority = auction,
    )]
    pub vault: Box<Account<'info, TokenAccount>>,
    /// CHECK: address checked (== auction.rent_payer); receives the vault + Auction rent.
    #[account(mut, address = auction.rent_payer @ BidonError::Unauthorized)]
    pub rent_recipient: UncheckedAccount<'info>,
    pub token_program: Program<'info, Token>,
}

/// Accounts for cancel_auction (creator-only, EMPTY auction): close vault + Auction, rent -> relayer.
#[derive(Accounts)]
pub struct CancelAuction<'info> {
    #[account(
        mut,
        seeds = [AUCTION_SEED, auction.id.to_le_bytes().as_ref()],
        bump = auction.bump,
        has_one = creator @ BidonError::Unauthorized, // только создатель
        close = rent_recipient,
    )]
    pub auction: Box<Account<'info, Auction>>,
    #[account(
        mut,
        seeds = [VAULT_SEED, auction.key().as_ref()],
        bump,
        token::authority = auction,
    )]
    pub vault: Box<Account<'info, TokenAccount>>,
    /// Auction creator — authority (signs), pays NO rent (0 SOL ok; relayer is the fee-payer).
    pub creator: Signer<'info>,
    /// CHECK: address checked (== auction.rent_payer); receives the vault + Auction rent.
    #[account(mut, address = auction.rent_payer @ BidonError::Unauthorized)]
    pub rent_recipient: UncheckedAccount<'info>,
    pub token_program: Program<'info, Token>,
}

// #[event] makes the struct part of the Anchor IDL for client decoding.

/// Per-user position: rent-free compressed account, addressed by
/// (auction, proposal_id, bidder).
#[event]
#[derive(Clone, Debug, Default, LightDiscriminator, LightHasher)]
pub struct Bid {
    #[hash]
    pub bidder: Pubkey,
    pub proposal: u64,
    pub amount: u64,
}

/// Hot per-proposal aggregate. content_hash anchors the off-chain text (32 bytes,
/// fixed regardless of text length); total is the running parimutuel sum.
#[event]
#[derive(Clone, Debug, Default, LightDiscriminator, LightHasher)]
pub struct ProposalTotal {
    #[hash]
    pub creator: Pubkey,
    #[hash]
    pub content_hash: [u8; 32],
    pub total: u64,
}

// ---- regular (uncompressed) accounts: Config / Auction ----

/// Global registry config (singleton PDA, seed "config").
#[account]
#[derive(InitSpace)]
pub struct Config {
    pub owner: Pubkey,
    pub fee_bps: u16,
    pub fee_receiver: Pubkey,
    pub usdc_mint: Pubkey,
    pub auction_count: u64,
    pub bump: u8,
}

/// Auction (PDA seed "auction" + id LE). The hot global leader is a regular account
/// (native serialization). No `finalized`: all gates are time-based (now >= end_time).
#[account]
#[derive(InitSpace)]
pub struct Auction {
    pub id: u64,
    pub creator: Pubkey,
    pub min_bid: u64,
    pub fee_bps: u16,
    pub end_time: i64,
    pub creator_paid: bool,
    pub total_staked: u64,
    pub proposal_count: u64,
    /// Leader = proposal_id with the max running total (incremental, strict >).
    pub winner_proposal: u64,
    pub winner_amount: u64,
    /// Who fronted rent for Auction+vault (relayer) — refunded on close.
    pub rent_payer: Pubkey,
    pub bump: u8,
}

impl Auction {
    /// Update the leader only if a proposal's total strictly exceeds the current max
    /// (a tie keeps the earlier leader).
    pub fn update_leader(&mut self, proposal_id: u64, total: u64) {
        if total > self.winner_amount {
            self.winner_proposal = proposal_id;
            self.winner_amount = total;
        }
    }
}
