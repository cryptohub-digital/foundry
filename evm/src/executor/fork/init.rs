use crate::utils::{
    apply_network_and_block_specific_env_changes, h176_to_b176, h256_to_b256, u256_to_ru256,
};
use corebc::{
    providers::Middleware,
    types::{Address, Block, TxHash, U256},
};
use eyre::WrapErr;
use foundry_common::NON_ARCHIVE_NODE_WARNING;
use futures::TryFutureExt;
use revm::primitives::{BlockEnv, CfgEnv, Env, TxEnv, Network};

/// Initializes a REVM block environment based on a forked
/// ethereum provider.
pub async fn environment<M: Middleware>(
    provider: &M,
    memory_limit: u64,
    gas_price: Option<u64>,
    override_network_id: Option<u64>,
    pin_block: Option<u64>,
    origin: Address,
) -> eyre::Result<(Env, Block<TxHash>)>
where
    M::Error: 'static,
{
    let block_number = if let Some(pin_block) = pin_block {
        pin_block
    } else {
        provider.get_block_number().await.wrap_err("Failed to get latest block number")?.as_u64()
    };
    let (fork_gas_price, rpc_network_id, block) = tokio::try_join!(
        provider
            .get_gas_price()
            .map_err(|err| { eyre::Error::new(err).wrap_err("Failed to get gas price") }),
        provider
            .get_networkid()
            .map_err(|err| { eyre::Error::new(err).wrap_err("Failed to get network id") }),
        provider.get_block(block_number).map_err(|err| {
            eyre::Error::new(err).wrap_err(format!("Failed to get block {block_number}"))
        })
    )?;
    let block = if let Some(block) = block {
        block
    } else {
        if let Ok(latest_block) = provider.get_block_number().await {
            // If the `eth_getBlockByNumber` call succeeds, but returns null instead of
            // the block, and the block number is less than equal the latest block, then
            // the user is forking from a non-archive node with an older block number.
            if block_number <= latest_block.as_u64() {
                error!("{NON_ARCHIVE_NODE_WARNING}");
            }
            eyre::bail!(
                "Failed to get block for block number: {}\nlatest block number: {}",
                block_number,
                latest_block
            );
        }
        eyre::bail!("Failed to get block for block number: {}", block_number)
    };

    let mut env = Env {
        cfg: CfgEnv {
            network: Network::from(
                override_network_id.unwrap_or(rpc_network_id.as_u64()),
            ),
            memory_limit,
            limit_contract_code_size: Some(usize::MAX),
            // EIP-3607 rejects transactions from senders with deployed code.
            // If EIP-3607 is enabled it can cause issues during fuzz/invariant tests if the caller
            // is a contract. So we disable the check by default.
            disable_eip3607: true,
            ..Default::default()
        },
        block: BlockEnv {
            number: u256_to_ru256(block.number.expect("block number not found").as_u64().into()),
            timestamp: u256_to_ru256(block.timestamp),
            coinbase: h176_to_b176(block.author.unwrap_or_default()),
            difficulty: u256_to_ru256(block.difficulty),
            prevrandao: Some(block.mix_hash.map(h256_to_b256).unwrap_or_default()),
            basefee: u256_to_ru256(block.base_fee_per_gas.unwrap_or_default()),
            gas_limit: u256_to_ru256(block.gas_limit),
        },
        tx: TxEnv {
            caller: h176_to_b176(origin),
            gas_price: u256_to_ru256(gas_price.map(U256::from).unwrap_or(fork_gas_price)),
            network_id: Some(override_network_id.unwrap_or(rpc_network_id.as_u64())),
            gas_limit: block.gas_limit.as_u64(),
            ..Default::default()
        },
    };

    apply_network_and_block_specific_env_changes(&mut env, &block);

    Ok((env, block))
}
