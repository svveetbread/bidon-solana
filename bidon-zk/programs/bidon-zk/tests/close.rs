#![cfg(feature = "test-sbf")]

mod common;
use common::*;
use light_program_test::Rpc;
use solana_sdk::signature::Signer;

const CONTENT: [u8; 32] = [9u8; 32];

/// After settlement (creator paid, vault drained), close_auction closes the vault and the
/// Auction, returning all rent to the relayer — the gasless loop closes at ~0 net rent.
#[tokio::test]
async fn test_close_auction() {
    let mut rpc = new_rpc().await;
    let ctx = setup(&mut rpc, MIN_BID).await;

    // Single bidder/proposal -> winner; after claim the vault is empty.
    let (bidder, bidder_token) = funded_bidder(&mut rpc, &ctx, 1_000_000).await;
    do_place_bid(&mut rpc, &ctx, &bidder, bidder_token, 0, CONTENT, 1_000_000).await;

    let creator_token = token_account(&mut rpc, &ctx.payer, ctx.mint, &ctx.creator.pubkey()).await;
    let fee_receiver_token =
        token_account(&mut rpc, &ctx.payer, ctx.mint, &ctx.owner.pubkey()).await;

    warp_past(&mut rpc, 3601);
    do_claim(&mut rpc, &ctx, creator_token, fee_receiver_token).await;
    assert_eq!(token_amount(&mut rpc, ctx.vault_pda).await, 0);

    let rent_before = rpc.get_balance(&ctx.payer.pubkey()).await.unwrap();
    do_close_auction(&mut rpc, &ctx).await;

    // Vault + Auction are gone; relayer recovered the rent.
    assert!(rpc.get_account(ctx.vault_pda).await.unwrap().is_none());
    assert!(rpc.get_account(ctx.auction_pda).await.unwrap().is_none());
    assert!(rpc.get_balance(&ctx.payer.pubkey()).await.unwrap() > rent_before);
}
