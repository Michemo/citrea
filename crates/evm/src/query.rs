use std::collections::BTreeMap;
use std::ops::{Range, RangeInclusive};

use alloy_consensus::Eip658Value;
use alloy_eips::eip2930::AccessListWithGasUsed;
use alloy_primitives::Uint;
use alloy_rlp::Encodable;
use jsonrpsee::core::RpcResult;
use reth_primitives::constants::GWEI_TO_WEI;
use reth_primitives::TxKind::{Call, Create};
use reth_primitives::{
    Block, BlockId, BlockNumberOrTag, SealedHeader, TransactionSignedEcRecovered, U256, U64,
};
use reth_provider::ProviderError;
use reth_rpc_eth_types::error::{EthApiError, EthResult, RevertError, RpcInvalidTransactionError};
use reth_rpc_types::trace::geth::{GethDebugTracingOptions, GethTrace};
use reth_rpc_types::{
    AnyReceiptEnvelope, AnyTransactionReceipt, Log, OtherFields, ReceiptWithBloom,
    TransactionReceipt,
};
use reth_rpc_types_compat::block::from_primitive_with_hash;
use revm::primitives::{
    CfgEnvWithHandlerCfg, EVMError, ExecutionResult, HaltReason, InvalidTransaction, TransactTo,
    TxEnv, KECCAK_EMPTY,
};
use revm::{Database, DatabaseCommit};
use revm_inspectors::access_list::AccessListInspector;
use revm_inspectors::tracing::{TracingInspector, TracingInspectorConfig};
use serde::{Deserialize, Serialize};
use sov_modules_api::macros::rpc_gen;
use sov_modules_api::prelude::*;
use sov_modules_api::WorkingSet;
use tracing::debug;

use crate::call::get_cfg_env;
use crate::conversions::create_tx_env;
use crate::error::rpc::ensure_success;
use crate::evm::call::prepare_call_env;
use crate::evm::db::EvmDb;
use crate::evm::primitive_types::{BlockEnv, Receipt, SealedBlock, TransactionSignedAndRecovered};
use crate::handler::TxInfo;
use crate::rpc_helpers::*;
use crate::{BloomFilter, Evm, EvmChainConfig, FilterBlockOption, FilterError};

/// Gas per transaction not creating a contract.
pub const MIN_TRANSACTION_GAS: u64 = 21_000u64;

/// https://github.com/paradigmxyz/reth/pull/7133/files
/// Allowed error ratio for gas estimation
/// Taken from Geth's implementation in order to pass the hive tests
/// <https://github.com/ethereum/go-ethereum/blob/a5a4fa7032bb248f5a7c40f4e8df2b131c4186a4/internal/ethapi/api.go#L56>
const ESTIMATE_GAS_ERROR_RATIO: f64 = 0.015;

/// The result of gas/diffsize estimation.
/// This struct holds estimated gas and l1_fee_overhead.
/// This is very useful for users to test their balance after calling to `eth_estimateGas`
/// whether they can afford to execute a transaction.
#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub(crate) struct EstimatedTxExpenses {
    /// Evm gas used.
    gas_used: U64,
    /// Base fee of the L2 block when tx was executed.
    base_fee: U256,
    /// L1 fee.
    l1_fee: U256,
    /// L1 diff size.
    l1_diff_size: u64,
}

impl EstimatedTxExpenses {
    /// Return total estimated gas used including evm gas and L1 fee.
    pub(crate) fn gas_with_l1_overhead(&self) -> U256 {
        // Actually not an L1 fee but l1_fee / base_fee.
        let l1_fee_overhead =
            U256::from(1).max(self.l1_fee / (self.base_fee + U256::from(GWEI_TO_WEI))); // assume 1 gwei priority fee
        l1_fee_overhead + U256::from(self.gas_used)
    }
}

/// Result of estimation of diff size.
#[derive(Clone, Default, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EstimatedDiffSize {
    /// Gas used.
    pub gas: U64,
    /// Diff size.
    pub l1_diff_size: U64,
}

#[rpc_gen(client, server)]
impl<C: sov_modules_api::Context> Evm<C> {
    /// Handler for `net_version`
    #[rpc_method(name = "net_version")]
    pub fn net_version(&self, working_set: &mut WorkingSet<C>) -> RpcResult<String> {
        debug!("evm module: net_version");

        // Network ID is the same as chain ID for most networks
        let chain_id = self
            .cfg
            .get(working_set)
            .expect("EVM config must be set at genesis")
            .chain_id;

        Ok(chain_id.to_string())
    }

    /// Handler for: `eth_chainId`
    #[rpc_method(name = "eth_chainId")]
    pub fn chain_id(&self, working_set: &mut WorkingSet<C>) -> RpcResult<Option<U64>> {
        tracing::debug!("evm module: eth_chainId");

        let chain_id = reth_primitives::U64::from(
            self.cfg
                .get(working_set)
                .expect("EVM config must be set at genesis")
                .chain_id,
        );
        debug!("evm module: eth_chainId() -> {}", chain_id);

        Ok(Some(chain_id))
    }

    /// Handler for `eth_getBlockByHash`
    #[rpc_method(name = "eth_getBlockByHash")]
    pub fn get_block_by_hash(
        &self,
        block_hash: reth_primitives::B256,
        details: Option<bool>,
        working_set: &mut WorkingSet<C>,
    ) -> RpcResult<Option<reth_rpc_types::RichBlock>> {
        debug!("evm module: eth_getBlockByHash");

        // if block hash is not known, return None
        let block_number = match self
            .block_hashes
            .get(&block_hash, &mut working_set.accessory_state())
        {
            Some(block_number) => block_number,
            None => return Ok(None),
        };

        self.get_block_by_number(
            Some(BlockNumberOrTag::Number(block_number)),
            details,
            working_set,
        )
    }

    /// Handler for: `eth_getBlockByNumber`
    #[rpc_method(name = "eth_getBlockByNumber")]
    pub fn get_block_by_number(
        &self,
        block_number: Option<BlockNumberOrTag>,
        details: Option<bool>,
        working_set: &mut WorkingSet<C>,
    ) -> RpcResult<Option<reth_rpc_types::RichBlock>> {
        debug!("evm module: eth_getBlockByNumber");

        let sealed_block = match self.get_sealed_block_by_number(block_number, working_set)? {
            Some(sealed_block) => sealed_block,
            None => return Ok(None), // if block doesn't exist return null
        };

        // Build rpc header response
        let mut header = from_primitive_with_hash(sealed_block.header.clone());
        header.total_difficulty = Some(header.difficulty);
        // Collect transactions with ids from db
        let transactions: Vec<TransactionSignedAndRecovered> = sealed_block
            .transactions
            .clone()
            .map(|id| {
                self.transactions
                    .get(id as usize, &mut working_set.accessory_state())
                    .expect("Transaction must be set")
            })
            .collect();

        let block = Block {
            header: sealed_block.header.header().clone(),
            body: transactions
                .iter()
                .map(|tx| tx.signed_transaction.clone())
                .collect(),
            ommers: Default::default(),
            withdrawals: Default::default(),
            requests: None,
        };

        let size = block.length();

        // Build rpc transactions response
        let transactions = match details {
            Some(true) => reth_rpc_types::BlockTransactions::Full(
                transactions
                    .iter()
                    .enumerate()
                    .map(|(id, tx)| {
                        reth_rpc_types_compat::transaction::from_recovered_with_block_context(
                            tx.clone().into(),
                            header.hash.expect("Block must be already sealed"),
                            header.number.expect("Block must be already sealed"),
                            header.base_fee_per_gas.map(|bfpg| bfpg.try_into().unwrap()), // u64 max is 18446744073 gwei, for the conversion to fail the base fee per gas would have to be higher than that
                            id,
                        )
                    })
                    .collect::<Vec<_>>(),
            ),
            _ => reth_rpc_types::BlockTransactions::Hashes({
                transactions
                    .iter()
                    .map(|tx| tx.signed_transaction.hash)
                    .collect::<Vec<_>>()
            }),
        };

        // Build rpc block response
        let block = reth_rpc_types::Block {
            header,
            size: Some(U256::from(size)),
            uncles: Default::default(),
            transactions,
            withdrawals: Default::default(),
            other: OtherFields::new(BTreeMap::<String, _>::from([
                (
                    "l1FeeRate".to_string(),
                    serde_json::json!(sealed_block.l1_fee_rate),
                ),
                (
                    "l1Hash".to_string(),
                    serde_json::json!(sealed_block.l1_hash),
                ),
            ])),
        };

        Ok(Some(block.into()))
    }

    /// Handler for: `eth_getBlockReceipts`
    #[rpc_method(name = "eth_getBlockReceipts")]
    pub fn get_block_receipts(
        &self,
        block_number_or_hash: BlockId,
        working_set: &mut WorkingSet<C>,
    ) -> RpcResult<Option<Vec<AnyTransactionReceipt>>> {
        debug!("evm module: eth_getBlockReceipts");

        let block = match block_number_or_hash {
            BlockId::Hash(block_hash) => {
                let block_number = match self
                    .block_hashes
                    .get(&block_hash.block_hash, &mut working_set.accessory_state())
                {
                    Some(block_number) => block_number,
                    None => return Ok(None), // if hash is unknown, return None
                };

                // if hash is known, but we don't have the block, fail
                self.blocks
                    .get(block_number as usize, &mut working_set.accessory_state())
                    .expect("Block must be set")
            }
            BlockId::Number(block_number) => {
                match self.get_sealed_block_by_number(Some(block_number), working_set)? {
                    Some(block) => block,
                    None => return Ok(None), // if block doesn't exist return null
                }
            }
        };

        let receipts = &block
            .transactions
            .clone()
            .map(|id| {
                let tx = self
                    .transactions
                    .get(id as usize, &mut working_set.accessory_state())
                    .expect("Transaction must be set");

                let receipt = self
                    .receipts
                    .get(id as usize, &mut working_set.accessory_state())
                    .expect("Receipt for known transaction must be set");

                build_rpc_receipt(&block, tx, id, receipt)
            })
            .collect::<Vec<_>>();

        Ok(Some(receipts.clone()))
    }

    /// Handler for: `eth_getBalance`
    #[rpc_method(name = "eth_getBalance")]
    pub fn get_balance(
        &self,
        address: reth_primitives::Address,
        block_number: Option<BlockNumberOrTag>,
        working_set: &mut WorkingSet<C>,
    ) -> RpcResult<reth_primitives::U256> {
        debug!("evm module: eth_getBalance");

        let curr_block_number = self
            .blocks
            .last(&mut working_set.accessory_state())
            .expect("Head block must be set")
            .header
            .number;

        // Specs from https://ethereum.org/en/developers/docs/apis/json-rpc
        match block_number {
            Some(BlockNumberOrTag::Number(num)) => {
                if num > curr_block_number {
                    return Err(EthApiError::UnknownBlockNumber.into());
                }
                set_state_to_end_of_evm_block(num, working_set);
            }
            // Working state here is already at the latest state, so no need to anything
            Some(BlockNumberOrTag::Latest) | Some(BlockNumberOrTag::Pending) | None => {}
            Some(BlockNumberOrTag::Earliest) => {
                set_state_to_end_of_evm_block(0, working_set);
            }
            _ => {
                return Err(EthApiError::InvalidParams(
                    "Please provide a number or earliest/latest tag".to_string(),
                )
                .into())
            }
        }

        let balance = self
            .accounts
            .get(&address, working_set)
            .map(|account| account.info.balance)
            .unwrap_or_default();

        Ok(balance)
    }

    /// Handler for: `eth_getStorageAt`
    #[rpc_method(name = "eth_getStorageAt")]
    pub fn get_storage_at(
        &self,
        address: reth_primitives::Address,
        index: reth_primitives::U256,
        block_number: Option<BlockNumberOrTag>,
        working_set: &mut WorkingSet<C>,
    ) -> RpcResult<reth_primitives::B256> {
        debug!("evm module: eth_getStorageAt");

        let curr_block_number = self
            .blocks
            .last(&mut working_set.accessory_state())
            .expect("Head block must be set")
            .header
            .number;

        // Specs from https://ethereum.org/en/developers/docs/apis/json-rpc
        match block_number {
            Some(BlockNumberOrTag::Number(num)) => {
                if num > curr_block_number {
                    return Err(EthApiError::UnknownBlockNumber.into());
                }
                set_state_to_end_of_evm_block(num, working_set);
            }
            // Working state here is already at the latest state, so no need to anything
            Some(BlockNumberOrTag::Latest) | Some(BlockNumberOrTag::Pending) | None => {}
            Some(BlockNumberOrTag::Earliest) => {
                set_state_to_end_of_evm_block(0, working_set);
            }
            _ => {
                return Err(EthApiError::InvalidParams(
                    "Please provide a number or earliest/latest tag".to_string(),
                )
                .into())
            }
        }

        let storage_slot = self
            .accounts
            .get(&address, working_set)
            .and_then(|account| account.storage.get(&index, working_set))
            .unwrap_or_default();

        Ok(storage_slot.into())
    }

    /// Handler for: `eth_getTransactionCount`
    #[rpc_method(name = "eth_getTransactionCount")]
    pub fn get_transaction_count(
        &self,
        address: reth_primitives::Address,
        block_number: Option<BlockNumberOrTag>,
        working_set: &mut WorkingSet<C>,
    ) -> RpcResult<reth_primitives::U64> {
        debug!("evm module: eth_getTransactionCount");

        let curr_block_number = self
            .blocks
            .last(&mut working_set.accessory_state())
            .expect("Head block must be set")
            .header
            .number;

        // Specs from https://ethereum.org/en/developers/docs/apis/json-rpc
        match block_number {
            Some(BlockNumberOrTag::Number(num)) => {
                if num > curr_block_number {
                    return Err(EthApiError::UnknownBlockNumber.into());
                }
                set_state_to_end_of_evm_block(num, working_set);
            }
            // Working state here is already at the latest state, so no need to anything
            Some(BlockNumberOrTag::Latest) | Some(BlockNumberOrTag::Pending) | None => {}
            Some(BlockNumberOrTag::Earliest) => {
                set_state_to_end_of_evm_block(0, working_set);
            }
            _ => {
                return Err(EthApiError::InvalidParams(
                    "Please provide a number or earliest/latest tag".to_string(),
                )
                .into())
            }
        }

        let nonce = self
            .accounts
            .get(&address, working_set)
            .map(|account| account.info.nonce)
            .unwrap_or_default();

        Ok(U64::from(nonce))
    }

    /// Handler for: `eth_getCode`
    #[rpc_method(name = "eth_getCode")]
    pub fn get_code(
        &self,
        address: reth_primitives::Address,
        block_number: Option<BlockNumberOrTag>,
        working_set: &mut WorkingSet<C>,
    ) -> RpcResult<reth_primitives::Bytes> {
        debug!("evm module: eth_getCode");

        let curr_block_number = self
            .blocks
            .last(&mut working_set.accessory_state())
            .expect("Head block must be set")
            .header
            .number;

        match block_number {
            Some(BlockNumberOrTag::Number(num)) => {
                if num > curr_block_number {
                    return Err(EthApiError::UnknownBlockNumber.into());
                }
                set_state_to_end_of_evm_block(num, working_set);
            }
            // Working state here is already at the latest state, so no need to anything
            Some(BlockNumberOrTag::Latest) | Some(BlockNumberOrTag::Pending) | None => {}
            Some(BlockNumberOrTag::Earliest) => {
                set_state_to_end_of_evm_block(0, working_set);
            }
            // Is this the way?
            // Note that reth works for all types of BlockNumberOrTag
            _ => {
                return Err(EthApiError::InvalidParams(
                    "Please provide a number or earliest/latest tag".to_string(),
                )
                .into())
            }
        }

        let code = self
            .accounts
            .get(&address, working_set)
            .and_then(|account| self.code.get(&account.info.code_hash, working_set))
            .unwrap_or_default();

        Ok(code.original_bytes())
    }

    /// Handler for: `eth_getTransactionByBlockHashAndIndex`
    #[rpc_method(name = "eth_getTransactionByBlockHashAndIndex")]
    pub fn get_transaction_by_block_hash_and_index(
        &self,
        block_hash: reth_primitives::B256,
        index: reth_primitives::U64,
        working_set: &mut WorkingSet<C>,
    ) -> RpcResult<Option<reth_rpc_types::Transaction>> {
        debug!("evm module: eth_getTransactionByBlockHashAndIndex");

        let mut accessory_state = working_set.accessory_state();

        let block_number = match self.block_hashes.get(&block_hash, &mut accessory_state) {
            Some(block_number) => block_number,
            None => return Ok(None),
        };

        let block = self
            .blocks
            .get(block_number as usize, &mut accessory_state)
            .expect("Block must be set");

        match check_tx_range(&block.transactions, index) {
            Some(_) => (),
            None => return Ok(None),
        }

        let tx_number = block.transactions.start + index.to::<u64>();

        let tx = self
            .transactions
            .get(tx_number as usize, &mut accessory_state)
            .expect("Transaction must be set");

        let block = self
            .blocks
            .get(tx.block_number as usize, &mut accessory_state)
            .expect("Block number for known transaction must be set");

        let transaction = reth_rpc_types_compat::transaction::from_recovered_with_block_context(
            tx.into(),
            block.header.hash(),
            block.header.number,
            block.header.base_fee_per_gas,
            (tx_number - block.transactions.start) as usize,
        );

        Ok(Some(transaction))
    }

    /// Handler for: `eth_getTransactionByBlockNumberAndIndex`
    #[rpc_method(name = "eth_getTransactionByBlockNumberAndIndex")]
    pub fn get_transaction_by_block_number_and_index(
        &self,
        block_number: BlockNumberOrTag,
        index: reth_primitives::U64,
        working_set: &mut WorkingSet<C>,
    ) -> RpcResult<Option<reth_rpc_types::Transaction>> {
        debug!("evm module: eth_getTransactionByBlockNumberAndIndex");

        let block_number = match self.block_number_for_id(&block_number, working_set) {
            Ok(block_number) => block_number,
            Err(EthApiError::UnknownBlockNumber) => return Ok(None),
            Err(err) => return Err(err.into()),
        };

        let block = self
            .blocks
            .get(block_number as usize, &mut working_set.accessory_state())
            .expect("Block must be set");

        match check_tx_range(&block.transactions, index) {
            Some(_) => (),
            None => return Ok(None),
        }

        let tx_number = block.transactions.start + index.to::<u64>();

        let tx = self
            .transactions
            .get(tx_number as usize, &mut working_set.accessory_state())
            .expect("Transaction must be set");

        let block = self
            .blocks
            .get(tx.block_number as usize, &mut working_set.accessory_state())
            .expect("Block number for known transaction must be set");

        let transaction = reth_rpc_types_compat::transaction::from_recovered_with_block_context(
            tx.into(),
            block.header.hash(),
            block.header.number,
            block.header.base_fee_per_gas,
            (tx_number - block.transactions.start) as usize,
        );

        Ok(Some(transaction))
    }

    /// Handler for: `eth_getTransactionReceipt`
    #[rpc_method(name = "eth_getTransactionReceipt")]
    pub fn get_transaction_receipt(
        &self,
        hash: reth_primitives::B256,
        working_set: &mut WorkingSet<C>,
    ) -> RpcResult<Option<AnyTransactionReceipt>> {
        debug!("evm module: eth_getTransactionReceipt");
        let mut accessory_state = working_set.accessory_state();

        let tx_number = self.transaction_hashes.get(&hash, &mut accessory_state);

        let receipt = tx_number.map(|number| {
            let tx = self
                .transactions
                .get(number as usize, &mut accessory_state)
                .expect("Transaction with known hash must be set");
            let block = self
                .blocks
                .get(tx.block_number as usize, &mut accessory_state)
                .expect("Block number for known transaction must be set");

            let receipt = self
                .receipts
                .get(number as usize, &mut accessory_state)
                .expect("Receipt for known transaction must be set");

            build_rpc_receipt(&block, tx, number, receipt)
        });

        Ok(receipt)
    }

    /// Handler for: `eth_call`
    //https://github.com/paradigmxyz/reth/blob/f577e147807a783438a3f16aad968b4396274483/crates/rpc/rpc/src/eth/api/transactions.rs#L502
    //https://github.com/paradigmxyz/reth/blob/main/crates/rpc/rpc-types/src/eth/call.rs#L7
    #[rpc_method(name = "eth_call")]
    pub fn get_call(
        &self,
        request: reth_rpc_types::TransactionRequest,
        block_number: Option<BlockNumberOrTag>,
        _state_overrides: Option<reth_rpc_types::state::StateOverride>,
        _block_overrides: Option<Box<reth_rpc_types::BlockOverrides>>,
        working_set: &mut WorkingSet<C>,
    ) -> RpcResult<reth_primitives::Bytes> {
        debug!("evm module: eth_call");
        let mut block_env = match block_number {
            None | Some(BlockNumberOrTag::Pending | BlockNumberOrTag::Latest) => {
                // if no block is produced yet, should default to genesis block env, else just return the lates
                self.block_env.get(working_set).unwrap_or_else(|| {
                    BlockEnv::from(
                        &self
                            .get_sealed_block_by_number(
                                Some(BlockNumberOrTag::Earliest),
                                working_set,
                            )
                            .unwrap()
                            .expect("Genesis block must be set"),
                    )
                })
            }
            _ => {
                let block = match self.get_sealed_block_by_number(block_number, working_set)? {
                    Some(block) => block,
                    None => return Err(EthApiError::UnknownBlockNumber.into()),
                };

                set_state_to_end_of_evm_block(block.header.number, working_set);

                BlockEnv::from(&block)
            }
        };

        let cfg = self
            .cfg
            .get(working_set)
            .expect("EVM chain config should be set");
        let mut cfg_env = get_cfg_env(&block_env, cfg);

        // set endpoint specific params
        cfg_env.disable_eip3607 = true;
        cfg_env.disable_base_fee = true;
        // set higher block gas limit than usual
        // but still cap it to prevent DoS
        block_env.gas_limit = 100_000_000;

        let mut evm_db = self.get_db(working_set);
        let mut tx_env = prepare_call_env(
            &block_env,
            request.clone(),
            Some(
                evm_db
                    .basic(request.from.unwrap_or_default())
                    .unwrap()
                    .unwrap_or_default()
                    .balance,
            ),
        )?;

        // https://github.com/paradigmxyz/reth/issues/6574
        tx_env.nonce = None;

        let result = match inspect(
            evm_db,
            cfg_env,
            block_env,
            tx_env,
            TracingInspector::new(TracingInspectorConfig::all()),
        ) {
            Ok(result) => result.result,
            Err(err) => {
                return Err(EthApiError::from(err).into());
            }
        };

        Ok(ensure_success(result)?)
    }

    /// Handler for: `eth_blockNumber`
    #[rpc_method(name = "eth_blockNumber")]
    pub fn block_number(
        &self,
        working_set: &mut WorkingSet<C>,
    ) -> RpcResult<reth_primitives::U256> {
        debug!("evm module: eth_blockNumber");

        let block_number = U256::from(
            self.blocks
                .len(&mut working_set.accessory_state())
                .saturating_sub(1),
        );
        Ok(block_number)
    }

    /// Handler for `eth_createAccessList`
    #[rpc_method(name = "eth_createAccessList")]
    pub fn create_access_list(
        &self,
        request: reth_rpc_types::TransactionRequest,
        block_number: Option<BlockNumberOrTag>,
        working_set: &mut WorkingSet<C>,
    ) -> RpcResult<AccessListWithGasUsed> {
        debug!("evm module: eth_createAccessList");

        let mut request = request.clone();

        let (l1_fee_rate, mut block_env) = match block_number {
            None | Some(BlockNumberOrTag::Pending | BlockNumberOrTag::Latest) => {
                // so we don't unnecessarily set archival version
                // if no block was produced yet, the l1 fee rate can unwrap to 0, we don't care, else just return the latest
                let l1_fee_rate = self.l1_fee_rate.get(working_set).unwrap_or_default();
                // if no block is produced yet, should default to genesis block env, else just return the lates
                let block_env = self.block_env.get(working_set).unwrap_or_else(|| {
                    BlockEnv::from(
                        &self
                            .get_sealed_block_by_number(
                                Some(BlockNumberOrTag::Earliest),
                                working_set,
                            )
                            .unwrap()
                            .expect("Genesis block must be set"),
                    )
                });
                (l1_fee_rate, block_env)
            }
            _ => {
                let block = match self.get_sealed_block_by_number(block_number, working_set)? {
                    Some(block) => block,
                    None => return Err(EthApiError::UnknownBlockNumber.into()),
                };

                set_state_to_end_of_evm_block(block.header.number, working_set);
                let l1_fee_rate = block.l1_fee_rate;
                let block_env = BlockEnv::from(&block);
                (l1_fee_rate, block_env)
            }
        };

        let cfg = self
            .cfg
            .get(working_set)
            .expect("EVM chain config should be set");
        let mut cfg_env = get_cfg_env(&block_env, cfg);

        // set endpoint specific params
        cfg_env.disable_eip3607 = true;
        cfg_env.disable_base_fee = true;
        // set higher block gas limit than usual
        // but still cap it to prevent DoS
        block_env.gas_limit = 100_000_000;

        let mut evm_db = self.get_db(working_set);

        let mut tx_env = prepare_call_env(
            &block_env,
            request.clone(),
            Some(
                evm_db
                    .basic(request.from.unwrap_or_default())
                    .unwrap()
                    .unwrap_or_default()
                    .balance,
            ),
        )?;

        let from = request.from.unwrap_or_default();
        let to = if let Some(Call(to)) = request.to {
            to
        } else {
            let account = evm_db.basic(from).unwrap();

            let nonce = account.unwrap_or_default().nonce;
            from.create(nonce)
        };

        // can consume the list since we're not using the request anymore
        let initial = request.access_list.take().unwrap_or_default();

        let precompiles = get_precompiles(cfg_env.handler_cfg.spec_id);
        let mut inspector = AccessListInspector::new(initial, from, to, precompiles);

        let result = inspect(
            &mut evm_db,
            cfg_env.clone(),
            block_env,
            tx_env.clone(),
            &mut inspector,
        )
        .map_err(EthApiError::from)?;

        match result.result {
            ExecutionResult::Halt { reason, .. } => Err(match reason {
                HaltReason::NonceOverflow => RpcInvalidTransactionError::NonceMaxValue,
                halt => RpcInvalidTransactionError::EvmHalt(halt),
            }),
            ExecutionResult::Revert { output, .. } => {
                Err(RpcInvalidTransactionError::Revert(RevertError::new(output)))
            }
            ExecutionResult::Success { .. } => Ok(()),
        }?;

        let access_list = inspector.into_access_list();

        request.access_list = Some(access_list.clone());
        tx_env.access_list = access_list.to_vec();

        let estimated = self.estimate_gas_with_env(
            request,
            l1_fee_rate,
            block_env,
            cfg_env,
            &mut tx_env,
            working_set,
        )?;

        Ok(AccessListWithGasUsed {
            access_list,
            gas_used: estimated.gas_with_l1_overhead(),
        })
    }

    // This is a common function for both eth_estimateGas and eth_estimateDiffSize.
    // The point of this function is to prepare env and call estimate_gas_with_env.
    fn estimate_tx_expenses(
        &self,
        request: reth_rpc_types::TransactionRequest,
        block_number: Option<BlockNumberOrTag>,
        working_set: &mut WorkingSet<C>,
    ) -> RpcResult<EstimatedTxExpenses> {
        let (l1_fee_rate, block_env) = match block_number {
            None | Some(BlockNumberOrTag::Pending | BlockNumberOrTag::Latest) => {
                // so we don't unnecessarily set archival version
                // if no block was produced yet, the l1 fee rate can unwrap to 0, we don't care, else just return the latest
                let l1_fee_rate = self.l1_fee_rate.get(working_set).unwrap_or_default();
                // if no block is produced yet, should default to genesis block env, else just return the lates
                let block_env = self.block_env.get(working_set).unwrap_or_else(|| {
                    BlockEnv::from(
                        &self
                            .get_sealed_block_by_number(
                                Some(BlockNumberOrTag::Earliest),
                                working_set,
                            )
                            .unwrap()
                            .expect("Genesis block must be set"),
                    )
                });
                (l1_fee_rate, block_env)
            }
            _ => {
                let block = match self.get_sealed_block_by_number(block_number, working_set)? {
                    Some(block) => block,
                    None => return Err(EthApiError::UnknownBlockNumber.into()),
                };

                set_state_to_end_of_evm_block(block.header.number, working_set);
                let l1_fee_rate = block.l1_fee_rate;
                let block_env = BlockEnv::from(&block);
                (l1_fee_rate, block_env)
            }
        };

        let mut tx_env = prepare_call_env(&block_env, request.clone(), None)?;

        let cfg = self
            .cfg
            .get(working_set)
            .expect("EVM chain config should be set");
        let mut cfg_env = get_cfg_env(&block_env, cfg);

        // set endpoint specific params
        cfg_env.disable_eip3607 = true;
        cfg_env.disable_base_fee = true;

        self.estimate_gas_with_env(
            request,
            l1_fee_rate,
            block_env,
            cfg_env,
            &mut tx_env,
            working_set,
        )
    }

    /// Handler for: `eth_estimateGas`
    // https://github.com/paradigmxyz/reth/blob/main/crates/rpc/rpc/src/eth/api/call.rs#L172
    #[rpc_method(name = "eth_estimateGas")]
    pub fn eth_estimate_gas(
        &self,
        request: reth_rpc_types::TransactionRequest,
        block_number: Option<BlockNumberOrTag>,
        working_set: &mut WorkingSet<C>,
    ) -> RpcResult<reth_primitives::U256> {
        debug!("evm module: eth_estimateGas");

        let estimated = self.estimate_tx_expenses(request, block_number, working_set)?;
        Ok(estimated.gas_with_l1_overhead())
    }

    /// Handler for: `eth_estimateDiffSize`
    #[rpc_method(name = "eth_estimateDiffSize")]
    pub fn eth_estimate_diff_size(
        &self,
        request: reth_rpc_types::TransactionRequest,
        block_number: Option<BlockNumberOrTag>,
        working_set: &mut WorkingSet<C>,
    ) -> RpcResult<EstimatedDiffSize> {
        debug!("evm module: eth_estimateDiffSize");
        if request.gas.is_none() {
            return Err(EthApiError::InvalidParams("gas must be set".into()))?;
        }
        let estimated = self.estimate_tx_expenses(request, block_number, working_set)?;

        Ok(EstimatedDiffSize {
            gas: estimated.gas_used,
            l1_diff_size: U64::from(estimated.l1_diff_size),
        })
    }

    /// Handler for: `eth_getBlockTransactionCountByHash`
    // https://github.com/paradigmxyz/reth/blob/main/crates/rpc/rpc/src/eth/api/call.rs#L172
    #[rpc_method(name = "eth_getBlockTransactionCountByHash")]
    pub fn eth_get_block_transaction_count_by_hash(
        &self,
        block_hash: reth_primitives::B256,
        working_set: &mut WorkingSet<C>,
    ) -> RpcResult<Option<reth_primitives::U256>> {
        debug!("evm module: eth_getBlockTransactionCountByHash");
        // Get the number of transactions in a block given blockhash
        let block = self.get_block_by_hash(block_hash, None, working_set)?;
        match block {
            Some(block) => Ok(Some(U256::from(block.transactions.len()))),
            None => Ok(None),
        }
    }

    /// Handler for: `eth_getBlockTransactionCountByNumber`
    #[rpc_method(name = "eth_getBlockTransactionCountByNumber")]
    pub fn eth_get_block_transaction_count_by_number(
        &self,
        block_number: BlockNumberOrTag,
        working_set: &mut WorkingSet<C>,
    ) -> RpcResult<Option<reth_primitives::U256>> {
        debug!("evm module: eth_getBlockTransactionCountByNumber");
        // Get the number of transactions in a block given block number
        let block = self.get_block_by_number(Some(block_number), None, working_set)?;
        match block {
            Some(block) => Ok(Some(U256::from(block.transactions.len()))),
            None => Ok(None),
        }
    }

    /// Inner gas estimator
    pub(crate) fn estimate_gas_with_env(
        &self,
        request: reth_rpc_types::TransactionRequest,
        l1_fee_rate: u128,
        block_env: BlockEnv,
        cfg_env: CfgEnvWithHandlerCfg,
        tx_env: &mut TxEnv,
        working_set: &mut WorkingSet<C>,
    ) -> RpcResult<EstimatedTxExpenses> {
        let request_gas = request.gas;
        let request_gas_price = request.gas_price;
        let env_gas_limit = block_env.gas_limit.into();
        let env_base_fee = U256::from(block_env.basefee);

        // get the highest possible gas limit, either the request's set value or the currently
        // configured gas limit
        let mut highest_gas_limit = request.gas.unwrap_or(env_gas_limit);

        let account = self
            .accounts
            .get(&tx_env.caller, working_set)
            .map(|account| account.info)
            .unwrap_or_default();

        // if the request is a simple transfer we can optimize
        if tx_env.data.is_empty() {
            if let TransactTo::Call(to) = tx_env.transact_to {
                let to_account = self
                    .accounts
                    .get(&to, working_set)
                    .map(|account| account.info)
                    .unwrap_or_default();
                if KECCAK_EMPTY == to_account.code_hash {
                    // If the tx is a simple transfer (call to an account with no code) we can
                    // shortcircuit But simply returning

                    // `MIN_TRANSACTION_GAS` is dangerous because there might be additional
                    // field combos that bump the price up, so we try executing the function
                    // with the minimum gas limit to make sure.

                    let mut tx_env = tx_env.clone();
                    tx_env.gas_limit = MIN_TRANSACTION_GAS;

                    let res = inspect_no_tracing(
                        self.get_db(working_set),
                        cfg_env.clone(),
                        block_env,
                        tx_env.clone(),
                        l1_fee_rate,
                    );

                    if let Ok((res, tx_info)) = res {
                        if res.result.is_success() {
                            return Ok(EstimatedTxExpenses {
                                gas_used: U64::from(MIN_TRANSACTION_GAS),
                                base_fee: env_base_fee,
                                l1_fee: tx_info.l1_fee,
                                l1_diff_size: tx_info.l1_diff_size,
                            });
                        }
                    }
                }
            }
        }

        // check funds of the sender
        if tx_env.gas_price > U256::ZERO {
            // allowance is (balance - tx.value) / tx.gas_price
            let allowance = ((account.balance - tx_env.value) / tx_env.gas_price).saturating_to();

            if highest_gas_limit > allowance {
                // cap the highest gas limit by max gas caller can afford with given gas price
                highest_gas_limit = allowance;
            }
        }

        // if the provided gas limit is less than computed cap, use that
        let gas_limit: u64 = std::cmp::min(tx_env.gas_limit, highest_gas_limit as u64); // highest_gas_limit is capped to u64::MAX
        tx_env.gas_limit = gas_limit;

        let evm_db = self.get_db(working_set);

        // execute the call without writing to db
        let result = inspect_no_tracing(
            evm_db,
            cfg_env.clone(),
            block_env,
            tx_env.clone(),
            l1_fee_rate,
        );

        // Exceptional case: init used too much gas, we need to increase the gas limit and try
        // again
        if let Err(EVMError::Transaction(InvalidTransaction::CallerGasLimitMoreThanBlock)) = result
        {
            // if price or limit was included in the request then we can execute the request
            // again with the block's gas limit to check if revert is gas related or not
            if request_gas.is_some() || request_gas_price.is_some() {
                let evm_db = self.get_db(working_set);
                return Err(map_out_of_gas_err(
                    block_env,
                    tx_env.clone(),
                    cfg_env,
                    evm_db,
                    l1_fee_rate,
                )
                .into());
            }
        }

        let (result, mut l1_fee, mut diff_size) = match result {
            Ok((result, tx_info)) => match result.result {
                ExecutionResult::Success { .. } => {
                    (result.result, tx_info.l1_fee, tx_info.l1_diff_size)
                }
                ExecutionResult::Halt { reason, gas_used } => {
                    return Err(RpcInvalidTransactionError::halt(reason, gas_used).into())
                }
                ExecutionResult::Revert { output, .. } => {
                    // if price or limit was included in the request then we can execute the request
                    // again with the block's gas limit to check if revert is gas related or not
                    return if request_gas.is_some() || request_gas_price.is_some() {
                        let evm_db = self.get_db(working_set);
                        Err(map_out_of_gas_err(
                            block_env,
                            tx_env.clone(),
                            cfg_env,
                            evm_db,
                            l1_fee_rate,
                        )
                        .into())
                    } else {
                        // the transaction did revert
                        Err(RpcInvalidTransactionError::Revert(RevertError::new(output)).into())
                    };
                }
            },
            Err(err) => return Err(EthApiError::from(err).into()),
        };

        // at this point we know the call succeeded but want to find the _best_ (lowest) gas the
        // transaction succeeds with. We find this by doing a binary search over the
        // possible range NOTE: this is the gas the transaction used, which is less than the
        // transaction requires to succeed
        let gas_used = result.gas_used();
        let mut highest_gas_limit: u64 = highest_gas_limit.try_into().unwrap_or(u64::MAX);

        // https://github.com/paradigmxyz/reth/pull/7133/files
        // the lowest value is capped by the gas used by the unconstrained transaction
        let mut lowest_gas_limit = gas_used.saturating_sub(1);

        let gas_refund = match result {
            ExecutionResult::Success { gas_refunded, .. } => gas_refunded,
            _ => 0,
        };
        // As stated in Geth, there is a good change that the transaction will pass if we set the
        // gas limit to the execution gas used plus the gas refund, so we check this first
        // <https://github.com/ethereum/go-ethereum/blob/a5a4fa7032bb248f5a7c40f4e8df2b131c4186a4/eth/gasestimator/gasestimator.go#L135
        let optimistic_gas_limit = (gas_used + gas_refund) * 64 / 63;
        if optimistic_gas_limit < highest_gas_limit {
            tx_env.gas_limit = optimistic_gas_limit;
            // (result, env) = executor::transact(&mut db, env)?;
            let curr_result = inspect_no_tracing(
                self.get_db(working_set),
                cfg_env.clone(),
                block_env,
                tx_env.clone(),
                l1_fee_rate,
            );
            let (curr_result, tx_info) = match curr_result {
                Ok(result) => result,
                Err(err) => return Err(EthApiError::from(err).into()),
            };
            update_estimated_gas_range(
                curr_result.result,
                optimistic_gas_limit,
                &mut highest_gas_limit,
                &mut lowest_gas_limit,
                &mut l1_fee,
                &mut diff_size,
                tx_info,
            )?;
        };

        // pick a point that's close to the estimated gas
        let mut mid_gas_limit = std::cmp::min(
            gas_used * 3,
            ((highest_gas_limit as u128 + lowest_gas_limit as u128) / 2) as u64,
        );
        // binary search
        while (highest_gas_limit - lowest_gas_limit) > 1 {
            // An estimation error is allowed once the current gas limit range used in the binary
            // search is small enough (less than 1.5% of the highest gas limit)
            // <https://github.com/ethereum/go-ethereum/blob/a5a4fa7032bb248f5a7c40f4e8df2b131c4186a4/eth/gasestimator/gasestimator.go#L152
            if (highest_gas_limit - lowest_gas_limit) as f64 / (highest_gas_limit as f64)
                < ESTIMATE_GAS_ERROR_RATIO
            {
                break;
            };

            let mut tx_env = tx_env.clone();
            tx_env.gas_limit = mid_gas_limit;

            let evm_db = self.get_db(working_set);
            let result = inspect_no_tracing(
                evm_db,
                cfg_env.clone(),
                block_env,
                tx_env.clone(),
                l1_fee_rate,
            );

            // Exceptional case: init used too much gas, we need to increase the gas limit and try
            // again
            if let Err(EVMError::Transaction(InvalidTransaction::CallerGasLimitMoreThanBlock)) =
                result
            {
                // increase the lowest gas limit
                lowest_gas_limit = mid_gas_limit;
            } else {
                let (result, tx_info) = match result {
                    Ok(result) => result,
                    Err(err) => return Err(EthApiError::from(err).into()),
                };

                update_estimated_gas_range(
                    result.result,
                    mid_gas_limit,
                    &mut highest_gas_limit,
                    &mut lowest_gas_limit,
                    &mut l1_fee,
                    &mut diff_size,
                    tx_info,
                )?;
            }

            // new midpoint
            mid_gas_limit = ((highest_gas_limit as u128 + lowest_gas_limit as u128) / 2) as u64;
        }

        Ok(EstimatedTxExpenses {
            gas_used: U64::from(highest_gas_limit),
            base_fee: env_base_fee,
            l1_fee,
            l1_diff_size: diff_size,
        })
    }

    /// Returns logs matching given filter object.
    ///
    /// Handler for `eth_getLogs`
    #[rpc_method(name = "eth_getLogs")]
    pub fn eth_get_logs(
        &self,
        filter: Filter,
        working_set: &mut WorkingSet<C>,
    ) -> RpcResult<Vec<LogResponse>> {
        // https://github.com/paradigmxyz/reth/blob/8892d04a88365ba507f28c3314d99a6b54735d3f/crates/rpc/rpc/src/eth/filter.rs#L302
        Ok(self.logs_for_filter(filter, working_set)?)
    }

    /// Handler for: `eth_getTransactionByHash`
    /// RPC method is moved to sequencer and ethereum-rpc modules
    pub fn get_transaction_by_hash(
        &self,
        hash: reth_primitives::B256,
        working_set: &mut WorkingSet<C>,
    ) -> RpcResult<Option<reth_rpc_types::Transaction>> {
        debug!("evm module: eth_getTransactionByHash({})", hash);
        let mut accessory_state = working_set.accessory_state();

        let tx_number = self.transaction_hashes.get(&hash, &mut accessory_state);

        let transaction = tx_number.map(|number| {
            let tx = self
                .transactions
                .get(number as usize, &mut accessory_state)
                .unwrap_or_else(|| panic!("Transaction with known hash {} and number {} must be set in all {} transaction",
                hash,
                number,
                self.transactions.len(&mut accessory_state)));

            let block = self
                .blocks
                .get(tx.block_number as usize, &mut accessory_state)
                .unwrap_or_else(|| panic!("Block with number {} for known transaction {} must be set",
                    tx.block_number,
                    tx.signed_transaction.hash));

            reth_rpc_types_compat::transaction::from_recovered_with_block_context(
                tx.into(),
                block.header.hash(),
                block.header.number,
                block.header.base_fee_per_gas,
                (number - block.transactions.start) as usize,
            )
        });

        Ok(transaction)
    }

    /// Traces the entire block txs and returns the traces
    pub fn trace_block_transactions_by_number(
        &self,
        block_number: u64,
        opts: Option<GethDebugTracingOptions>,
        stop_at: Option<usize>,
        working_set: &mut WorkingSet<C>,
    ) -> RpcResult<Vec<GethTrace>> {
        let sealed_block = self
            .get_sealed_block_by_number(Some(BlockNumberOrTag::Number(block_number)), working_set)?
            .ok_or_else(|| EthApiError::UnknownBlockNumber)?;

        let tx_range = sealed_block.transactions.clone();
        if tx_range.is_empty() {
            return Ok(Vec::new());
        }
        let block_txs: Vec<TransactionSignedEcRecovered> = tx_range
            .clone()
            .map(|id| {
                self.transactions
                    .get(id as usize, &mut working_set.accessory_state())
                    .expect("Transaction must be set")
                    .into()
            })
            .collect();

        // set state to end of the previous block
        set_state_to_end_of_evm_block(block_number - 1, working_set);

        let block_env = BlockEnv::from(&sealed_block);
        let cfg = self
            .cfg
            .get(working_set)
            .expect("EVM chain config should be set");
        let cfg_env = get_cfg_env(&block_env, cfg);
        let l1_fee_rate = sealed_block.l1_fee_rate;

        // EvmDB is the replacement of revm::CacheDB because cachedb requires immutable state
        // TODO: Move to CacheDB once immutable state is implemented
        let mut evm_db = self.get_db(working_set);

        // TODO: Convert below steps to blocking task like in reth after implementing the semaphores
        let mut traces = Vec::new();
        let mut transactions = block_txs.into_iter().enumerate().peekable();
        let limit = stop_at.unwrap_or(usize::MAX);
        while let Some((index, tx)) = transactions.next() {
            let (trace, state_changes) = trace_transaction(
                opts.clone().unwrap_or_default(),
                cfg_env.clone(),
                block_env,
                create_tx_env(&tx),
                tx.hash(),
                &mut evm_db,
                l1_fee_rate,
            )?;
            traces.push(trace);

            if limit == index {
                break;
            }

            if transactions.peek().is_some() {
                // need to apply the state changes of this transaction before executing the
                // next transaction
                evm_db.commit(state_changes)
            }
        }
        Ok(traces)
    }

    // https://github.com/paradigmxyz/reth/blob/8892d04a88365ba507f28c3314d99a6b54735d3f/crates/rpc/rpc/src/eth/filter.rs#L349
    fn logs_for_filter(
        &self,
        filter: Filter,
        working_set: &mut WorkingSet<C>,
    ) -> Result<Vec<LogResponse>, FilterError> {
        match filter.block_option {
            FilterBlockOption::AtBlockHash(block_hash) => {
                let block_number = match self
                    .block_hashes
                    .get(&block_hash, &mut working_set.accessory_state())
                {
                    Some(block_number) => block_number,
                    None => {
                        return Err(FilterError::EthAPIError(
                            ProviderError::BlockHashNotFound(block_hash).into(),
                        ))
                    }
                };

                // if we know the hash, but can't find the block, fail
                let block = self
                    .blocks
                    .get(block_number as usize, &mut working_set.accessory_state())
                    .expect("Block must be set");

                // all of the logs we have in the block
                let mut all_logs: Vec<LogResponse> = Vec::new();

                self.append_matching_block_logs(working_set, &mut all_logs, &filter, block);

                Ok(all_logs)
            }
            FilterBlockOption::Range {
                from_block,
                to_block,
            } => {
                // we start at the most recent block if unset in filter
                let start_block = self
                    .blocks
                    .last(&mut working_set.accessory_state())
                    .expect("Head block must be set")
                    .header
                    .number;
                let from = from_block
                    .map(|num| convert_block_number(num, start_block))
                    .transpose()?
                    .flatten();
                let to = to_block
                    .map(|num| convert_block_number(num, start_block))
                    .transpose()?
                    .flatten();
                let (from_block_number, to_block_number) =
                    get_filter_block_range(from, to, start_block);
                self.get_logs_in_block_range(
                    working_set,
                    &filter,
                    from_block_number,
                    to_block_number,
                )
            }
        }
    }

    // https://github.com/paradigmxyz/reth/blob/8892d04a88365ba507f28c3314d99a6b54735d3f/crates/rpc/rpc/src/eth/filter.rs#L423
    /// Returns all logs in the given _inclusive_ range that match the filter
    ///
    /// Returns an error if:
    ///  - underlying database error
    ///  - amount of matches exceeds configured limit
    pub fn get_logs_in_block_range(
        &self,
        working_set: &mut WorkingSet<C>,
        filter: &Filter,
        from_block_number: u64,
        to_block_number: u64,
    ) -> Result<Vec<LogResponse>, FilterError> {
        let max_blocks_per_filter: u64 = DEFAULT_MAX_BLOCKS_PER_FILTER;
        if to_block_number - from_block_number >= max_blocks_per_filter {
            return Err(FilterError::QueryExceedsMaxBlocks(max_blocks_per_filter));
        }
        // all of the logs we have in the block
        let mut all_logs: Vec<LogResponse> = Vec::new();

        let address_filter: BloomFilter = filter.address.to_bloom_filter();
        let topics_filter: Vec<BloomFilter> =
            filter.topics.iter().map(|t| t.to_bloom_filter()).collect();

        let max_headers_range = MAX_HEADERS_RANGE;

        // loop over the range of new blocks and check logs if the filter matches the log's bloom
        // filter
        for (from, to) in
            BlockRangeInclusiveIter::new(from_block_number..=to_block_number, max_headers_range)
        {
            for idx in from..=to {
                let block = match self
                    .blocks
                    .get((idx) as usize, &mut working_set.accessory_state())
                {
                    Some(block) => block,
                    None => {
                        return Err(FilterError::EthAPIError(
                            // from and to are checked against last block
                            // so this should never happen ideally
                            ProviderError::BlockBodyIndicesNotFound(idx).into(),
                        ));
                    }
                };

                let logs_bloom = block.header.logs_bloom;

                let alloy_logs_bloom = alloy_primitives::Bloom::from(logs_bloom.data());
                if matches_address(alloy_logs_bloom, &address_filter)
                    && matches_topics(alloy_logs_bloom, &topics_filter)
                {
                    self.append_matching_block_logs(working_set, &mut all_logs, filter, block);
                    let max_logs_per_response = DEFAULT_MAX_LOGS_PER_RESPONSE;
                    // size check but only if range is multiple blocks, so we always return all
                    // logs of a single block
                    let is_multi_block_range = from_block_number != to_block_number;
                    if is_multi_block_range && all_logs.len() > max_logs_per_response {
                        return Err(FilterError::QueryExceedsMaxResults(max_logs_per_response));
                    }
                }
            }
        }
        Ok(all_logs)
    }

    // https://github.com/paradigmxyz/reth/blob/main/crates/rpc/rpc/src/eth/logs_utils.rs#L21
    fn append_matching_block_logs(
        &self,
        working_set: &mut WorkingSet<C>,
        all_logs: &mut Vec<LogResponse>,
        filter: &Filter,
        block: SealedBlock,
    ) {
        // tracks the index of a log in the entire block
        let mut log_index: u32 = 0;

        // TODO: Understand how to handle this
        // TAG - true when the log was removed, due to a chain reorganization. false if its a valid log.
        let removed = false;

        let tx_range = block.transactions;

        for i in tx_range {
            let receipt = self
                .receipts
                .get(i as usize, &mut working_set.accessory_state())
                .expect("Transaction must be set");
            let tx = self
                .transactions
                .get(i as usize, &mut working_set.accessory_state())
                .unwrap();
            let logs = receipt.receipt.logs;

            for log in logs.into_iter() {
                if log_matches_filter(&log, filter, &block.header.hash(), &block.header.number) {
                    let log = LogResponse {
                        address: log.address,
                        topics: log.topics().to_vec(),
                        data: log.data.data.to_vec().into(),
                        block_hash: Some(block.header.hash()),
                        block_number: Some(U256::from(block.header.number)),
                        transaction_hash: Some(tx.signed_transaction.hash),
                        transaction_index: Some(U256::from(i)),
                        log_index: Some(U256::from(log_index)),
                        removed,
                    };
                    all_logs.push(log);
                }
                log_index += 1;
            }
        }
    }

    /// Helper function to get chain config
    pub fn get_chain_config(&self, working_set: &mut WorkingSet<C>) -> EvmChainConfig {
        self.cfg
            .get(working_set)
            .expect("EVM chain config should be set")
    }

    /// Helper function to get block hash from block number
    pub fn block_hash_from_number(
        &self,
        block_number: u64,
        working_set: &mut WorkingSet<C>,
    ) -> Option<reth_primitives::B256> {
        let block = self
            .blocks
            .get(block_number as usize, &mut working_set.accessory_state())?;
        Some(block.header.hash())
    }

    /// Helper function to get headers in range
    pub fn sealed_headers_range(
        &self,
        range: RangeInclusive<u64>,
        working_set: &mut WorkingSet<C>,
    ) -> Result<Vec<SealedHeader>, EthApiError> {
        let mut headers = Vec::new();
        for i in range {
            let block = self
                .blocks
                .get(i as usize, &mut working_set.accessory_state())
                .ok_or_else(|| EthApiError::InvalidBlockRange)?;
            headers.push(block.header);
        }
        Ok(headers)
    }

    /// Helper function to check if the block number is valid
    /// If returns None, block doesn't exist
    pub fn block_number_for_id(
        &self,
        block_id: &BlockNumberOrTag,
        working_set: &mut WorkingSet<C>,
    ) -> Result<u64, EthApiError> {
        match block_id {
            BlockNumberOrTag::Earliest => Ok(0),
            BlockNumberOrTag::Latest => Ok(self
                .blocks
                .last(&mut working_set.accessory_state())
                .map(|block| block.header.number)
                .expect("Head block must be set")),
            BlockNumberOrTag::Number(block_number) => {
                if *block_number < self.blocks.len(&mut working_set.accessory_state()) as u64 {
                    Ok(*block_number)
                } else {
                    Err(EthApiError::UnknownBlockNumber)
                }
            }
            _ => Err(EthApiError::InvalidParams(
                "Please provide a number or earliest/latest tag".to_string(),
            )),
        }
    }

    /// Helper function to get sealed block by number
    /// If returns None, block doesn't exist
    fn get_sealed_block_by_number(
        &self,
        block_number: Option<BlockNumberOrTag>,
        working_set: &mut WorkingSet<C>,
    ) -> Result<Option<SealedBlock>, EthApiError> {
        // safe, finalized, and pending are not supported
        match block_number {
            Some(BlockNumberOrTag::Number(block_number)) => Ok(self
                .blocks
                .get(block_number as usize, &mut working_set.accessory_state())),
            Some(BlockNumberOrTag::Earliest) => Ok(Some(
                self.blocks
                    .get(0, &mut working_set.accessory_state())
                    .expect("Genesis block must be set"),
            )),
            Some(BlockNumberOrTag::Latest) => Ok(Some(
                self.blocks
                    .last(&mut working_set.accessory_state())
                    .expect("Head block must be set"),
            )),
            None => Ok(Some(
                self.blocks
                    .last(&mut working_set.accessory_state())
                    .expect("Head block must be set"),
            )),
            _ => Err(EthApiError::InvalidParams(
                "pending/safe/finalized block not supported".to_string(),
            )),
        }
    }

    /// Returns the block number given block hash
    /// If block not found returns None
    pub fn get_block_number_by_block_hash(
        &self,
        block_hash: reth_primitives::B256,
        working_set: &mut WorkingSet<C>,
    ) -> Option<u64> {
        let block_number = self
            .block_hashes
            .get(&block_hash, &mut working_set.accessory_state());
        block_number
    }

    /// Returns the cumulative gas used in pending transactions
    /// Used to calculate how much gas system transactions use at the beginning of the block
    pub fn get_pending_txs_cumulative_gas_used(&self, working_set: &mut WorkingSet<C>) -> u128 {
        self.pending_transactions
            .iter(working_set)
            .map(|tx| tx.receipt.gas_used)
            .sum::<u128>()
    }
}

// modified from: https://github.com/paradigmxyz/reth/blob/cc576bc8690a3e16e6e5bf1cbbbfdd029e85e3d4/crates/rpc/rpc/src/eth/api/transactions.rs#L849
pub(crate) fn build_rpc_receipt(
    block: &SealedBlock,
    tx: TransactionSignedAndRecovered,
    tx_number: u64,
    receipt: Receipt,
) -> AnyTransactionReceipt {
    let transaction: TransactionSignedEcRecovered = tx.into();
    let transaction_kind = transaction.kind();

    let transaction_hash = transaction.hash;
    let transaction_index = tx_number - block.transactions.start;
    let block_hash = block.header.hash();
    let block_number = block.header.number;
    let block_timestamp = block.header.timestamp;
    let block_base_fee = block.header.base_fee_per_gas;
    let other = OtherFields::new(
        [
            (
                "l1FeeRate".into(),
                format!("{:#x}", block.l1_fee_rate).into(),
            ),
            (
                "l1DiffSize".into(),
                format!("{:#x}", receipt.l1_diff_size).into(),
            ),
        ]
        .into_iter()
        .collect(),
    );

    let mut logs = Vec::with_capacity(receipt.receipt.logs.len());
    for (tx_log_idx, log) in receipt.receipt.logs.iter().enumerate() {
        let rpclog = Log {
            inner: log.clone(),
            block_hash: Some(block_hash),
            block_number: Some(block_number),
            block_timestamp: Some(block_timestamp),
            transaction_hash: Some(transaction_hash),
            transaction_index: Some(transaction_index),
            log_index: Some(receipt.log_index_start + tx_log_idx as u64),
            removed: false,
        };
        logs.push(rpclog);
    }

    let rpc_receipt = reth_rpc_types::Receipt {
        status: Eip658Value::Eip658(receipt.receipt.success),
        cumulative_gas_used: receipt.receipt.cumulative_gas_used as u128,
        logs,
    };

    let res_receipt = TransactionReceipt {
        inner: AnyReceiptEnvelope {
            inner: ReceiptWithBloom {
                receipt: rpc_receipt,
                logs_bloom: receipt.receipt.bloom_slow(),
            },
            r#type: transaction.transaction.tx_type().into(),
        },
        transaction_hash,
        transaction_index: Some(transaction_index),
        block_hash: Some(block_hash),
        block_number: Some(block_number),
        from: transaction.signer(),
        to: match transaction_kind {
            Create => None,
            Call(addr) => Some(addr),
        },
        gas_used: receipt.gas_used,
        contract_address: match transaction_kind {
            Create => Some(transaction.signer().create(transaction.nonce())),
            Call(_) => None,
        },
        effective_gas_price: transaction.effective_gas_price(block_base_fee),
        // TODO pre-byzantium receipts have a post-transaction state root
        state_root: None,
        // EIP-4844 related
        // https://github.com/Sovereign-Labs/sovereign-sdk/issues/912
        blob_gas_price: None,
        blob_gas_used: None,
        authorization_list: None,
    };
    AnyTransactionReceipt {
        inner: res_receipt,
        other,
    }
}

// range is not inclusive, if we have the block but the transaction
// index is out of range, return None
fn check_tx_range(transactions_range: &Range<u64>, index: Uint<64, 1>) -> Option<()> {
    let range_len = transactions_range.end - transactions_range.start;
    if index.to::<u64>() >= range_len {
        None
    } else {
        Some(())
    }
}

fn map_out_of_gas_err<C: sov_modules_api::Context>(
    block_env: BlockEnv,
    mut tx_env: revm::primitives::TxEnv,
    cfg_env: revm::primitives::CfgEnvWithHandlerCfg,
    db: EvmDb<'_, C>,
    l1_fee_rate: u128,
) -> EthApiError {
    let req_gas_limit = tx_env.gas_limit;
    tx_env.gas_limit = block_env.gas_limit;

    match inspect_no_tracing(db, cfg_env, block_env, tx_env, l1_fee_rate) {
        Ok((res, _tx_info)) => match res.result {
            ExecutionResult::Success { .. } => {
                // transaction succeeded by manually increasing the gas limit to
                // highest, which means the caller lacks funds to pay for the tx
                RpcInvalidTransactionError::BasicOutOfGas(req_gas_limit).into()
            }
            ExecutionResult::Revert { output, .. } => {
                // reverted again after bumping the limit
                RpcInvalidTransactionError::Revert(RevertError::new(output)).into()
            }
            ExecutionResult::Halt { reason, .. } => {
                RpcInvalidTransactionError::EvmHalt(reason).into()
            }
        },
        Err(err) => EthApiError::from(err),
    }
}

/// Updates the highest and lowest gas limits for binary search
/// based on the result of the execution
#[inline]
fn update_estimated_gas_range(
    result: ExecutionResult,
    tx_gas_limit: u64,
    highest_gas_limit: &mut u64,
    lowest_gas_limit: &mut u64,
    l1_fee: &mut U256,
    diff_size: &mut u64,
    tx_info: TxInfo,
) -> EthResult<()> {
    match result {
        ExecutionResult::Success { .. } => {
            // cap the highest gas limit with succeeding gas limit
            *highest_gas_limit = tx_gas_limit;
            *l1_fee = tx_info.l1_fee;
            *diff_size = tx_info.l1_diff_size;
        }
        ExecutionResult::Revert { .. } => {
            // increase the lowest gas limit
            *lowest_gas_limit = tx_gas_limit;

            *l1_fee = tx_info.l1_fee;
            *diff_size = tx_info.l1_diff_size;
        }
        ExecutionResult::Halt { reason, .. } => {
            match reason {
                HaltReason::OutOfGas(_) | HaltReason::InvalidFEOpcode => {
                    // either out of gas or invalid opcode can be thrown dynamically if
                    // gasLeft is too low, so we treat this as `out of gas`, we know this
                    // call succeeds with a higher gaslimit. common usage of invalid opcode in openzeppelin <https://github.com/OpenZeppelin/openzeppelin-contracts/blob/94697be8a3f0dfcd95dfb13ffbd39b5973f5c65d/contracts/metatx/ERC2771Forwarder.sol#L360-L367>

                    // increase the lowest gas limit
                    *lowest_gas_limit = tx_gas_limit;

                    // TODO: for halt l1 fee is calculated as 0, but it should be calculated
                }
                err => {
                    // these should be unreachable because we know the transaction succeeds,
                    // but we consider these cases an error
                    return Err(RpcInvalidTransactionError::EvmHalt(err).into());
                }
            }
        }
    };
    Ok(())
}

#[inline]
fn set_state_to_end_of_evm_block<C: sov_modules_api::Context>(
    block_number: u64,
    working_set: &mut WorkingSet<C>,
) {
    // genesis is committed at db version 1
    // so every block is offset by 1
    working_set.set_archival_version(block_number + 1);
}
