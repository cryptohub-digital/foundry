use crate::{
    eth::{
        backend,
        backend::{
            db::SerializableState,
            mem::{MIN_CREATE_GAS, MIN_TRANSACTION_GAS},
            notifications::NewBlockNotifications,
            validate::TransactionValidator,
        },
        error::{
            decode_revert_reason, BlockchainError, InvalidTransactionError, Result,
            ToRpcResponseResult,
        },
        fees::FeeDetails,
        macros::node_info,
        miner::FixedBlockTimeMiner,
        pool::{
            transactions::{
                to_marker, PoolTransaction, TransactionOrder, TransactionPriority, TxMarker,
            },
            Pool,
        },
        sign,
        sign::Signer,
    },
    filter::{EthFilter, Filters, LogsFilter},
    mem::transaction_build,
    revm::primitives::Output,
    ClientFork, LoggingManager, Miner, MiningMode, StorageInfo,
};
use anvil_core::{
    eth::{
        block::BlockInfo,
        proof::AccountProof,
        state::StateOverride,
        transaction::{
            EthTransactionRequest, LegacyTransaction, PendingTransaction, TransactionKind,
            TypedTransaction, TypedTransactionRequest,
        },
        EthRequest,
    },
    types::{EvmMineOptions, Forking, Index, NodeEnvironment, NodeForkConfig, NodeInfo, Work},
};
use anvil_rpc::{error::RpcError, response::ResponseResult};
use corebc::{
    abi::ethereum_types::H64,
    prelude::{DefaultFrame, TxpoolInspect},
    providers::ProviderError,
    types::{
        transaction::eip712::TypedData, Address, Block, BlockId, BlockNumber, Bytes, Filter,
        FilteredParams, GoCoreDebugTracingOptions, GoCoreTrace, Log, Trace, Transaction,
        TransactionReceipt, TxHash, TxpoolContent, TxpoolInspectSummary, TxpoolStatus, H256, U256,
        U64,
    },
    utils::rlp,
};
use forge::{executor::DatabaseRef, revm::primitives::BlockEnv};
use foundry_common::ProviderBuilder;
use foundry_evm::{
    executor::backend::DatabaseError,
    revm::interpreter::{return_ok, return_revert, InstructionResult},
};
use foundry_utils::types::ToEthersU256;
use futures::channel::mpsc::Receiver;
use parking_lot::RwLock;
use std::{sync::Arc, time::Duration};
use tracing::{trace, warn};

use super::{backend::mem::BlockRequest, sign::build_typed_transaction};

/// The client version: `anvil/v{major}.{minor}.{patch}`
pub const CLIENT_VERSION: &str = concat!("anvil/v", env!("CARGO_PKG_VERSION"));

/// The entry point for executing eth api RPC call - The Eth RPC interface.
///
/// This type is cheap to clone and can be used concurrently
#[derive(Clone)]
pub struct EthApi {
    /// The transaction pool
    pool: Arc<Pool>,
    /// Holds all blockchain related data
    /// In-Memory only for now
    backend: Arc<backend::mem::Backend>,
    /// Whether this node is mining
    is_mining: bool,
    /// available signers
    signers: Arc<Vec<Box<dyn Signer>>>,
    /// access to the actual miner
    ///
    /// This access is required in order to adjust miner settings based on requests received from
    /// custom RPC endpoints
    miner: Miner,
    /// allows to enabled/disable logging
    logger: LoggingManager,
    /// Tracks all active filters
    filters: Filters,
    /// How transactions are ordered in the pool
    transaction_order: Arc<RwLock<TransactionOrder>>,
    /// Whether we're listening for RPC calls
    net_listening: bool,
}

// === impl Eth RPC API ===

impl EthApi {
    /// Creates a new instance
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        pool: Arc<Pool>,
        backend: Arc<backend::mem::Backend>,
        signers: Arc<Vec<Box<dyn Signer>>>,
        miner: Miner,
        logger: LoggingManager,
        filters: Filters,
        transactions_order: TransactionOrder,
    ) -> Self {
        Self {
            pool,
            backend,
            is_mining: true,
            signers,
            miner,
            logger,
            filters,
            net_listening: true,
            transaction_order: Arc::new(RwLock::new(transactions_order)),
        }
    }

    /// Executes the [EthRequest] and returns an RPC [RpcResponse]
    pub async fn execute(&self, request: EthRequest) -> ResponseResult {
        trace!(target: "rpc::api", "executing eth request");
        match request {
            EthRequest::Web3ClientVersion(()) => self.client_version().to_rpc_result(),
            EthRequest::Web3Sha3(content) => self.sha3(content).to_rpc_result(),
            EthRequest::EthGetBalance(addr, block) => {
                self.balance(addr, block).await.to_rpc_result()
            }
            EthRequest::EthGetTransactionByHash(hash) => {
                self.transaction_by_hash(hash).await.to_rpc_result()
            }
            EthRequest::EthSendTransaction(request) => {
                self.send_transaction(*request).await.to_rpc_result()
            }
            EthRequest::EthNetworkId(_) => self.network_id().to_rpc_result(),
            EthRequest::NetListening(_) => self.net_listening().to_rpc_result(),
            EthRequest::EthGasPrice(_) => self.gas_price().to_rpc_result(),
            EthRequest::EthAccounts(_) => self.accounts().to_rpc_result(),
            EthRequest::EthBlockNumber(_) => self.block_number().to_rpc_result(),
            EthRequest::EthGetStorageAt(addr, slot, block) => {
                self.storage_at(addr, slot, block).await.to_rpc_result()
            }
            EthRequest::EthGetBlockByHash(hash, full) => {
                if full {
                    self.block_by_hash_full(hash).await.to_rpc_result()
                } else {
                    self.block_by_hash(hash).await.to_rpc_result()
                }
            }
            EthRequest::EthGetBlockByNumber(num, full) => {
                if full {
                    self.block_by_number_full(num).await.to_rpc_result()
                } else {
                    self.block_by_number(num).await.to_rpc_result()
                }
            }
            EthRequest::EthGetTransactionCount(addr, block) => {
                self.transaction_count(addr, block).await.to_rpc_result()
            }
            EthRequest::EthGetTransactionCountByHash(hash) => {
                self.block_transaction_count_by_hash(hash).await.to_rpc_result()
            }
            EthRequest::EthGetTransactionCountByNumber(num) => {
                self.block_transaction_count_by_number(num).await.to_rpc_result()
            }
            EthRequest::EthGetUnclesCountByHash(hash) => {
                self.block_uncles_count_by_hash(hash).await.to_rpc_result()
            }
            EthRequest::EthGetUnclesCountByNumber(num) => {
                self.block_uncles_count_by_number(num).await.to_rpc_result()
            }
            EthRequest::EthGetCodeAt(addr, block) => {
                self.get_code(addr, block).await.to_rpc_result()
            }
            EthRequest::EthGetProof(addr, keys, block) => {
                self.get_proof(addr, keys, block).await.to_rpc_result()
            }
            EthRequest::EthSign(addr, content) => self.sign(addr, content).await.to_rpc_result(),
            EthRequest::EthSignTransaction(request) => {
                self.sign_transaction(*request).await.to_rpc_result()
            }
            EthRequest::EthSignTypedData(addr, data) => {
                self.sign_typed_data(addr, data).await.to_rpc_result()
            }
            EthRequest::EthSignTypedDataV3(addr, data) => {
                self.sign_typed_data_v3(addr, data).await.to_rpc_result()
            }
            EthRequest::EthSignTypedDataV4(addr, data) => {
                self.sign_typed_data_v4(addr, &data).await.to_rpc_result()
            }
            EthRequest::EthSendRawTransaction(tx) => {
                self.send_raw_transaction(tx).await.to_rpc_result()
            }
            EthRequest::EthCall(call, block, overrides) => {
                self.call(call, block, overrides).await.to_rpc_result()
            }
            EthRequest::EthEstimateGas(call, block) => {
                self.estimate_gas(call, block).await.to_rpc_result()
            }
            EthRequest::EthGetTransactionByBlockHashAndIndex(hash, index) => {
                self.transaction_by_block_hash_and_index(hash, index).await.to_rpc_result()
            }
            EthRequest::EthGetTransactionByBlockNumberAndIndex(num, index) => {
                self.transaction_by_block_number_and_index(num, index).await.to_rpc_result()
            }
            EthRequest::EthGetTransactionReceipt(tx) => {
                self.transaction_receipt(tx).await.to_rpc_result()
            }
            EthRequest::EthGetUncleByBlockHashAndIndex(hash, index) => {
                self.uncle_by_block_hash_and_index(hash, index).await.to_rpc_result()
            }
            EthRequest::EthGetUncleByBlockNumberAndIndex(num, index) => {
                self.uncle_by_block_number_and_index(num, index).await.to_rpc_result()
            }
            EthRequest::EthGetLogs(filter) => self.logs(filter).await.to_rpc_result(),
            EthRequest::EthGetWork(_) => self.work().to_rpc_result(),
            EthRequest::EthSyncing(_) => self.syncing().to_rpc_result(),
            EthRequest::EthSubmitWork(nonce, pow, digest) => {
                self.submit_work(nonce, pow, digest).to_rpc_result()
            }
            EthRequest::EthSubmitHashRate(rate, id) => {
                self.submit_hashrate(rate, id).to_rpc_result()
            }

            // non eth-standard rpc calls
            EthRequest::DebugTraceTransaction(tx, opts) => {
                self.debug_trace_transaction(tx, opts).await.to_rpc_result()
            }
            // non eth-standard rpc calls
            EthRequest::DebugTraceCall(tx, block, opts) => {
                self.debug_trace_call(tx, block, opts).await.to_rpc_result()
            }
            EthRequest::TraceTransaction(tx) => self.trace_transaction(tx).await.to_rpc_result(),
            EthRequest::TraceBlock(block) => self.trace_block(block).await.to_rpc_result(),
            EthRequest::ImpersonateAccount(addr) => {
                self.anvil_impersonate_account(addr).await.to_rpc_result()
            }
            EthRequest::StopImpersonatingAccount(addr) => {
                self.anvil_stop_impersonating_account(addr).await.to_rpc_result()
            }
            EthRequest::AutoImpersonateAccount(enable) => {
                self.anvil_auto_impersonate_account(enable).await.to_rpc_result()
            }
            EthRequest::GetAutoMine(()) => self.anvil_get_auto_mine().to_rpc_result(),
            EthRequest::Mine(blocks, interval) => {
                self.anvil_mine(blocks, interval).await.to_rpc_result()
            }
            EthRequest::SetAutomine(enabled) => {
                self.anvil_set_auto_mine(enabled).await.to_rpc_result()
            }
            EthRequest::SetIntervalMining(interval) => {
                self.anvil_set_interval_mining(interval).to_rpc_result()
            }
            EthRequest::DropTransaction(tx) => {
                self.anvil_drop_transaction(tx).await.to_rpc_result()
            }
            EthRequest::Reset(fork) => {
                self.anvil_reset(fork.and_then(|p| p.params)).await.to_rpc_result()
            }
            EthRequest::SetBalance(addr, val) => {
                self.anvil_set_balance(addr, val).await.to_rpc_result()
            }
            EthRequest::SetCode(addr, code) => {
                self.anvil_set_code(addr, code).await.to_rpc_result()
            }
            EthRequest::SetNonce(addr, nonce) => {
                self.anvil_set_nonce(addr, nonce).await.to_rpc_result()
            }
            EthRequest::SetStorageAt(addr, slot, val) => {
                self.anvil_set_storage_at(addr, slot, val).await.to_rpc_result()
            }
            EthRequest::SetCoinbase(addr) => self.anvil_set_coinbase(addr).await.to_rpc_result(),
            EthRequest::SetLogging(log) => self.anvil_set_logging(log).await.to_rpc_result(),
            EthRequest::SetMinGasPrice(gas) => {
                self.anvil_set_min_gas_price(gas).await.to_rpc_result()
            }
            EthRequest::DumpState(_) => self.anvil_dump_state().await.to_rpc_result(),
            EthRequest::LoadState(buf) => self.anvil_load_state(buf).await.to_rpc_result(),
            EthRequest::NodeInfo(_) => self.anvil_node_info().await.to_rpc_result(),
            EthRequest::EvmSnapshot(_) => self.evm_snapshot().await.to_rpc_result(),
            EthRequest::EvmRevert(id) => self.evm_revert(id).await.to_rpc_result(),
            EthRequest::EvmIncreaseTime(time) => self.evm_increase_time(time).await.to_rpc_result(),
            EthRequest::EvmSetNextBlockTimeStamp(time) => {
                match u64::try_from(time).map_err(BlockchainError::UintConversion) {
                    Ok(time) => self.evm_set_next_block_timestamp(time).to_rpc_result(),
                    err @ Err(_) => err.to_rpc_result(),
                }
            }
            EthRequest::EvmSetTime(timestamp) => {
                match u64::try_from(timestamp).map_err(BlockchainError::UintConversion) {
                    Ok(timestamp) => self.evm_set_time(timestamp).to_rpc_result(),
                    err @ Err(_) => err.to_rpc_result(),
                }
            }
            EthRequest::EvmSetBlockGasLimit(gas_limit) => {
                self.evm_set_block_gas_limit(gas_limit).to_rpc_result()
            }
            EthRequest::EvmSetBlockTimeStampInterval(time) => {
                self.evm_set_block_timestamp_interval(time).to_rpc_result()
            }
            EthRequest::EvmRemoveBlockTimeStampInterval(()) => {
                self.evm_remove_block_timestamp_interval().to_rpc_result()
            }
            EthRequest::EvmMine(mine) => {
                self.evm_mine(mine.and_then(|p| p.params)).await.to_rpc_result()
            }
            EthRequest::EvmMineDetailed(mine) => {
                self.evm_mine_detailed(mine.and_then(|p| p.params)).await.to_rpc_result()
            }
            EthRequest::SetRpcUrl(url) => self.anvil_set_rpc_url(url).to_rpc_result(),
            EthRequest::EthSendUnsignedTransaction(tx) => {
                self.eth_send_unsigned_transaction(*tx).await.to_rpc_result()
            }
            EthRequest::EnableTraces(_) => self.anvil_enable_traces().await.to_rpc_result(),
            EthRequest::EthNewFilter(filter) => self.new_filter(filter).await.to_rpc_result(),
            EthRequest::EthGetFilterChanges(id) => self.get_filter_changes(&id).await,
            EthRequest::EthNewBlockFilter(_) => self.new_block_filter().await.to_rpc_result(),
            EthRequest::EthNewPendingTransactionFilter(_) => {
                self.new_pending_transaction_filter().await.to_rpc_result()
            }
            EthRequest::EthGetFilterLogs(id) => self.get_filter_logs(&id).await.to_rpc_result(),
            EthRequest::EthUninstallFilter(id) => self.uninstall_filter(&id).await.to_rpc_result(),
            EthRequest::TxPoolStatus(_) => self.txpool_status().await.to_rpc_result(),
            EthRequest::TxPoolInspect(_) => self.txpool_inspect().await.to_rpc_result(),
            EthRequest::TxPoolContent(_) => self.txpool_content().await.to_rpc_result(),
        }
    }

    fn sign_request(
        &self,
        from: &Address,
        request: TypedTransactionRequest,
    ) -> Result<TypedTransaction> {
        for signer in self.signers.iter() {
            if signer.accounts().contains(from) {
                let signature = signer.sign_transaction(request.clone(), from)?;
                return build_typed_transaction(request, signature)
            }
        }
        Err(BlockchainError::NoSignerAvailable)
    }

    /// Queries the current gas limit
    fn current_gas_limit(&self) -> Result<U256> {
        Ok(self.backend.gas_limit())
    }

    async fn block_request(&self, block_number: Option<BlockId>) -> Result<BlockRequest> {
        let block_request = match block_number {
            Some(BlockId::Number(BlockNumber::Pending)) => {
                let pending_txs = self.pool.ready_transactions().collect();
                BlockRequest::Pending(pending_txs)
            }
            _ => {
                let number = self.backend.ensure_block_number(block_number).await?;
                BlockRequest::Number(number.into())
            }
        };
        Ok(block_request)
    }

    /// Returns the current client version.
    ///
    /// Handler for ETH RPC call: `web3_clientVersion`
    pub fn client_version(&self) -> Result<String> {
        node_info!("web3_clientVersion");
        Ok(CLIENT_VERSION.to_string())
    }

    /// Returns Keccak-256 (not the standardized SHA3-256) of the given data.
    ///
    /// Handler for ETH RPC call: `web3_sha3`
    pub fn sha3(&self, bytes: Bytes) -> Result<String> {
        node_info!("web3_sha3");
        let hash = corebc::utils::sha3(bytes.as_ref());
        Ok(corebc::utils::hex::encode(&hash[..]))
    }

    /// Returns protocol version encoded as a string (quotes are necessary).
    ///
    /// Handler for ETH RPC call: `eth_protocolVersion`
    pub fn protocol_version(&self) -> Result<u64> {
        node_info!("eth_protocolVersion");
        Ok(1)
    }

    /// Returns the number of hashes per second that the node is mining with.
    ///
    /// Handler for ETH RPC call: `eth_hashrate`
    pub fn hashrate(&self) -> Result<U256> {
        node_info!("eth_hashrate");
        Ok(U256::zero())
    }

    /// Returns the client coinbase address.
    ///
    /// Handler for ETH RPC call: `eth_coinbase`
    pub fn author(&self) -> Result<Address> {
        node_info!("eth_coinbase");
        Ok(self.backend.coinbase())
    }

    /// Returns true if client is actively mining new blocks.
    ///
    /// Handler for ETH RPC call: `eth_mining`
    pub fn is_mining(&self) -> Result<bool> {
        node_info!("eth_mining");
        Ok(self.is_mining)
    }

    /// Returns the chain ID used for transaction signing at the
    /// current best block. None is returned if not
    /// available.
    ///
    /// Handler for ETH RPC call: `eth_chainId`
    pub fn eth_chain_id(&self) -> Result<Option<U64>> {
        node_info!("eth_chainId");
        Ok(Some(self.backend.chain_id().as_u64().into()))
    }

    /// Returns the same as `chain_id`
    ///
    /// Handler for ETH RPC call: `eth_networkId`
    pub fn network_id(&self) -> Result<Option<String>> {
        node_info!("eth_networkId");
        let chain_id = self.backend.chain_id().as_u64();
        Ok(Some(format!("{chain_id}")))
    }

    /// Returns true if client is actively listening for network connections.
    ///
    /// Handler for ETH RPC call: `net_listening`
    pub fn net_listening(&self) -> Result<bool> {
        node_info!("net_listening");
        Ok(self.net_listening)
    }

    /// Returns the current gas price
    pub fn gas_price(&self) -> Result<U256> {
        Ok(self.backend.gas_price())
    }

    /// Returns the block gas limit
    pub fn gas_limit(&self) -> U256 {
        self.backend.gas_limit()
    }

    /// Returns the accounts list
    ///
    /// Handler for ETH RPC call: `eth_accounts`
    pub fn accounts(&self) -> Result<Vec<Address>> {
        node_info!("eth_accounts");
        let mut accounts = Vec::new();
        for signer in self.signers.iter() {
            accounts.append(&mut signer.accounts());
        }
        Ok(accounts)
    }

    /// Returns the number of most recent block.
    ///
    /// Handler for ETH RPC call: `eth_blockNumber`
    pub fn block_number(&self) -> Result<U256> {
        node_info!("eth_blockNumber");
        Ok(self.backend.best_number().as_u64().into())
    }

    /// Returns balance of the given account.
    ///
    /// Handler for ETH RPC call: `eth_getBalance`
    pub async fn balance(&self, address: Address, block_number: Option<BlockId>) -> Result<U256> {
        node_info!("eth_getBalance");
        let block_request = self.block_request(block_number).await?;

        // check if the number predates the fork, if in fork mode
        if let BlockRequest::Number(number) = &block_request {
            if let Some(fork) = self.get_fork() {
                if fork.predates_fork(number.as_u64()) {
                    return Ok(fork.get_balance(address, number.as_u64()).await?)
                }
            }
        }

        self.backend.get_balance(address, Some(block_request)).await
    }

    /// Returns content of the storage at given address.
    ///
    /// Handler for ETH RPC call: `eth_getStorageAt`
    pub async fn storage_at(
        &self,
        address: Address,
        index: U256,
        block_number: Option<BlockId>,
    ) -> Result<H256> {
        node_info!("eth_getStorageAt");
        let block_request = self.block_request(block_number).await?;

        // check if the number predates the fork, if in fork mode
        if let BlockRequest::Number(number) = &block_request {
            if let Some(fork) = self.get_fork() {
                if fork.predates_fork(number.as_u64()) {
                    return Ok(fork
                        .storage_at(address, index, Some(BlockNumber::Number(*number)))
                        .await?)
                }
            }
        }

        self.backend.storage_at(address, index, Some(block_request)).await
    }

    /// Returns block with given hash.
    ///
    /// Handler for ETH RPC call: `eth_getBlockByHash`
    pub async fn block_by_hash(&self, hash: H256) -> Result<Option<Block<TxHash>>> {
        node_info!("eth_getBlockByHash");
        self.backend.block_by_hash(hash).await
    }

    /// Returns a _full_ block with given hash.
    ///
    /// Handler for ETH RPC call: `eth_getBlockByHash`
    pub async fn block_by_hash_full(&self, hash: H256) -> Result<Option<Block<Transaction>>> {
        node_info!("eth_getBlockByHash");
        self.backend.block_by_hash_full(hash).await
    }

    /// Returns block with given number.
    ///
    /// Handler for ETH RPC call: `eth_getBlockByNumber`
    pub async fn block_by_number(&self, number: BlockNumber) -> Result<Option<Block<TxHash>>> {
        node_info!("eth_getBlockByNumber");
        if number == BlockNumber::Pending {
            return Ok(Some(self.pending_block().await))
        }

        self.backend.block_by_number(number).await
    }

    /// Returns a _full_ block with given number
    ///
    /// Handler for ETH RPC call: `eth_getBlockByNumber`
    pub async fn block_by_number_full(
        &self,
        number: BlockNumber,
    ) -> Result<Option<Block<Transaction>>> {
        node_info!("eth_getBlockByNumber");
        if number == BlockNumber::Pending {
            return Ok(self.pending_block_full().await)
        }
        self.backend.block_by_number_full(number).await
    }

    /// Returns the number of transactions sent from given address at given time (block number).
    ///
    /// Also checks the pending transactions if `block_number` is
    /// `BlockId::Number(BlockNumber::Pending)`
    ///
    /// Handler for ETH RPC call: `eth_getTransactionCount`
    pub async fn transaction_count(
        &self,
        address: Address,
        block_number: Option<BlockId>,
    ) -> Result<U256> {
        node_info!("eth_getTransactionCount");
        self.get_transaction_count(address, block_number).await
    }

    /// Returns the number of transactions in a block with given hash.
    ///
    /// Handler for ETH RPC call: `eth_getBlockTransactionCountByHash`
    pub async fn block_transaction_count_by_hash(&self, hash: H256) -> Result<Option<U256>> {
        node_info!("eth_getBlockTransactionCountByHash");
        let block = self.backend.block_by_hash(hash).await?;
        Ok(block.map(|b| b.transactions.len().into()))
    }

    /// Returns the number of transactions in a block with given block number.
    ///
    /// Handler for ETH RPC call: `eth_getBlockTransactionCountByNumber`
    pub async fn block_transaction_count_by_number(
        &self,
        block_number: BlockNumber,
    ) -> Result<Option<U256>> {
        node_info!("eth_getBlockTransactionCountByNumber");
        let block_request = self.block_request(Some(block_number.into())).await?;
        if let BlockRequest::Pending(txs) = block_request {
            let block = self.backend.pending_block(txs).await;
            return Ok(Some(block.transactions.len().into()))
        }
        let block = self.backend.block_by_number(block_number).await?;
        Ok(block.map(|b| b.transactions.len().into()))
    }

    /// Returns the number of uncles in a block with given hash.
    ///
    /// Handler for ETH RPC call: `eth_getUncleCountByBlockHash`
    pub async fn block_uncles_count_by_hash(&self, hash: H256) -> Result<U256> {
        node_info!("eth_getUncleCountByBlockHash");
        let block =
            self.backend.block_by_hash(hash).await?.ok_or(BlockchainError::BlockNotFound)?;
        Ok(block.uncles.len().into())
    }

    /// Returns the number of uncles in a block with given block number.
    ///
    /// Handler for ETH RPC call: `eth_getUncleCountByBlockNumber`
    pub async fn block_uncles_count_by_number(&self, block_number: BlockNumber) -> Result<U256> {
        node_info!("eth_getUncleCountByBlockNumber");
        let block = self
            .backend
            .block_by_number(block_number)
            .await?
            .ok_or(BlockchainError::BlockNotFound)?;
        Ok(block.uncles.len().into())
    }

    /// Returns the code at given address at given time (block number).
    ///
    /// Handler for ETH RPC call: `eth_getCode`
    pub async fn get_code(&self, address: Address, block_number: Option<BlockId>) -> Result<Bytes> {
        node_info!("eth_getCode");
        let block_request = self.block_request(block_number).await?;
        // check if the number predates the fork, if in fork mode
        if let BlockRequest::Number(number) = &block_request {
            if let Some(fork) = self.get_fork() {
                if fork.predates_fork(number.as_u64()) {
                    return Ok(fork.get_code(address, number.as_u64()).await?)
                }
            }
        }
        self.backend.get_code(address, Some(block_request)).await
    }

    /// Returns the account and storage values of the specified account including the Merkle-proof.
    /// This call can be used to verify that the data you are pulling from is not tampered with.
    ///
    /// Handler for ETH RPC call: `eth_getProof`
    pub async fn get_proof(
        &self,
        address: Address,
        keys: Vec<H256>,
        block_number: Option<BlockId>,
    ) -> Result<AccountProof> {
        node_info!("eth_getProof");
        let block_request = self.block_request(block_number).await?;

        if let BlockRequest::Number(number) = &block_request {
            if let Some(fork) = self.get_fork() {
                // if we're in forking mode, or still on the forked block (no blocks mined yet) then
                // we can delegate the call
                if fork.predates_fork_inclusive(number.as_u64()) {
                    return Ok(fork.get_proof(address, keys, Some((*number).into())).await?)
                }
            }
        }

        let proof = self.backend.prove_account_at(address, keys, Some(block_request)).await?;
        Ok(proof)
    }

    /// Signs data via [EIP-712](https://github.com/ethereum/EIPs/blob/master/EIPS/eip-712.md).
    ///
    /// Handler for ETH RPC call: `eth_signTypedData`
    pub async fn sign_typed_data(
        &self,
        _address: Address,
        _data: serde_json::Value,
    ) -> Result<String> {
        node_info!("eth_signTypedData");
        Err(BlockchainError::RpcUnimplemented)
    }

    /// Signs data via [EIP-712](https://github.com/ethereum/EIPs/blob/master/EIPS/eip-712.md).
    ///
    /// Handler for ETH RPC call: `eth_signTypedData_v3`
    pub async fn sign_typed_data_v3(
        &self,
        _address: Address,
        _data: serde_json::Value,
    ) -> Result<String> {
        node_info!("eth_signTypedData_v3");
        Err(BlockchainError::RpcUnimplemented)
    }

    /// Signs data via [EIP-712](https://github.com/ethereum/EIPs/blob/master/EIPS/eip-712.md), and includes full support of arrays and recursive data structures.
    ///
    /// Handler for ETH RPC call: `eth_signTypedData_v4`
    pub async fn sign_typed_data_v4(&self, address: Address, data: &TypedData) -> Result<String> {
        node_info!("eth_signTypedData_v4");
        let signer = self.get_signer(address).ok_or(BlockchainError::NoSignerAvailable)?;
        let signature = signer.sign_typed_data(address, data).await?;
        Ok(format!("0x{signature}"))
    }

    /// The sign method calculates an Ethereum specific signature
    ///
    /// Handler for ETH RPC call: `eth_sign`
    pub async fn sign(&self, address: Address, content: impl AsRef<[u8]>) -> Result<String> {
        node_info!("eth_sign");
        let signer = self.get_signer(address).ok_or(BlockchainError::NoSignerAvailable)?;
        let signature = signer.sign(address, content.as_ref()).await?;
        Ok(format!("0x{signature}"))
    }

    /// Signs a transaction
    ///
    /// Handler for ETH RPC call: `eth_signTransaction`
    pub async fn sign_transaction(&self, request: EthTransactionRequest) -> Result<String> {
        node_info!("eth_signTransaction");

        let from = request.from.map(Ok).unwrap_or_else(|| {
            self.accounts()?.get(0).cloned().ok_or(BlockchainError::NoSignerAvailable)
        })?;

        let (nonce, _) = self.request_nonce(&request, from).await?;

        let request = self.build_typed_tx_request(request, nonce)?;

        let signer = self.get_signer(from).ok_or(BlockchainError::NoSignerAvailable)?;
        let signature = signer.sign_transaction(request, &from)?;
        Ok(format!("0x{signature}"))
    }

    /// Sends a transaction
    ///
    /// Handler for ETH RPC call: `eth_sendTransaction`
    pub async fn send_transaction(&self, request: EthTransactionRequest) -> Result<TxHash> {
        node_info!("eth_sendTransaction");

        let from = request.from.map(Ok).unwrap_or_else(|| {
            self.accounts()?.get(0).cloned().ok_or(BlockchainError::NoSignerAvailable)
        })?;

        let (nonce, on_chain_nonce) = self.request_nonce(&request, from).await?;

        let request = self.build_typed_tx_request(request, nonce)?;

        // if the sender is currently impersonated we need to "bypass" signing
        let pending_transaction = if self.is_impersonated(from) {
            let bypass_signature = self.backend.cheats().bypass_signature();
            let transaction = sign::build_typed_transaction(request, bypass_signature)?;
            self.ensure_typed_transaction_supported(&transaction)?;
            trace!(target : "node", ?from, "eth_sendTransaction: impersonating");
            PendingTransaction::with_impersonated(transaction, from)
        } else {
            let transaction = self.sign_request(&from, request)?;
            self.ensure_typed_transaction_supported(&transaction)?;
            PendingTransaction::new(transaction)?
        };

        // pre-validate
        self.backend.validate_pool_transaction(&pending_transaction).await?;

        let requires = required_marker(nonce, on_chain_nonce, from);
        let provides = vec![to_marker(nonce.as_u64(), from)];
        debug_assert!(requires != provides);

        self.add_pending_transaction(pending_transaction, requires, provides)
    }

    /// Sends signed transaction, returning its hash.
    ///
    /// Handler for ETH RPC call: `eth_sendRawTransaction`
    pub async fn send_raw_transaction(&self, tx: Bytes) -> Result<TxHash> {
        node_info!("eth_sendRawTransaction");
        let data = tx.as_ref();
        if data.is_empty() {
            return Err(BlockchainError::EmptyRawTransactionData)
        }
        let transaction = if data[0] > 0x7f {
            // legacy transaction
            match rlp::decode::<LegacyTransaction>(data) {
                Ok(transaction) => TypedTransaction::Legacy(transaction),
                Err(_) => return Err(BlockchainError::FailedToDecodeSignedTransaction),
            }
        } else {
            // the [TypedTransaction] requires a valid rlp input,
            // but EIP-1559 prepends a version byte, so we need to encode the data first to get a
            // valid rlp and then rlp decode impl of `TypedTransaction` will remove and check the
            // version byte
            let extend = rlp::encode(&data);
            let tx = match rlp::decode::<TypedTransaction>(&extend[..]) {
                Ok(transaction) => transaction,
                Err(_) => return Err(BlockchainError::FailedToDecodeSignedTransaction),
            };

            self.ensure_typed_transaction_supported(&tx)?;

            tx
        };

        let pending_transaction = PendingTransaction::new(transaction)?;

        // pre-validate
        self.backend.validate_pool_transaction(&pending_transaction).await?;

        let on_chain_nonce = self.backend.current_nonce(*pending_transaction.sender()).await?;
        let from = *pending_transaction.sender();
        let nonce = *pending_transaction.transaction.nonce();
        let requires = required_marker(nonce, on_chain_nonce, from);

        let priority = self.transaction_priority(&pending_transaction.transaction);
        let pool_transaction = PoolTransaction {
            requires,
            provides: vec![to_marker(nonce.as_u64(), *pending_transaction.sender())],
            pending_transaction,
            priority,
        };

        let tx = self.pool.add_transaction(pool_transaction)?;
        trace!(target: "node", "Added transaction: [{:?}] sender={:?}", tx.hash(), from);
        Ok(*tx.hash())
    }

    /// Call contract, returning the output data.
    ///
    /// Handler for ETH RPC call: `eth_call`
    pub async fn call(
        &self,
        request: EthTransactionRequest,
        block_number: Option<BlockId>,
        overrides: Option<StateOverride>,
    ) -> Result<Bytes> {
        node_info!("eth_call");
        let block_request = self.block_request(block_number).await?;
        // check if the number predates the fork, if in fork mode
        if let BlockRequest::Number(number) = &block_request {
            if let Some(fork) = self.get_fork() {
                if fork.predates_fork(number.as_u64()) {
                    if overrides.is_some() {
                        return Err(BlockchainError::StateOverrideError(
                            "not available on past forked blocks".to_string(),
                        ))
                    }
                    return Ok(fork.call(&request, Some(number.into())).await?)
                }
            }
        }

        let fees = FeeDetails::new(request.gas_price)?.or_zero_fees();

        let (exit, out, gas, _) =
            self.backend.call(request, fees, Some(block_request), overrides).await?;
        trace!(target : "node", "Call status {:?}, gas {}", exit, gas);

        ensure_return_ok(exit, &out)
    }

    /// Estimate gas needed for execution of given contract.
    /// If no block parameter is given, it will use the pending block by default
    ///
    /// Handler for ETH RPC call: `eth_estimateGas`
    pub async fn estimate_gas(
        &self,
        request: EthTransactionRequest,
        block_number: Option<BlockId>,
    ) -> Result<U256> {
        node_info!("eth_estimateGas");
        self.do_estimate_gas(request, block_number.or_else(|| Some(BlockNumber::Pending.into())))
            .await
    }

    /// Get transaction by its hash.
    ///
    /// This will check the storage for a matching transaction, if no transaction exists in storage
    /// this will also scan the mempool for a matching pending transaction
    ///
    /// Handler for ETH RPC call: `eth_getTransactionByHash`
    pub async fn transaction_by_hash(&self, hash: H256) -> Result<Option<Transaction>> {
        node_info!("eth_getTransactionByHash");
        let mut tx = self.pool.get_transaction(hash).map(|pending| {
            let from = *pending.sender();
            let mut tx = transaction_build(Some(*pending.hash()), pending.transaction, None, None);
            // we set the from field here explicitly to the set sender of the pending transaction,
            // in case the transaction is impersonated.
            tx.from = from;
            tx
        });
        if tx.is_none() {
            tx = self.backend.transaction_by_hash(hash).await?
        }

        Ok(tx)
    }

    /// Returns transaction at given block hash and index.
    ///
    /// Handler for ETH RPC call: `eth_getTransactionByBlockHashAndIndex`
    pub async fn transaction_by_block_hash_and_index(
        &self,
        hash: H256,
        index: Index,
    ) -> Result<Option<Transaction>> {
        node_info!("eth_getTransactionByBlockHashAndIndex");
        self.backend.transaction_by_block_hash_and_index(hash, index).await
    }

    /// Returns transaction by given block number and index.
    ///
    /// Handler for ETH RPC call: `eth_getTransactionByBlockNumberAndIndex`
    pub async fn transaction_by_block_number_and_index(
        &self,
        block: BlockNumber,
        idx: Index,
    ) -> Result<Option<Transaction>> {
        node_info!("eth_getTransactionByBlockNumberAndIndex");
        self.backend.transaction_by_block_number_and_index(block, idx).await
    }

    /// Returns transaction receipt by transaction hash.
    ///
    /// Handler for ETH RPC call: `eth_getTransactionReceipt`
    pub async fn transaction_receipt(&self, hash: H256) -> Result<Option<TransactionReceipt>> {
        node_info!("eth_getTransactionReceipt");
        let tx = self.pool.get_transaction(hash);
        if tx.is_some() {
            return Ok(None)
        }
        self.backend.transaction_receipt(hash).await
    }

    /// Returns an uncles at given block and index.
    ///
    /// Handler for ETH RPC call: `eth_getUncleByBlockHashAndIndex`
    pub async fn uncle_by_block_hash_and_index(
        &self,
        block_hash: H256,
        idx: Index,
    ) -> Result<Option<Block<TxHash>>> {
        node_info!("eth_getUncleByBlockHashAndIndex");
        let number = self.backend.ensure_block_number(Some(BlockId::Hash(block_hash))).await?;
        if let Some(fork) = self.get_fork() {
            if fork.predates_fork_inclusive(number) {
                return Ok(fork.uncle_by_block_hash_and_index(block_hash, idx.into()).await?)
            }
        }
        // It's impossible to have uncles outside of fork mode
        Ok(None)
    }

    /// Returns an uncles at given block and index.
    ///
    /// Handler for ETH RPC call: `eth_getUncleByBlockNumberAndIndex`
    pub async fn uncle_by_block_number_and_index(
        &self,
        block_number: BlockNumber,
        idx: Index,
    ) -> Result<Option<Block<TxHash>>> {
        node_info!("eth_getUncleByBlockNumberAndIndex");
        let number = self.backend.ensure_block_number(Some(BlockId::Number(block_number))).await?;
        if let Some(fork) = self.get_fork() {
            if fork.predates_fork_inclusive(number) {
                return Ok(fork.uncle_by_block_number_and_index(number, idx.into()).await?)
            }
        }
        // It's impossible to have uncles outside of fork mode
        Ok(None)
    }

    /// Returns logs matching given filter object.
    ///
    /// Handler for ETH RPC call: `eth_getLogs`
    pub async fn logs(&self, filter: Filter) -> Result<Vec<Log>> {
        node_info!("eth_getLogs");
        self.backend.logs(filter).await
    }

    /// Returns the hash of the current block, the seedHash, and the boundary condition to be met.
    ///
    /// Handler for ETH RPC call: `eth_getWork`
    pub fn work(&self) -> Result<Work> {
        node_info!("eth_getWork");
        Err(BlockchainError::RpcUnimplemented)
    }

    /// Returns the sync status, always be fails.
    ///
    /// Handler for ETH RPC call: `eth_syncing`
    pub fn syncing(&self) -> Result<bool> {
        node_info!("eth_syncing");
        Ok(false)
    }

    /// Used for submitting a proof-of-work solution.
    ///
    /// Handler for ETH RPC call: `eth_submitWork`
    pub fn submit_work(&self, _: H64, _: H256, _: H256) -> Result<bool> {
        node_info!("eth_submitWork");
        Err(BlockchainError::RpcUnimplemented)
    }

    /// Used for submitting mining hashrate.
    ///
    /// Handler for ETH RPC call: `eth_submitHashrate`
    pub fn submit_hashrate(&self, _: U256, _: H256) -> Result<bool> {
        node_info!("eth_submitHashrate");
        Err(BlockchainError::RpcUnimplemented)
    }

    /// Creates a filter object, based on filter options, to notify when the state changes (logs).
    ///
    /// Handler for ETH RPC call: `eth_newFilter`
    pub async fn new_filter(&self, filter: Filter) -> Result<String> {
        node_info!("eth_newFilter");
        // all logs that are already available that match the filter if the filter's block range is
        // in the past
        let historic = if filter.block_option.get_from_block().is_some() {
            self.backend.logs(filter.clone()).await?
        } else {
            vec![]
        };
        let filter = EthFilter::Logs(Box::new(LogsFilter {
            blocks: self.new_block_notifications(),
            storage: self.storage_info(),
            filter: FilteredParams::new(Some(filter)),
            historic: Some(historic),
        }));
        Ok(self.filters.add_filter(filter).await)
    }

    /// Creates a filter in the node, to notify when a new block arrives.
    ///
    /// Handler for ETH RPC call: `eth_newBlockFilter`
    pub async fn new_block_filter(&self) -> Result<String> {
        node_info!("eth_newBlockFilter");
        let filter = EthFilter::Blocks(self.new_block_notifications());
        Ok(self.filters.add_filter(filter).await)
    }

    /// Creates a filter in the node, to notify when new pending transactions arrive.
    ///
    /// Handler for ETH RPC call: `eth_newPendingTransactionFilter`
    pub async fn new_pending_transaction_filter(&self) -> Result<String> {
        node_info!("eth_newPendingTransactionFilter");
        let filter = EthFilter::PendingTransactions(self.new_ready_transactions());
        Ok(self.filters.add_filter(filter).await)
    }

    /// Polling method for a filter, which returns an array of logs which occurred since last poll.
    ///
    /// Handler for ETH RPC call: `eth_getFilterChanges`
    pub async fn get_filter_changes(&self, id: &str) -> ResponseResult {
        node_info!("eth_getFilterChanges");
        self.filters.get_filter_changes(id).await
    }

    /// Returns an array of all logs matching filter with given id.
    ///
    /// Handler for ETH RPC call: `eth_getFilterLogs`
    pub async fn get_filter_logs(&self, id: &str) -> Result<Vec<Log>> {
        node_info!("eth_getFilterLogs");
        if let Some(filter) = self.filters.get_log_filter(id).await {
            self.backend.logs(filter).await
        } else {
            Ok(Vec::new())
        }
    }

    /// Handler for ETH RPC call: `eth_uninstallFilter`
    pub async fn uninstall_filter(&self, id: &str) -> Result<bool> {
        node_info!("eth_uninstallFilter");
        Ok(self.filters.uninstall_filter(id).await.is_some())
    }

    /// Returns traces for the transaction hash for geth's tracing endpoint
    ///
    /// Handler for RPC call: `debug_traceTransaction`
    pub async fn debug_trace_transaction(
        &self,
        tx_hash: H256,
        opts: GoCoreDebugTracingOptions,
    ) -> Result<GoCoreTrace> {
        node_info!("debug_traceTransaction");
        if opts.tracer.is_some() {
            return Err(RpcError::invalid_params("non-default tracer not supported yet").into())
        }

        self.backend.debug_trace_transaction(tx_hash, opts).await
    }

    /// Returns traces for the transaction for geth's tracing endpoint
    ///
    /// Handler for RPC call: `debug_traceCall`
    pub async fn debug_trace_call(
        &self,
        request: EthTransactionRequest,
        block_number: Option<BlockId>,
        opts: GoCoreDebugTracingOptions,
    ) -> Result<DefaultFrame> {
        node_info!("debug_traceCall");
        if opts.tracer.is_some() {
            return Err(RpcError::invalid_params("non-default tracer not supported yet").into())
        }
        let block_request = self.block_request(block_number).await?;
        let fees = FeeDetails::new(request.gas_price)?.or_zero_fees();

        self.backend.call_with_tracing(request, fees, Some(block_request), opts).await
    }

    /// Returns traces for the transaction hash via parity's tracing endpoint
    ///
    /// Handler for RPC call: `trace_transaction`
    pub async fn trace_transaction(&self, tx_hash: H256) -> Result<Vec<Trace>> {
        node_info!("trace_transaction");
        self.backend.trace_transaction(tx_hash).await
    }

    /// Returns traces for the transaction hash via parity's tracing endpoint
    ///
    /// Handler for RPC call: `trace_block`
    pub async fn trace_block(&self, block: BlockNumber) -> Result<Vec<Trace>> {
        node_info!("trace_block");
        self.backend.trace_block(block).await
    }
}

// == impl EthApi anvil endpoints ==

impl EthApi {
    /// Send transactions impersonating specific account and contract addresses.
    ///
    /// Handler for ETH RPC call: `anvil_impersonateAccount`
    pub async fn anvil_impersonate_account(&self, address: Address) -> Result<()> {
        node_info!("anvil_impersonateAccount");
        self.backend.impersonate(address).await?;
        Ok(())
    }

    /// Stops impersonating an account if previously set with `anvil_impersonateAccount`.
    ///
    /// Handler for ETH RPC call: `anvil_stopImpersonatingAccount`
    pub async fn anvil_stop_impersonating_account(&self, address: Address) -> Result<()> {
        node_info!("anvil_stopImpersonatingAccount");
        self.backend.stop_impersonating(address).await?;
        Ok(())
    }

    /// If set to true will make every account impersonated
    ///
    /// Handler for ETH RPC call: `anvil_autoImpersonateAccount`
    pub async fn anvil_auto_impersonate_account(&self, enabled: bool) -> Result<()> {
        node_info!("anvil_autoImpersonateAccount");
        self.backend.auto_impersonate_account(enabled).await?;
        Ok(())
    }

    /// Returns true if auto mining is enabled, and false.
    ///
    /// Handler for ETH RPC call: `anvil_getAutomine`
    pub fn anvil_get_auto_mine(&self) -> Result<bool> {
        node_info!("anvil_getAutomine");
        Ok(self.miner.is_auto_mine())
    }

    /// Enables or disables, based on the single boolean argument, the automatic mining of new
    /// blocks with each new transaction submitted to the network.
    ///
    /// Handler for ETH RPC call: `evm_setAutomine`
    pub async fn anvil_set_auto_mine(&self, enable_automine: bool) -> Result<()> {
        node_info!("evm_setAutomine");
        if self.miner.is_auto_mine() {
            if enable_automine {
                return Ok(())
            }
            self.miner.set_mining_mode(MiningMode::None);
        } else if enable_automine {
            let listener = self.pool.add_ready_listener();
            let mode = MiningMode::instant(1_000, listener);
            self.miner.set_mining_mode(mode);
        }
        Ok(())
    }

    /// Mines a series of blocks.
    ///
    /// Handler for ETH RPC call: `anvil_mine`
    pub async fn anvil_mine(&self, num_blocks: Option<U256>, interval: Option<U256>) -> Result<()> {
        node_info!("anvil_mine");
        let interval = interval.map(|i| i.as_u64());
        let blocks = num_blocks.unwrap_or_else(U256::one);
        if blocks == U256::zero() {
            return Ok(())
        }

        // mine all the blocks
        for _ in 0..blocks.as_u64() {
            self.mine_one().await;

            if let Some(interval) = interval {
                tokio::time::sleep(Duration::from_secs(interval)).await;
            }
        }

        Ok(())
    }

    /// Sets the mining behavior to interval with the given interval (seconds)
    ///
    /// Handler for ETH RPC call: `evm_setIntervalMining`
    pub fn anvil_set_interval_mining(&self, secs: u64) -> Result<()> {
        node_info!("evm_setIntervalMining");
        let mining_mode = if secs == 0 {
            MiningMode::None
        } else {
            let block_time = Duration::from_secs(secs);

            // This ensures that memory limits are stricter in interval-mine mode
            self.backend.update_interval_mine_block_time(block_time);

            MiningMode::FixedBlockTime(FixedBlockTimeMiner::new(block_time))
        };
        self.miner.set_mining_mode(mining_mode);
        Ok(())
    }

    /// Removes transactions from the pool
    ///
    /// Handler for RPC call: `anvil_dropTransaction`
    pub async fn anvil_drop_transaction(&self, tx_hash: H256) -> Result<Option<H256>> {
        node_info!("anvil_dropTransaction");
        Ok(self.pool.drop_transaction(tx_hash).map(|tx| *tx.hash()))
    }

    /// Reset the fork to a fresh forked state, and optionally update the fork config.
    ///
    /// If `forking` is `None` then this will disable forking entirely.
    ///
    /// Handler for RPC call: `anvil_reset`
    pub async fn anvil_reset(&self, forking: Option<Forking>) -> Result<()> {
        node_info!("anvil_reset");
        if let Some(forking) = forking {
            self.backend.reset_fork(forking).await
        } else {
            Err(BlockchainError::RpcUnimplemented)
        }
    }

    /// Modifies the balance of an account.
    ///
    /// Handler for RPC call: `anvil_setBalance`
    pub async fn anvil_set_balance(&self, address: Address, balance: U256) -> Result<()> {
        node_info!("anvil_setBalance");
        self.backend.set_balance(address, balance).await?;
        Ok(())
    }

    /// Sets the code of a contract.
    ///
    /// Handler for RPC call: `anvil_setCode`
    pub async fn anvil_set_code(&self, address: Address, code: Bytes) -> Result<()> {
        node_info!("anvil_setCode");
        self.backend.set_code(address, code).await?;
        Ok(())
    }

    /// Sets the nonce of an address.
    ///
    /// Handler for RPC call: `anvil_setNonce`
    pub async fn anvil_set_nonce(&self, address: Address, nonce: U256) -> Result<()> {
        node_info!("anvil_setNonce");
        self.backend.set_nonce(address, nonce).await?;
        Ok(())
    }

    /// Writes a single slot of the account's storage.
    ///
    /// Handler for RPC call: `anvil_setStorageAt`
    pub async fn anvil_set_storage_at(
        &self,
        address: Address,
        slot: U256,
        val: H256,
    ) -> Result<bool> {
        node_info!("anvil_setStorageAt");
        self.backend.set_storage_at(address, slot, val).await?;
        Ok(true)
    }

    /// Enable or disable logging.
    ///
    /// Handler for RPC call: `anvil_setLoggingEnabled`
    pub async fn anvil_set_logging(&self, enable: bool) -> Result<()> {
        node_info!("anvil_setLoggingEnabled");
        self.logger.set_enabled(enable);
        Ok(())
    }

    /// Set the minimum gas price for the node.
    ///
    /// Handler for RPC call: `anvil_setMinGasPrice`
    pub async fn anvil_set_min_gas_price(&self, gas: U256) -> Result<()> {
        node_info!("anvil_setMinGasPrice");
        self.backend.set_gas_price(gas);
        Ok(())
    }

    /// Sets the coinbase address.
    ///
    /// Handler for RPC call: `anvil_setCoinbase`
    pub async fn anvil_set_coinbase(&self, address: Address) -> Result<()> {
        node_info!("anvil_setCoinbase");
        self.backend.set_coinbase(address);
        Ok(())
    }

    /// Create a bufer that represents all state on the chain, which can be loaded to separate
    /// process by calling `anvil_loadState`
    ///
    /// Handler for RPC call: `anvil_dumpState`
    pub async fn anvil_dump_state(&self) -> Result<Bytes> {
        node_info!("anvil_dumpState");
        self.backend.dump_state().await
    }

    /// Returns the current state
    pub async fn serialized_state(&self) -> Result<SerializableState> {
        self.backend.serialized_state().await
    }

    /// Append chain state buffer to current chain. Will overwrite any conflicting addresses or
    /// storage.
    ///
    /// Handler for RPC call: `anvil_loadState`
    pub async fn anvil_load_state(&self, buf: Bytes) -> Result<bool> {
        node_info!("anvil_loadState");
        self.backend.load_state(buf).await
    }

    /// Retrieves the Anvil node configuration params.
    ///
    /// Handler for RPC call: `anvil_nodeInfo`
    pub async fn anvil_node_info(&self) -> Result<NodeInfo> {
        node_info!("anvil_nodeInfo");

        let env = self.backend.env().read();
        let fork_config = self.backend.get_fork();
        let tx_order = self.transaction_order.read();

        Ok(NodeInfo {
            current_block_number: self.backend.best_number(),
            current_block_timestamp: env.block.timestamp.try_into().unwrap_or(u64::MAX),
            current_block_hash: self.backend.best_hash(),
            hard_fork: env.cfg.spec_id,
            transaction_order: match *tx_order {
                TransactionOrder::Fifo => "fifo".to_string(),
                TransactionOrder::Fees => "fees".to_string(),
            },
            environment: NodeEnvironment {
                chain_id: self.backend.chain_id(),
                gas_limit: self.backend.gas_limit(),
                gas_price: self.backend.gas_price(),
            },
            fork_config: fork_config
                .map(|fork| {
                    let config = fork.config.read();

                    NodeForkConfig {
                        fork_url: Some(config.eth_rpc_url.clone()),
                        fork_block_number: Some(config.block_number),
                        fork_retry_backoff: Some(config.backoff.as_millis()),
                    }
                })
                .unwrap_or_default(),
        })
    }

    /// Snapshot the state of the blockchain at the current block.
    ///
    /// Handler for RPC call: `evm_snapshot`
    pub async fn evm_snapshot(&self) -> Result<U256> {
        node_info!("evm_snapshot");
        Ok(self.backend.create_snapshot().await)
    }

    /// Revert the state of the blockchain to a previous snapshot.
    /// Takes a single parameter, which is the snapshot id to revert to.
    ///
    /// Handler for RPC call: `evm_revert`
    pub async fn evm_revert(&self, id: U256) -> Result<bool> {
        node_info!("evm_revert");
        self.backend.revert_snapshot(id).await
    }

    /// Jump forward in time by the given amount of time, in seconds.
    ///
    /// Handler for RPC call: `evm_increaseTime`
    pub async fn evm_increase_time(&self, seconds: U256) -> Result<i64> {
        node_info!("evm_increaseTime");
        Ok(self.backend.time().increase_time(seconds.try_into().unwrap_or(u64::MAX)) as i64)
    }

    /// Similar to `evm_increaseTime` but takes the exact timestamp that you want in the next block
    ///
    /// Handler for RPC call: `evm_setNextBlockTimestamp`
    pub fn evm_set_next_block_timestamp(&self, seconds: u64) -> Result<()> {
        node_info!("evm_setNextBlockTimestamp");
        self.backend.time().set_next_block_timestamp(seconds)
    }

    /// Sets the specific timestamp and returns the number of seconds between the given timestamp
    /// and the current time.
    ///
    /// Handler for RPC call: `evm_setTime`
    pub fn evm_set_time(&self, timestamp: u64) -> Result<u64> {
        node_info!("evm_setTime");
        let now = self.backend.time().current_call_timestamp();
        self.backend.time().reset(timestamp);

        // number of seconds between the given timestamp and the current time.
        let offset = timestamp.saturating_sub(now);
        Ok(Duration::from_millis(offset).as_secs())
    }

    /// Set the next block gas limit
    ///
    /// Handler for RPC call: `evm_setBlockGasLimit`
    pub fn evm_set_block_gas_limit(&self, gas_limit: U256) -> Result<bool> {
        node_info!("evm_setBlockGasLimit");
        self.backend.set_gas_limit(gas_limit);
        Ok(true)
    }

    /// Sets an interval for the block timestamp
    ///
    /// Handler for RPC call: `anvil_setBlockTimestampInterval`
    pub fn evm_set_block_timestamp_interval(&self, seconds: u64) -> Result<()> {
        node_info!("anvil_setBlockTimestampInterval");
        self.backend.time().set_block_timestamp_interval(seconds);
        Ok(())
    }

    /// Sets an interval for the block timestamp
    ///
    /// Handler for RPC call: `anvil_removeBlockTimestampInterval`
    pub fn evm_remove_block_timestamp_interval(&self) -> Result<bool> {
        node_info!("anvil_removeBlockTimestampInterval");
        Ok(self.backend.time().remove_block_timestamp_interval())
    }

    /// Mine blocks, instantly.
    ///
    /// Handler for RPC call: `evm_mine`
    ///
    /// This will mine the blocks regardless of the configured mining mode.
    /// **Note**: ganache returns `0x0` here as placeholder for additional meta-data in the future.
    pub async fn evm_mine(&self, opts: Option<EvmMineOptions>) -> Result<String> {
        node_info!("evm_mine");

        self.do_evm_mine(opts).await?;

        Ok("0x0".to_string())
    }

    /// Mine blocks, instantly and return the mined blocks.
    ///
    /// Handler for RPC call: `evm_mine_detailed`
    ///
    /// This will mine the blocks regardless of the configured mining mode.
    ///
    /// **Note**: This behaves exactly as [Self::evm_mine] but returns different output, for
    /// compatibility reasons, this is a separate call since `evm_mine` is not an anvil original.
    /// and `ganache` may change the `0x0` placeholder.
    pub async fn evm_mine_detailed(
        &self,
        opts: Option<EvmMineOptions>,
    ) -> Result<Vec<Block<Transaction>>> {
        node_info!("evm_mine_detailed");

        let mined_blocks = self.do_evm_mine(opts).await?;

        let mut blocks = Vec::with_capacity(mined_blocks as usize);

        let latest = self.backend.best_number().as_u64();
        for offset in (0..mined_blocks).rev() {
            let block_num = latest - offset;
            if let Some(mut block) =
                self.backend.block_by_number_full(BlockNumber::Number(block_num.into())).await?
            {
                for tx in block.transactions.iter_mut() {
                    if let Some(receipt) = self.backend.mined_transaction_receipt(tx.hash) {
                        #[allow(unreachable_code)]
                        if let Some(_output) = receipt.out {
                            todo!("CORETODO: Handle this: anvil/src/eth/api.rs");
                            // insert revert reason if failure
                            if receipt.inner.status.unwrap_or_default().as_u64() == 0 {
                                if let Some(_reason) = decode_revert_reason(&_output) {

                                    // tx.other.insert(
                                    //     "revertReason".to_string(),
                                    //     serde_json::to_value(reason).expect("Infallible"),
                                    // );
                                }
                            }
                            // tx.other.insert(
                            //     "output".to_string(),
                            //     serde_json::to_value(output).expect("Infallible"),
                            // );
                        }
                    }
                }
                blocks.push(block);
            }
        }

        Ok(blocks)
    }

    /// Sets the reported block number
    ///
    /// Handler for ETH RPC call: `anvil_setBlock`
    pub fn anvil_set_block(&self, block_number: U256) -> Result<()> {
        node_info!("anvil_setBlock");
        self.backend.set_block_number(block_number);
        Ok(())
    }

    /// Sets the backend rpc url
    ///
    /// Handler for ETH RPC call: `anvil_setRpcUrl`
    pub fn anvil_set_rpc_url(&self, url: String) -> Result<()> {
        node_info!("anvil_setRpcUrl");
        if let Some(fork) = self.backend.get_fork() {
            let mut config = fork.config.write();
            let interval = config.provider.get_interval();
            let new_provider = Arc::new(
                ProviderBuilder::new(&url)
                    .max_retry(10)
                    .initial_backoff(1000)
                    .build()
                    .map_err(|_| {
                        ProviderError::CustomError(format!("Failed to parse invalid url {url}"))
                    })?
                    .interval(interval),
            );
            config.provider = new_provider;
            trace!(target: "backend", "Updated fork rpc from \"{}\" to \"{}\"", config.eth_rpc_url, url);
            config.eth_rpc_url = url;
        }
        Ok(())
    }

    /// Turn on call traces for transactions that are returned to the user when they execute a
    /// transaction (instead of just txhash/receipt)
    ///
    /// Handler for ETH RPC call: `anvil_enableTraces`
    pub async fn anvil_enable_traces(&self) -> Result<()> {
        node_info!("anvil_enableTraces");
        Err(BlockchainError::RpcUnimplemented)
    }

    /// Execute a transaction regardless of signature status
    ///
    /// Handler for ETH RPC call: `eth_sendUnsignedTransaction`
    pub async fn eth_send_unsigned_transaction(
        &self,
        request: EthTransactionRequest,
    ) -> Result<TxHash> {
        node_info!("eth_sendUnsignedTransaction");
        // either use the impersonated account of the request's `from` field
        let from = request.from.ok_or(BlockchainError::NoSignerAvailable)?;

        let (nonce, on_chain_nonce) = self.request_nonce(&request, from).await?;

        let request = self.build_typed_tx_request(request, nonce)?;

        let bypass_signature = self.backend.cheats().bypass_signature();
        let transaction = sign::build_typed_transaction(request, bypass_signature)?;

        self.ensure_typed_transaction_supported(&transaction)?;

        let pending_transaction = PendingTransaction::with_impersonated(transaction, from);

        // pre-validate
        self.backend.validate_pool_transaction(&pending_transaction).await?;

        let requires = required_marker(nonce, on_chain_nonce, from);
        let provides = vec![to_marker(nonce.as_u64(), from)];

        self.add_pending_transaction(pending_transaction, requires, provides)
    }

    /// Returns the number of transactions currently pending for inclusion in the next block(s), as
    /// well as the ones that are being scheduled for future execution only.
    /// Ref: [Here](https://geth.ethereum.org/docs/rpc/ns-txpool#txpool_status)
    ///
    /// Handler for ETH RPC call: `txpool_status`
    pub async fn txpool_status(&self) -> Result<TxpoolStatus> {
        node_info!("txpool_status");
        Ok(self.pool.txpool_status())
    }

    /// Returns a summary of all the transactions currently pending for inclusion in the next
    /// block(s), as well as the ones that are being scheduled for future execution only.
    ///
    /// See [here](https://geth.ethereum.org/docs/rpc/ns-txpool#txpool_inspect) for more details
    ///
    /// Handler for ETH RPC call: `txpool_inspect`
    pub async fn txpool_inspect(&self) -> Result<TxpoolInspect> {
        node_info!("txpool_inspect");
        let mut inspect = TxpoolInspect::default();

        fn convert(tx: Arc<PoolTransaction>) -> TxpoolInspectSummary {
            let tx = &tx.pending_transaction.transaction;
            let to = tx.to().copied();
            let gas_price = tx.gas_price();
            let value = tx.value();
            let gas = tx.gas_limit();
            TxpoolInspectSummary { to, value, energy: gas, energy_price: gas_price }
        }

        // Note: naming differs geth vs anvil:
        //
        // _Pending transactions_ are transactions that are ready to be processed and included in
        // the block. _Queued transactions_ are transactions where the transaction nonce is
        // not in sequence. The transaction nonce is an incrementing number for each transaction
        // with the same From address.
        for pending in self.pool.ready_transactions() {
            let entry = inspect.pending.entry(*pending.pending_transaction.sender()).or_default();
            let key = pending.pending_transaction.nonce().to_string();
            entry.insert(key, convert(pending));
        }
        for queued in self.pool.pending_transactions() {
            let entry = inspect.pending.entry(*queued.pending_transaction.sender()).or_default();
            let key = queued.pending_transaction.nonce().to_string();
            entry.insert(key, convert(queued));
        }
        Ok(inspect)
    }

    /// Returns the details of all transactions currently pending for inclusion in the next
    /// block(s), as well as the ones that are being scheduled for future execution only.
    ///
    /// See [here](https://geth.ethereum.org/docs/rpc/ns-txpool#txpool_content) for more details
    ///
    /// Handler for ETH RPC call: `txpool_inspect`
    pub async fn txpool_content(&self) -> Result<TxpoolContent> {
        node_info!("txpool_content");
        let mut content = TxpoolContent::default();
        fn convert(tx: Arc<PoolTransaction>) -> Transaction {
            let from = *tx.pending_transaction.sender();
            let mut tx = transaction_build(
                Some(*tx.hash()),
                tx.pending_transaction.transaction.clone(),
                None,
                None,
            );

            // we set the from field here explicitly to the set sender of the pending transaction,
            // in case the transaction is impersonated.
            tx.from = from;
            tx
        }

        for pending in self.pool.ready_transactions() {
            let entry = content.pending.entry(*pending.pending_transaction.sender()).or_default();
            let key = pending.pending_transaction.nonce().to_string();
            entry.insert(key, convert(pending));
        }
        for queued in self.pool.pending_transactions() {
            let entry = content.pending.entry(*queued.pending_transaction.sender()).or_default();
            let key = queued.pending_transaction.nonce().to_string();
            entry.insert(key, convert(queued));
        }

        Ok(content)
    }
}

// === impl EthApi utility functions ===

impl EthApi {
    /// Executes the `evm_mine` and returns the number of blocks mined
    async fn do_evm_mine(&self, opts: Option<EvmMineOptions>) -> Result<u64> {
        let mut blocks_to_mine = 1u64;

        if let Some(opts) = opts {
            let timestamp = match opts {
                EvmMineOptions::Timestamp(timestamp) => timestamp,
                EvmMineOptions::Options { timestamp, blocks } => {
                    if let Some(blocks) = blocks {
                        blocks_to_mine = blocks;
                    }
                    timestamp
                }
            };
            if let Some(timestamp) = timestamp {
                // timestamp was explicitly provided to be the next timestamp
                self.evm_set_next_block_timestamp(timestamp)?;
            }
        }

        // mine all the blocks
        for _ in 0..blocks_to_mine {
            self.mine_one().await;
        }

        Ok(blocks_to_mine)
    }

    async fn do_estimate_gas(
        &self,
        request: EthTransactionRequest,
        block_number: Option<BlockId>,
    ) -> Result<U256> {
        let block_request = self.block_request(block_number).await?;
        // check if the number predates the fork, if in fork mode
        if let BlockRequest::Number(number) = &block_request {
            if let Some(fork) = self.get_fork() {
                if fork.predates_fork(number.as_u64()) {
                    return Ok(fork.estimate_gas(&request, Some(number.into())).await?)
                }
            }
        }

        self.backend
            .with_database_at(Some(block_request), |state, block| {
                self.do_estimate_gas_with_state(request, state, block)
            })
            .await?
    }

    /// Estimates the gas usage of the `request` with the state.
    ///
    /// This will execute the [EthTransactionRequest] and find the best gas limit via binary search
    fn do_estimate_gas_with_state<D>(
        &self,
        mut request: EthTransactionRequest,
        state: D,
        block_env: BlockEnv,
    ) -> Result<U256>
    where
        D: DatabaseRef<Error = DatabaseError>,
    {
        // if the request is a simple transfer we can optimize
        let likely_transfer =
            request.data.as_ref().map(|data| data.as_ref().is_empty()).unwrap_or(true);
        if likely_transfer {
            if let Some(to) = request.to {
                if let Ok(target_code) = self.backend.get_code_with_state(&state, to) {
                    if target_code.as_ref().is_empty() {
                        return Ok(MIN_TRANSACTION_GAS)
                    }
                }
            }
        }

        let fees = FeeDetails::new(request.gas_price)?.or_zero_fees();

        // get the highest possible gas limit, either the request's set value or the currently
        // configured gas limit
        let mut highest_gas_limit = request.gas.unwrap_or(block_env.energy_limit.to_ethers_u256());

        // check with the funds of the sender
        if let Some(from) = request.from {
            let gas_price = fees.gas_price.unwrap_or_default();
            if gas_price > U256::zero() {
                let mut available_funds = self.backend.get_balance_with_state(&state, from)?;
                if let Some(value) = request.value {
                    if value > available_funds {
                        return Err(InvalidTransactionError::InsufficientFunds.into())
                    }
                    // safe: value < available_funds
                    available_funds -= value;
                }
                // amount of gas the sender can afford with the `gas_price`
                let allowance = available_funds.checked_div(gas_price).unwrap_or_default();
                if highest_gas_limit > allowance {
                    trace!(target: "node", "eth_estimateGas capped by limited user funds");
                    highest_gas_limit = allowance;
                }
            }
        }

        // if the provided gas limit is less than computed cap, use that
        let gas_limit = std::cmp::min(request.gas.unwrap_or(highest_gas_limit), highest_gas_limit);
        let mut call_to_estimate = request.clone();
        call_to_estimate.gas = Some(gas_limit);

        // execute the call without writing to db
        let ethres =
            self.backend.call_with_state(&state, call_to_estimate, fees.clone(), block_env.clone());

        // Exceptional case: init used too much gas, we need to increase the gas limit and try
        // again
        if let Err(BlockchainError::InvalidTransaction(InvalidTransactionError::GasTooHigh)) =
            ethres
        {
            // if price or limit was included in the request then we can execute the request
            // again with the block's gas limit to check if revert is gas related or not
            if request.gas.is_some() || request.gas_price.is_some() {
                return Err(map_out_of_gas_err(
                    request,
                    state,
                    self.backend.clone(),
                    block_env,
                    fees,
                    gas_limit,
                ))
            }
        }

        let (exit, out, gas, _) = ethres?;
        match exit {
            return_ok!() => {
                // succeeded
            }
            InstructionResult::OutOfEnergy | InstructionResult::OutOfFund => {
                return Err(InvalidTransactionError::BasicOutOfGas(gas_limit).into())
            }
            // need to check if the revert was due to lack of gas or unrelated reason
            return_revert!() => {
                // if price or limit was included in the request then we can execute the request
                // again with the max gas limit to check if revert is gas related or not
                return if request.gas.is_some() || request.gas_price.is_some() {
                    Err(map_out_of_gas_err(
                        request,
                        state,
                        self.backend.clone(),
                        block_env,
                        fees,
                        gas_limit,
                    ))
                } else {
                    // the transaction did fail due to lack of gas from the user
                    Err(InvalidTransactionError::Revert(Some(convert_transact_out(&out))).into())
                }
            }
            reason => {
                warn!(target: "node", "estimation failed due to {:?}", reason);
                return Err(BlockchainError::EvmError(reason))
            }
        }

        // at this point we know the call succeeded but want to find the _best_ (lowest) gas the
        // transaction succeeds with. we find this by doing a binary search over the
        // possible range NOTE: this is the gas the transaction used, which is less than the
        // transaction requires to succeed
        let gas: U256 = gas.into();
        // Get the starting lowest gas needed depending on the transaction kind.
        let mut lowest_gas_limit = determine_base_gas_by_kind(request.clone());

        // pick a point that's close to the estimated gas
        let mut mid_gas_limit = std::cmp::min(gas * 3, (highest_gas_limit + lowest_gas_limit) / 2);

        // Binary search for the ideal gas limit
        while (highest_gas_limit - lowest_gas_limit) > U256::one() {
            request.gas = Some(mid_gas_limit);
            let ethres = self.backend.call_with_state(
                &state,
                request.clone(),
                fees.clone(),
                block_env.clone(),
            );

            // Exceptional case: init used too much gas, we need to increase the gas limit and try
            // again
            if let Err(BlockchainError::InvalidTransaction(InvalidTransactionError::GasTooHigh)) =
                ethres
            {
                // increase the lowest gas limit
                lowest_gas_limit = mid_gas_limit;

                // new midpoint
                mid_gas_limit = (highest_gas_limit + lowest_gas_limit) / 2;
                continue
            }

            match ethres {
                Ok((exit, _, _gas, _)) => match exit {
                    // If the transaction succeeded, we can set a ceiling for the highest gas limit
                    // at the current midpoint, as spending any more gas would
                    // make no sense (as the TX would still succeed).
                    return_ok!() => {
                        highest_gas_limit = mid_gas_limit;
                    }
                    // If the transaction failed due to lack of gas, we can set a floor for the
                    // lowest gas limit at the current midpoint, as spending any
                    // less gas would make no sense (as the TX would still revert due to lack of
                    // gas).
                    InstructionResult::Revert |
                    InstructionResult::OutOfEnergy |
                    InstructionResult::OutOfFund => {
                        lowest_gas_limit = mid_gas_limit;
                    }
                    // The tx failed for some other reason.
                    reason => {
                        warn!(target: "node", "estimation failed due to {:?}", reason);
                        return Err(BlockchainError::EvmError(reason))
                    }
                },
                // We've already checked for the exceptional GasTooHigh case above, so this is a
                // real error.
                Err(reason) => {
                    warn!(target: "node", "estimation failed due to {:?}", reason);
                    return Err(reason)
                }
            }
            // new midpoint
            mid_gas_limit = (highest_gas_limit + lowest_gas_limit) / 2;
        }

        trace!(target : "node", "Estimated Gas for call {:?}", highest_gas_limit);

        Ok(highest_gas_limit)
    }

    /// Updates the `TransactionOrder`
    pub fn set_transaction_order(&self, order: TransactionOrder) {
        *self.transaction_order.write() = order;
    }

    /// Returns the priority of the transaction based on the current `TransactionOrder`
    fn transaction_priority(&self, tx: &TypedTransaction) -> TransactionPriority {
        self.transaction_order.read().priority(tx)
    }

    /// Returns the chain ID used for transaction
    pub fn chain_id(&self) -> u64 {
        self.backend.chain_id().as_u64()
    }

    pub fn get_fork(&self) -> Option<&ClientFork> {
        self.backend.get_fork()
    }

    /// Returns the first signer that can sign for the given address
    #[allow(clippy::borrowed_box)]
    pub fn get_signer(&self, address: Address) -> Option<&Box<dyn Signer>> {
        self.signers.iter().find(|signer| signer.is_signer_for(address))
    }

    /// Returns a new block event stream that yields Notifications when a new block was added
    pub fn new_block_notifications(&self) -> NewBlockNotifications {
        self.backend.new_block_notifications()
    }

    /// Returns a new listeners for ready transactions
    pub fn new_ready_transactions(&self) -> Receiver<TxHash> {
        self.pool.add_ready_listener()
    }

    /// Returns a new accessor for certain storage elements
    pub fn storage_info(&self) -> StorageInfo {
        StorageInfo::new(Arc::clone(&self.backend))
    }

    /// Returns true if forked
    pub fn is_fork(&self) -> bool {
        self.backend.is_fork()
    }

    /// Mines exactly one block
    pub async fn mine_one(&self) {
        let transactions = self.pool.ready_transactions().collect::<Vec<_>>();
        let outcome = self.backend.mine_block(transactions).await;

        trace!(target: "node", blocknumber = ?outcome.block_number, "mined block");
        self.pool.on_mined_block(outcome);
    }

    /// Returns the pending block with tx hashes
    async fn pending_block(&self) -> Block<TxHash> {
        let transactions = self.pool.ready_transactions().collect::<Vec<_>>();
        let info = self.backend.pending_block(transactions).await;
        self.backend.convert_block(info.block)
    }

    /// Returns the full pending block with `Transaction` objects
    async fn pending_block_full(&self) -> Option<Block<Transaction>> {
        let transactions = self.pool.ready_transactions().collect::<Vec<_>>();
        let BlockInfo { block, transactions, receipts: _ } =
            self.backend.pending_block(transactions).await;

        let corebc_block = self.backend.convert_block(block.clone());

        let mut block_transactions = Vec::with_capacity(block.transactions.len());

        for info in transactions {
            let tx = block.transactions.get(info.transaction_index as usize)?.clone();

            let tx = transaction_build(Some(info.transaction_hash), tx, Some(&block), Some(info));
            block_transactions.push(tx);
        }

        Some(corebc_block.into_full_block(block_transactions))
    }

    fn build_typed_tx_request(
        &self,
        request: EthTransactionRequest,
        nonce: U256,
    ) -> Result<TypedTransactionRequest> {
        let chain_id = request.network_id.map(|c| c.as_u64()).unwrap_or_else(|| self.chain_id());
        let gas_price = request.gas_price;

        let gas_limit = request.gas.map(Ok).unwrap_or_else(|| self.current_gas_limit())?;

        let request = match request.into_typed_request() {
            Some(TypedTransactionRequest::Legacy(mut m)) => {
                m.nonce = nonce;
                m.network_id = Some(chain_id);
                m.gas_limit = gas_limit;
                if gas_price.is_none() {
                    m.gas_price = self.gas_price().unwrap_or_default();
                }
                TypedTransactionRequest::Legacy(m)
            }
            _ => return Err(BlockchainError::FailedToDecodeTransaction),
        };
        Ok(request)
    }

    /// Returns true if the `addr` is currently impersonated
    pub fn is_impersonated(&self, addr: Address) -> bool {
        self.backend.cheats().is_impersonated(addr)
    }

    /// Returns the nonce of the `address` depending on the `block_number`
    async fn get_transaction_count(
        &self,
        address: Address,
        block_number: Option<BlockId>,
    ) -> Result<U256> {
        let block_request = self.block_request(block_number).await?;

        if let BlockRequest::Number(number) = &block_request {
            if let Some(fork) = self.get_fork() {
                if fork.predates_fork_inclusive(number.as_u64()) {
                    return Ok(fork.get_nonce(address, number.as_u64()).await?)
                }
            }
        }

        let nonce = self.backend.get_nonce(address, Some(block_request)).await?;

        Ok(nonce)
    }

    /// Returns the nonce for this request
    ///
    /// This returns a tuple of `(request nonce, highest nonce)`
    /// If the nonce field of the `request` is `None` then the tuple will be `(highest nonce,
    /// highest nonce)`.
    ///
    /// This will also check the tx pool for pending transactions from the sender.
    async fn request_nonce(
        &self,
        request: &EthTransactionRequest,
        from: Address,
    ) -> Result<(U256, U256)> {
        let highest_nonce =
            self.get_transaction_count(from, Some(BlockId::Number(BlockNumber::Pending))).await?;
        let nonce = request.nonce.unwrap_or(highest_nonce);

        Ok((nonce, highest_nonce))
    }

    /// Adds the given transaction to the pool
    fn add_pending_transaction(
        &self,
        pending_transaction: PendingTransaction,
        requires: Vec<TxMarker>,
        provides: Vec<TxMarker>,
    ) -> Result<TxHash> {
        let from = *pending_transaction.sender();
        let priority = self.transaction_priority(&pending_transaction.transaction);
        let pool_transaction =
            PoolTransaction { requires, provides, pending_transaction, priority };
        let tx = self.pool.add_transaction(pool_transaction)?;
        trace!(target: "node", "Added transaction: [{:?}] sender={:?}", tx.hash(), from);
        Ok(*tx.hash())
    }

    /// Returns the current state root
    pub async fn state_root(&self) -> Option<H256> {
        self.backend.get_db().read().await.maybe_state_root()
    }

    /// additional validation against hardfork
    fn ensure_typed_transaction_supported(&self, tx: &TypedTransaction) -> Result<()> {
        match &tx {
            TypedTransaction::Legacy(_) => Ok(()),
        }
    }
}

fn required_marker(provided_nonce: U256, on_chain_nonce: U256, from: Address) -> Vec<TxMarker> {
    if provided_nonce == on_chain_nonce {
        return Vec::new()
    }
    let prev_nonce = provided_nonce.saturating_sub(U256::one());
    if on_chain_nonce <= prev_nonce {
        vec![to_marker(prev_nonce.as_u64(), from)]
    } else {
        Vec::new()
    }
}

fn convert_transact_out(out: &Option<Output>) -> Bytes {
    match out {
        None => Default::default(),
        Some(Output::Call(out)) => out.to_vec().into(),
        Some(Output::Create(out, _)) => out.to_vec().into(),
    }
}

/// Returns an error if the `exit` code is _not_ ok
fn ensure_return_ok(exit: InstructionResult, out: &Option<Output>) -> Result<Bytes> {
    let out = convert_transact_out(out);
    match exit {
        return_ok!() => Ok(out),
        return_revert!() => Err(InvalidTransactionError::Revert(Some(out)).into()),
        reason => Err(BlockchainError::EvmError(reason)),
    }
}

/// Executes the requests again after an out of gas error to check if the error is gas related or
/// not
#[inline]
fn map_out_of_gas_err<D>(
    mut request: EthTransactionRequest,
    state: D,
    backend: Arc<backend::mem::Backend>,
    block_env: BlockEnv,
    fees: FeeDetails,
    gas_limit: U256,
) -> BlockchainError
where
    D: DatabaseRef<Error = DatabaseError>,
{
    request.gas = Some(backend.gas_limit());
    let (exit, out, _, _) = match backend.call_with_state(&state, request, fees, block_env) {
        Ok(res) => res,
        Err(err) => return err,
    };
    match exit {
        return_ok!() => {
            // transaction succeeded by manually increasing the gas limit to
            // highest, which means the caller lacks funds to pay for the tx
            InvalidTransactionError::BasicOutOfGas(gas_limit).into()
        }
        return_revert!() => {
            // reverted again after bumping the limit
            InvalidTransactionError::Revert(Some(convert_transact_out(&out))).into()
        }
        reason => {
            warn!(target: "node", "estimation failed due to {:?}", reason);
            BlockchainError::EvmError(reason)
        }
    }
}

/// Determines the minimum gas needed for a transaction depending on the transaction kind.
#[inline]
fn determine_base_gas_by_kind(request: EthTransactionRequest) -> U256 {
    match request.into_typed_request() {
        Some(request) => match request {
            TypedTransactionRequest::Legacy(req) => match req.kind {
                TransactionKind::Call(_) => MIN_TRANSACTION_GAS,
                TransactionKind::Create => MIN_CREATE_GAS,
            },
        },
        // Tighten the gas limit upwards if we don't know the transaction type to avoid deployments
        // failing.
        _ => MIN_CREATE_GAS,
    }
}
