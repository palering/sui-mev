use dex_indexer::{
    types::{Pool, Protocol},
    DexIndexer,
};
use eyre::{ensure, eyre, OptionExt, Result}; // Added eyre for error construction
use object_pool::ObjectPool;
use simulator::Simulator;
use std::sync::Arc;
use sui_sdk::SUI_COIN_TYPE;
use sui_types::base_types::ObjectID;
use tokio::sync::OnceCell;
// JoinSet might be removed if find_dexes is simplified
// use tokio::task::JoinSet;

// Core ProtocolAdapter trait
use dex_indexer::protocols::ProtocolAdapter;

// Specific, refactored adapters
use dex_indexer::protocols::cetus_adapter::CetusAdapter;
use dex_indexer::protocols::aftermath_adapter::AftermathAdapter;
use dex_indexer::protocols::blue_move::BlueMoveAdapter;
use dex_indexer::protocols::deepbook_v2::DeepbookV2Adapter;
use dex_indexer::protocols::flowx_clmm::FlowxCLMMAdapter;
use dex_indexer::protocols::kriya_amm::KriyaAmmAdapter;
use dex_indexer::protocols::navi::NaviAdapter;
use dex_indexer::protocols::turbos::TurbosAdapter;
use crate::defi::kriya_clmm::KriyaClmm; // This should point to the refactored KriyaClmm adapter

// DexSearcher trait and Path struct from the same `defi` module (super)
use super::{DexSearcher, Path};
// Note: Old, unrefactored adapter structs like AftermathAdapter, DeepbookV2, FlowxClmm, Turbos, BlueMove, KriyaAmm
// are removed from imports here. If find_dexes needs to handle them, it will be by skipping them.
// The AftermathAdapter was imported from dex_indexer::protocols, if it's refactored and ready, it can be used.
// For this task, focusing on Cetus and KriyaClmm.

static INDEXER: OnceCell<Arc<DexIndexer>> = OnceCell::const_new();

#[derive(Clone)]
pub struct IndexerDexSearcher {
    simulator_pool: Arc<ObjectPool<Box<dyn Simulator>>>,
    indexer: Arc<DexIndexer>,
}

impl IndexerDexSearcher {
    pub async fn new(http_url: &str, simulator_pool: Arc<ObjectPool<Box<dyn Simulator>>>) -> Result<Self> {
        let indexer = INDEXER
            .get_or_init(|| async {
                let indexer = DexIndexer::new(http_url).await.unwrap();
                Arc::new(indexer)
            })
            .await
            .clone();

        Ok(Self {
            simulator_pool,
            indexer,
        })
    }
}

// The new_adapters function is removed. Logic will be in find_dexes and find_test_path.

#[async_trait::async_trait]
impl DexSearcher for IndexerDexSearcher {
    async fn find_dexes(&self, coin_in_type: &str, coin_out_type: Option<String>) -> Result<Vec<Box<dyn ProtocolAdapter>>> {
        let pools_data = if let Some(token_out_type_str) = coin_out_type.as_ref() {
            self.indexer.get_pools_by_token01(coin_in_type, token_out_type_str)
        } else {
            self.indexer.get_pools_by_token(coin_in_type)
        };

        let pools_data = pools_data.ok_or_else(|| eyre!(
            "pools not found, coin_in: {}, coin_out: {:?}",
            token_in_type,
            coin_out_type.as_deref().unwrap_or("None")
        ))?;

        let mut adapters: Vec<Box<dyn ProtocolAdapter>> = Vec::new();

        for pool_data in pools_data {
            // simulator_pool.get() returns a Box<dyn Simulator>. Adapters need Arc<dyn Simulator>.
            // Arc::new() will move the Box into an Arc.
            let simulator = Arc::new(self.simulator_pool.get());
            let coin_in_type_for_pool = coin_in_type; // Already a &str

            let adapter_result = match pool_data.protocol {
                Protocol::Cetus => {
                    CetusAdapter::new(simulator, &pool_data, coin_in_type_for_pool)
                        .await
                        .map(|adapter| Box::new(adapter) as Box<dyn ProtocolAdapter>)
                        .map_err(|e| eyre!("CetusAdapter error for pool {}: {}", pool_data.pool, e))
                }
                Protocol::KriyaClmm => {
                    KriyaClmm::new(simulator, &pool_data, coin_in_type_for_pool)
                        .await
                        .map(|adapter| Box::new(adapter) as Box<dyn ProtocolAdapter>)
                        .map_err(|e| eyre!("KriyaClmm adapter error for pool {}: {}", pool_data.pool, e))
                }
                Protocol::Aftermath => {
                    // AftermathAdapter requires a specific coin_out_type.
                    // We use the coin_out_type parameter of find_dexes.
                    // If it's None, we cannot create this adapter.
                    if let Some(ref out_type_str) = coin_out_type {
                        AftermathAdapter::new(simulator, &pool_data, coin_in_type_for_pool, out_type_str)
                            .await
                            .map(|adapter| Box::new(adapter) as Box<dyn ProtocolAdapter>)
                            .map_err(|e| eyre!("AftermathAdapter error for pool {}: {}", pool_data.pool, e))
                    } else {
                        // If no specific coin_out_type is requested for the search, skip Aftermath.
                        // Alternatively, one could try to infer or iterate all possible pairs from the pool,
                        // but current AftermathAdapter isn't designed for that.
                        // tracing::debug!("Skipping Aftermath pool {} as no specific coin_out_type was provided for search", pool_data.pool);
                        continue; // Special handling: skip if coin_out_type is None
                    }
                }
                Protocol::BlueMove => {
                    BlueMoveAdapter::new(simulator, &pool_data, coin_in_type_for_pool)
                        .await
                        .map(|adapter| Box::new(adapter) as Box<dyn ProtocolAdapter>)
                        .map_err(|e| eyre!("BlueMoveAdapter error for pool {}: {}", pool_data.pool, e))
                }
                Protocol::DeepBookV2 => { // Enum variant is DeepBookV2
                    DeepbookV2Adapter::new(simulator, &pool_data, coin_in_type_for_pool)
                        .await
                        .map(|adapter| Box::new(adapter) as Box<dyn ProtocolAdapter>)
                        .map_err(|e| eyre!("DeepbookV2Adapter error for pool {}: {}", pool_data.pool, e))
                }
                Protocol::FlowxClmm => { // Enum variant is FlowxClmm
                    FlowxCLMMAdapter::new(simulator, &pool_data, coin_in_type_for_pool)
                        .await
                        .map(|adapter| Box::new(adapter) as Box<dyn ProtocolAdapter>)
                        .map_err(|e| eyre!("FlowxCLMMAdapter error for pool {}: {}", pool_data.pool, e))
                }
                Protocol::KriyaAmm => {
                    KriyaAmmAdapter::new(simulator, &pool_data, coin_in_type_for_pool)
                        .await
                        .map(|adapter| Box::new(adapter) as Box<dyn ProtocolAdapter>)
                        .map_err(|e| eyre!("KriyaAmmAdapter error for pool {}: {}", pool_data.pool, e))
                }
                Protocol::Navi => {
                    NaviAdapter::new(simulator, &pool_data, coin_in_type_for_pool)
                        .await
                        .map(|adapter| Box::new(adapter) as Box<dyn ProtocolAdapter>)
                        .map_err(|e| eyre!("NaviAdapter error for pool {}: {}", pool_data.pool, e))
                }
                Protocol::TurbosFinance => { // Enum variant is TurbosFinance
                    TurbosAdapter::new(simulator, &pool_data, coin_in_type_for_pool)
                        .await
                        .map(|adapter| Box::new(adapter) as Box<dyn ProtocolAdapter>)
                        .map_err(|e| eyre!("TurbosAdapter error for pool {}: {}", pool_data.pool, e))
                }
                unsupported_protocol => {
                    // tracing::warn!("Skipping unsupported protocol {:?} for pool {}", unsupported_protocol, pool_data.pool);
                    continue; // Skip this pool if protocol is not supported
                }
            };

            match adapter_result {
                Ok(adapter_instance) => {
                    // Additional check: if coin_out_type is specified, ensure adapter matches
                    if let Some(expected_out_type) = &coin_out_type {
                        if adapter_instance.coin_out_type() == *expected_out_type {
                            adapters.push(adapter_instance);
                        } else {
                            // This can happen if a pool supports multiple pairs with the same coin_in_type
                            // but the instantiated adapter resolved to a different coin_out_type.
                            // Or if coin_in_type was coin1 of the pool, and coin_out_type became coin0.
                            // We might need to call flip() here if the adapter's initial state
                            // does not match the desired coin_in_type -> coin_out_type direction.
                            // For now, simply filter.
                            // tracing::debug!("Adapter for pool {} resolved to {} -> {} but expected {} -> {}",
                            //    pool_data.pool, adapter_instance.coin_in_type(), adapter_instance.coin_out_type(),
                            //    coin_in_type, expected_out_type);
                        }
                    } else {
                        // If no coin_out_type specified, add any valid adapter for coin_in_type
                        adapters.push(adapter_instance);
                    }
                }
                Err(e) => {
                    // tracing::debug!("Failed to create adapter for pool {}: {}", pool_data.pool, e);
                    // Optionally log error, but continue processing other pools.
                }
            }
        }
        Ok(adapters)
    }

    async fn find_test_path(&self, path_object_ids: &[ObjectID]) -> Result<Path> {
        let mut adapters_for_path: Vec<Box<dyn ProtocolAdapter>> = Vec::new();

        for (index, object_id) in path_object_ids.iter().enumerate() {
            let pool_info = self
                .indexer
                .get_pool_by_id(object_id) // Assuming get_pool_by_id returns Option<Pool>
                .ok_or_else(|| eyre!("Pool {} not found in find_test_path", object_id))?;

            let simulator = Arc::new(self.simulator_pool.get());

            let current_coin_in_type_str: String;
            let current_coin_in_type: &str;

            if adapters_for_path.is_empty() {
                // For the first dex in path, we need to determine its input coin.
                // The problem is, a pool (ObjectID) can be TokenA/TokenB.
                // If path is A->B->C, first pool is A/B. We need to know if A is token0 or token1.
                // This requires either the caller to specify the initial coin_in_type for the path,
                // or we make an assumption. The previous logic used SUI_COIN_TYPE.
                // The new prompt implies using pool_info.token0_type() as a default for the very first step.
                // This might not be correct if the path doesn't start with token0_type.
                // A robust find_test_path might need an initial coin_in_type parameter.
                // For now, following the prompt's simplified logic:
                current_coin_in_type_str = pool_info.token0_type().to_string();
                current_coin_in_type = &current_coin_in_type_str;
                // A better default might be to try instantiating with token0, and if that doesn't align (e.g. if adapter flips), adjust.
                // Or, the test path provider should ensure the sequence of ObjectIDs implies a valid coin flow.
            } else {
                current_coin_in_type_str = adapters_for_path.last().unwrap().coin_out_type();
                current_coin_in_type = &current_coin_in_type_str;
            }

            let adapter_result = match pool_info.protocol {
                Protocol::Cetus => {
                    CetusAdapter::new(simulator, &pool_info, current_coin_in_type)
                        .await
                        .map(|a| Box::new(a) as Box<dyn ProtocolAdapter>)
                        .map_err(|e| eyre!("CetusAdapter failed for test path pool {}: {}", pool_info.pool, e))
                }
                Protocol::KriyaClmm => {
                    KriyaClmm::new(simulator, &pool_info, current_coin_in_type)
                        .await
                        .map(|a| Box::new(a) as Box<dyn ProtocolAdapter>)
                        .map_err(|e| eyre!("KriyaClmm adapter failed for test path pool {}: {}", pool_info.pool, e))
                }
                Protocol::Aftermath => {
                    // Determine coin_out_type for Aftermath based on current_coin_in_type and pool's tokens
                    let coin_out_for_aftermath = if pool_info.token0_type() == current_coin_in_type {
                        pool_info.token1_type()
                    } else if pool_info.token1_type() == current_coin_in_type {
                        pool_info.token0_type()
                    } else {
                        return Err(eyre!(
                            "Logic error: current_coin_in_type {} not found in Aftermath pool {} ({}, {}) for test path",
                            current_coin_in_type,
                            pool_info.pool,
                            pool_info.token0_type(),
                            pool_info.token1_type()
                        ));
                    };
                    AftermathAdapter::new(simulator, &pool_info, current_coin_in_type, coin_out_for_aftermath)
                        .await
                        .map(|a| Box::new(a) as Box<dyn ProtocolAdapter>)
                        .map_err(|e| eyre!("AftermathAdapter failed for test path pool {}: {}", pool_info.pool, e))
                }
                Protocol::BlueMove => {
                    BlueMoveAdapter::new(simulator, &pool_info, current_coin_in_type)
                        .await
                        .map(|a| Box::new(a) as Box<dyn ProtocolAdapter>)
                        .map_err(|e| eyre!("BlueMoveAdapter failed for test path pool {}: {}", pool_info.pool, e))
                }
                Protocol::DeepBookV2 => {
                    DeepbookV2Adapter::new(simulator, &pool_info, current_coin_in_type)
                        .await
                        .map(|a| Box::new(a) as Box<dyn ProtocolAdapter>)
                        .map_err(|e| eyre!("DeepbookV2Adapter failed for test path pool {}: {}", pool_info.pool, e))
                }
                Protocol::FlowxClmm => {
                    FlowxCLMMAdapter::new(simulator, &pool_info, current_coin_in_type)
                        .await
                        .map(|a| Box::new(a) as Box<dyn ProtocolAdapter>)
                        .map_err(|e| eyre!("FlowxCLMMAdapter failed for test path pool {}: {}", pool_info.pool, e))
                }
                Protocol::KriyaAmm => {
                    KriyaAmmAdapter::new(simulator, &pool_info, current_coin_in_type)
                        .await
                        .map(|a| Box::new(a) as Box<dyn ProtocolAdapter>)
                        .map_err(|e| eyre!("KriyaAmmAdapter failed for test path pool {}: {}", pool_info.pool, e))
                }
                Protocol::Navi => {
                    NaviAdapter::new(simulator, &pool_info, current_coin_in_type)
                        .await
                        .map(|a| Box::new(a) as Box<dyn ProtocolAdapter>)
                        .map_err(|e| eyre!("NaviAdapter failed for test path pool {}: {}", pool_info.pool, e))
                }
                Protocol::TurbosFinance => {
                    TurbosAdapter::new(simulator, &pool_info, current_coin_in_type)
                        .await
                        .map(|a| Box::new(a) as Box<dyn ProtocolAdapter>)
                        .map_err(|e| eyre!("TurbosAdapter failed for test path pool {}: {}", pool_info.pool, e))
                }
                unsupported_protocol => {
                    return Err(eyre!(
                        "Unsupported protocol {:?} for object_id {} in test path",
                        unsupported_protocol,
                        object_id
                    ));
                }
            };

            match adapter_result {
                Ok(mut adapter_instance) => {
                    // Ensure the adapter's coin_in_type matches current_coin_in_type.
                    // Some adapters might initialize to a default pair orientation.
                    if adapter_instance.coin_in_type() != current_coin_in_type {
                        // This implies the adapter might have flipped, or was initialized with the other pair.
                        // e.g. KriyaClmm::new might try to set coin_in_type. If it doesn't match, it might flip itself,
                        // or we might need to flip it.
                        // For now, assume new() correctly sets the direction if coin_in_type is provided.
                        // If not, this is a point of potential error.
                        // A check could be:
                        if adapter_instance.coin_out_type() == current_coin_in_type { // It's flipped
                            adapter_instance.flip();
                        }
                        // After potential flip, re-check
                        if adapter_instance.coin_in_type() != current_coin_in_type {
                            return Err(eyre!("Adapter for pool {} initialized with {} as input, but expected {}",
                                pool_info.pool, adapter_instance.coin_in_type(), current_coin_in_type));
                        }
                    }
                    adapters_for_path.push(adapter_instance);
                }
                Err(e) => return Err(e), // Fail the whole function if any adapter in the test path fails
            }
        }
        Ok(Path::new(adapters_for_path))
    }
}
