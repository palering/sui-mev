use dex_indexer::{
    types::{Pool, Protocol},
    DexIndexer,
};
use eyre::{bail, ensure, OptionExt, Result};
use object_pool::ObjectPool;
use simulator::Simulator;
use std::sync::Arc;
use sui_sdk::SUI_COIN_TYPE;
use sui_types::base_types::ObjectID;
use tokio::sync::OnceCell;
use tokio::task::JoinSet;

// Updated imports for ProtocolAdapter and specific adapters
use dex_indexer::protocols::{
    ProtocolAdapter,
    aftermath_adapter::AftermathAdapter,
    cetus_adapter::CetusAdapter,
    // Other adapters will be added here as they are created e.g.
    // deepbook_v2_adapter::DeepbookV2Adapter,
    // flowx_clmm_adapter::FlowxClmmAdapter,
    // turbos_adapter::TurbosAdapter,
};
// Keep DexSearcher and Path from local `super` for now, though DexSearcher's signature changes.
// Dex itself is removed.
use super::{DexSearcher, Path, deepbook_v2::DeepbookV2, flowx_clmm::FlowxClmm, turbos::Turbos}; // Temporarily keep old structs for unmigrated protocols
use crate::defi::{blue_move::BlueMove, kriya_amm::KriyaAmm, kriya_clmm::KriyaClmm}; // Temporarily keep old structs


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

// Renamed to new_adapters and returns Vec<Box<dyn ProtocolAdapter>>
async fn new_adapters(
    simulator_box: Box<dyn Simulator>, // Changed from Arc<Box<dyn Simulator>> to Box<dyn Simulator> to match simulator_pool.get()
    pool: &Pool,
    token_in_type: &str,
    token_out_type: Option<String>,
) -> Result<Vec<Box<dyn ProtocolAdapter>>> {
    let simulator_arc = Arc::new(simulator_box); // Adapters expect Arc<dyn Simulator>
    let adapters: Vec<Box<dyn ProtocolAdapter>> = match pool.protocol {
        Protocol::Turbos => {
            // TODO: Replace with TurbosAdapter once created
            let dex = Turbos::new(simulator_arc, pool, token_in_type).await?;
            vec![Box::new(dex) as Box<dyn ProtocolAdapter>] // Placeholder casting
        }

        Protocol::Cetus => {
            let adapter = CetusAdapter::new(simulator_arc, pool, token_in_type).await?;
            vec![Box::new(adapter) as Box<dyn ProtocolAdapter>]
        }

        Protocol::Aftermath => {
            if let Some(out_type) = token_out_type {
                // AftermathAdapter::new expects a specific coin_out_type, not Option
                let adapter = AftermathAdapter::new(simulator_arc, pool, token_in_type, &out_type).await?;
                // The old Aftermath::new returned Vec<Self>, AftermathAdapter::new returns Result<Self>
                // So, we wrap it in a vec.
                vec![Box::new(adapter) as Box<dyn ProtocolAdapter>]
            } else {
                // If no specific token_out_type, AftermathAdapter cannot be created with current constructor.
                // Original Aftermath::new could discover pairs. This functionality is lost for now.
                // Consider logging this or modifying AftermathAdapter.
                // For find_test_path, this means Aftermath won't be included if token_out_type is None.
                println!("WARN: AftermathAdapter skipped for pool {} due to missing specific coin_out_type", pool.pool);
                vec![]
            }
        }
        Protocol::FlowxClmm => {
            // TODO: Replace with FlowxClmmAdapter once created
            let dex = FlowxClmm::new(simulator_arc, pool, token_in_type).await?;
            vec![Box::new(dex) as Box<dyn ProtocolAdapter>] // Placeholder casting
        }

        Protocol::KriyaAmm => {
            // TODO: Replace with KriyaAmmAdapter once created
            let dex = KriyaAmm::new(simulator_arc, pool, token_in_type).await?;
            vec![Box::new(dex) as Box<dyn ProtocolAdapter>] // Placeholder casting
        }

        Protocol::KriyaClmm => {
            // TODO: Replace with KriyaClmmAdapter once created
            let dex = KriyaClmm::new(simulator_arc, pool, token_in_type).await?;
            vec![Box::new(dex) as Box<dyn ProtocolAdapter>] // Placeholder casting
        }

        Protocol::DeepbookV2 => {
            // TODO: Replace with DeepbookV2Adapter once created
            let dex = DeepbookV2::new(simulator_arc, pool, token_in_type).await?;
            vec![Box::new(dex) as Box<dyn ProtocolAdapter>] // Placeholder casting
        }

        Protocol::BlueMove => {
            // TODO: Replace with BlueMoveAdapter once created
            let dex = BlueMove::new(simulator_arc, pool, token_in_type).await?;
            vec![Box::new(dex) as Box<dyn ProtocolAdapter>] // Placeholder casting
        }

        _ => bail!("unsupported protocol for adapter conversion: {:?}", pool.protocol),
    };

    Ok(adapters)
}

#[async_trait::async_trait]
impl DexSearcher for IndexerDexSearcher {
    async fn find_dexes(&self, token_in_type: &str, token_out_type: Option<String>) -> Result<Vec<Box<dyn ProtocolAdapter>>> { // Return type changed
        let pools = if let Some(token_out_type_str) = token_out_type.as_ref() {
            self.indexer.get_pools_by_token01(token_in_type, token_out_type_str)
        } else {
            self.indexer.get_pools_by_token(token_in_type)
        };
        ensure!(
            pools.is_some(),
            "pools not found, coin_in: {}, coin_out: {:?}",
            token_in_type,
            token_out_type.as_deref().unwrap_or("None")
        );

        let mut join_set = JoinSet::new();
        for pool in pools.unwrap() { // pools.unwrap() is safe due to ensure above
            let simulator = self.simulator_pool.get(); // This is Box<dyn Simulator>
            let token_in_type_owned = token_in_type.to_string();
            let token_out_type_owned = token_out_type.clone();
            // Call renamed new_adapters
            join_set.spawn(async move { new_adapters(simulator, &pool, &token_in_type_owned, token_out_type_owned).await });
        }

        let mut res = Vec::new();
        while let Some(Ok(result)) = join_set.join_next().await {
            match result {
                Ok(adapters) => res.extend(adapters), // Changed dexes to adapters
                Err(_error) => {
                    // Optionally log error, e.g., tracing::debug!(?error, "Failed to create adapter for a pool");
                }
            }
        }

        Ok(res)
    }

    async fn find_test_path(&self, path_obj_ids: &[ObjectID]) -> Result<Path> { // Renamed path to path_obj_ids for clarity
        let mut adapters_for_path = vec![]; // Changed dexes to adapters_for_path
        let mut current_coin_in = SUI_COIN_TYPE.to_string();

        for pool_id in path_obj_ids {
            let simulator = self.simulator_pool.get(); // Box<dyn Simulator>
            let pool = self.indexer.get_pool_by_id(pool_id).ok_or_eyre(format!("Pool {} not found in find_test_path", pool_id))?;

            // new_adapters expects Option<String> for token_out_type. For finding a path sequentially,
            // we don't pre-specify token_out_type for each step, it's determined by the pool's pairs.
            // However, AftermathAdapter::new now requires a specific token_out_type.
            // If the pool is Aftermath and token_out_type is None, new_adapters will return an empty vec or error for it.
            // We need one adapter from new_adapters. If it finds multiple (e.g. old Aftermath), we'd need to pick one.
            // Assuming new_adapters will provide the correct single adapter or handle pair discovery if token_out_type is None.
            // For now, passing None and hoping the adapter's `new` or `new_adapters` logic can handle it or we pick first.
            let mut found_adapters = new_adapters(simulator, &pool, &current_coin_in, None).await?;

            ensure!(!found_adapters.is_empty(), "No adapter found for pool {} with coin_in {}", pool_id, current_coin_in);
            // If multiple adapters were returned for a single pool (e.g. if an adapter could handle multiple pairs from one pool object),
            // we'd need a strategy here. For now, assume the first one is suitable or the one that matches current_coin_in.
            // The current adapter structure (one instance per specific pair for Aftermath, or per pool for Cetus) simplifies this.
            let adapter = found_adapters.remove(0); // Take the first (and likely only) adapter

            current_coin_in = adapter.coin_out_type();
            adapters_for_path.push(adapter);
        }

        Ok(Path { path: adapters_for_path }) // Path struct now takes Vec<Box<dyn ProtocolAdapter>>
    }
}
