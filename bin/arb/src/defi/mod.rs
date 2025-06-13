// mod aftermath; // Removed
mod blue_move;
// mod cetus; // Removed
mod deepbook_v2;
mod flowx_clmm;
mod indexer_searcher;
mod kriya_amm;
mod kriya_clmm;
mod navi;
mod shio;
mod trade;
mod turbos;
mod utils;

use std::{
    collections::{HashMap, HashSet},
    fmt,
    hash::Hash,
    sync::Arc,
};

use ::utils::coin;
use dex_indexer::types::Protocol;
use eyre::{bail, ensure, Result};
pub use indexer_searcher::IndexerDexSearcher;
use object_pool::ObjectPool;
use simulator::{SimulateCtx, Simulator};
use sui_sdk::SUI_COIN_TYPE;
use sui_types::{
    base_types::{ObjectID, ObjectRef, SuiAddress},
    transaction::{Argument, TransactionData},
};
use tokio::task::JoinSet;
use tracing::Instrument;
// FlashResult and TradeCtx are now directly imported if needed, not via trade module for these specific types
use dex_indexer::protocols::{ProtocolAdapter, CloneBoxedProtocolAdapter, TradeCtx, FlashResult}; // Added imports

// Path, TradeType, Trader, TradeResult are still from local trade module
pub use trade::{Path, TradeType, Trader, TradeResult};

use crate::{config::pegged_coin_types, types::Source};

const MAX_HOP_COUNT: usize = 2;
const MAX_POOL_COUNT: usize = 10;
const MIN_LIQUIDITY: u128 = 1000;

pub const CETUS_AGGREGATOR: &str = "0x11451575c775a3e633437b827ecbc1eb51a5964b0302210b28f5b89880be21a2";

#[async_trait::async_trait]
pub trait DexSearcher: Send + Sync {
    // coin_type: e.g. "0x2::sui::SUI"
    async fn find_dexes(&self, coin_in_type: &str, coin_out_type: Option<String>) -> Result<Vec<Box<dyn ProtocolAdapter>>>; // Changed Dex to ProtocolAdapter

    async fn find_test_path(&self, path: &[ObjectID]) -> Result<Path>;
}

// Dex trait removed
// CloneBoxedDex trait removed
// Implementations for Clone, PartialEq, Eq, Hash, Debug for Box<dyn Dex> removed
// Instead, these would be for Box<dyn ProtocolAdapter> if needed,
// but they are typically defined in the crate where ProtocolAdapter is defined if they are generic.
// For now, assuming these specific impls (PartialEq, Eq, Hash, Debug for the Boxed trait object) might not be immediately needed
// or would be defined elsewhere if required for collections.
// The ProtocolAdapter itself in dex_indexer::protocols might need to ensure its objects are comparable/hashable if stored in HashSets/HashMaps.
// Let's remove these impls for now as they were specific to Box<dyn Dex>.
// If Path requires Box<dyn ProtocolAdapter> to be Eq or Hash, we might need to add those for ProtocolAdapter.
// The Path struct in trade.rs does not seem to require Hash or Eq for Box<dyn ProtocolAdapter> for its current methods.
// The `all_hops` HashMap in `find_sell_paths` uses String as key and Vec<Box<dyn ProtocolAdapter>> as value, so no direct Hash/Eq on Box<dyn ProtocolAdapter> needed there.
// The `visited_dexes` HashSet in `find_sell_paths` stores ObjectID, not Box<dyn ProtocolAdapter>.

// For fmt::Debug on Box<dyn ProtocolAdapter>, it would look like:
impl fmt::Debug for Box<dyn ProtocolAdapter> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Assuming ProtocolAdapter has these methods (it does, from Dex)
        write!(
            f,
            "ProtocolAdapter({:?}, {}, {}, {})", // Added a marker for clarity
            self.protocol(),
            self.object_id(),
            self.coin_in_type(),
            self.coin_out_type()
        )
    }
}

#[derive(Clone)]
pub struct Defi {
    dex_searcher: Arc<dyn DexSearcher>,
    trader: Arc<Trader>,
}

impl Defi {
    pub async fn new(http_url: &str, simulator_pool: Arc<ObjectPool<Box<dyn Simulator>>>) -> Result<Self> {
        let dex_searcher = IndexerDexSearcher::new(http_url, simulator_pool.clone()).await?;
        let trade = Trader::new(simulator_pool).await?;

        Ok(Self {
            dex_searcher: Arc::new(dex_searcher),
            trader: Arc::new(trade),
        })
    }

    #[allow(dead_code)]
    pub async fn find_dexes(&self, coin_in_type: &str, coin_out_type: Option<String>) -> Result<Vec<Box<dyn ProtocolAdapter>>> { // Changed Dex to ProtocolAdapter
        self.dex_searcher.find_dexes(coin_in_type, coin_out_type).await
    }

    pub async fn find_sell_paths(&self, coin_in_type: &str) -> Result<Vec<Path>> {
        if coin::is_native_coin(coin_in_type) {
            return Ok(vec![Path::default()]);
        }

        let mut all_hops = HashMap::new();
        let mut stack = vec![coin_in_type.to_string()];
        let mut visited = HashSet::new();
        let mut visited_dexes = HashSet::new();

        for nth_hop in 0..MAX_HOP_COUNT {
            let is_last_hop = nth_hop == MAX_HOP_COUNT - 1;
            let mut new_stack = vec![];

            while let Some(coin_type) = stack.pop() {
                if visited.contains(&coin_type) || coin::is_native_coin(&coin_type) {
                    continue;
                }
                visited.insert(coin_type.clone());

                let coin_out_type = if pegged_coin_types().contains(coin_type.as_str()) || is_last_hop {
                    Some(SUI_COIN_TYPE.to_string())
                } else {
                    None
                };
                let mut dexes = if let Ok(dexes) = self.dex_searcher.find_dexes(&coin_type, coin_out_type).await {
                    dexes
                } else {
                    continue;
                };

                dexes.retain(|dex| dex.liquidity() >= MIN_LIQUIDITY);

                if dexes.len() > MAX_POOL_COUNT {
                    dexes.retain(|dex| !visited_dexes.contains(&dex.object_id()));
                    dexes.sort_by_key(|dex| std::cmp::Reverse(dex.liquidity()));
                    dexes.truncate(MAX_POOL_COUNT);
                }

                if dexes.is_empty() {
                    continue;
                }

                for dex in &dexes {
                    let out_coin_type = dex.coin_out_type();
                    if !visited.contains(&out_coin_type) {
                        new_stack.push(out_coin_type.clone());
                    }
                    visited_dexes.insert(dex.object_id());
                }
                all_hops.insert(coin_type.clone(), dexes);
            }

            if is_last_hop {
                break;
            }

            stack = new_stack;
        }

        let mut routes = vec![];
        dfs(coin_in_type, &mut vec![], &all_hops, &mut routes);

        Ok(routes.into_iter().map(Path::new).collect())
    }

    pub async fn find_buy_paths(&self, coin_out_type: &str) -> Result<Vec<Path>> {
        let mut paths = self.find_sell_paths(coin_out_type).await?;
        for path in &mut paths {
            path.path.reverse();
            for dex in &mut path.path {
                dex.flip();
            }
        }

        Ok(paths)
    }

    pub async fn find_best_path_exact_in(
        &self,
        paths: &[Path],
        sender: SuiAddress,
        amount_in: u64,
        trade_type: TradeType,
        gas_coins: &[ObjectRef],
        sim_ctx: &SimulateCtx,
    ) -> Result<PathTradeResult> {
        let mut joinset = JoinSet::new();

        for (idx, path) in paths.iter().enumerate() {
            if path.is_empty() {
                continue;
            }

            let trade = self.trader.clone();
            let path = path.clone();
            let gas_coins = gas_coins.to_vec();
            let sim_ctx = sim_ctx.clone();

            joinset.spawn(
                async move {
                    let result = trade
                        .get_trade_result(&path, sender, amount_in, trade_type, gas_coins, sim_ctx)
                        .await;

                    (idx, result)
                }
                .in_current_span(),
            );
        }

        let (mut best_idx, mut best_trade_res) = (0, TradeResult::default());
        while let Some(Ok((idx, trade_res))) = joinset.join_next().await {
            match trade_res {
                Ok(trade_res) => {
                    if trade_res > best_trade_res {
                        best_idx = idx;
                        best_trade_res = trade_res;
                    }
                }
                Err(_error) => {
                    // tracing::error!(path = ?paths[idx], ?error, "trade
                    // error");
                }
            }
        }

        ensure!(best_trade_res.amount_out > 0, "zero amount_out");

        Ok(PathTradeResult::new(paths[best_idx].clone(), amount_in, best_trade_res))
    }

    pub async fn build_final_tx_data(
        &self,
        sender: SuiAddress,
        amount_in: u64,
        path: &Path,
        gas_coins: Vec<ObjectRef>,
        gas_price: u64,
        source: Source,
    ) -> Result<TransactionData> {
        let (tx_data, _) = self
            .trader
            .get_flashloan_trade_tx(path, sender, amount_in, gas_coins, gas_price, source)
            .await?;

        Ok(tx_data)
    }
}

fn dfs(
    coin_type: &str,
    path: &mut Vec<Box<dyn ProtocolAdapter>>, // Changed Dex to ProtocolAdapter
    hops: &HashMap<String, Vec<Box<dyn ProtocolAdapter>>>, // Changed Dex to ProtocolAdapter
    routes: &mut Vec<Vec<Box<dyn ProtocolAdapter>>>, // Changed Dex to ProtocolAdapter
) {
    if coin::is_native_coin(coin_type) {
        routes.push(path.clone());
        return;
    }
    if path.len() >= MAX_HOP_COUNT {
        return;
    }
    if !hops.contains_key(coin_type) {
        return;
    }
    for dex in hops.get(coin_type).unwrap() {
        path.push(dex.clone());
        dfs(&dex.coin_out_type(), path, hops, routes);
        path.pop();
    }
}

#[derive(Debug, Clone)]
pub struct PathTradeResult {
    pub path: Path,
    pub amount_in: u64,
    pub amount_out: u64,
    pub gas_cost: i64,
    pub cache_misses: u64,
}

impl PathTradeResult {
    pub fn new(path: Path, amount_in: u64, trade_res: TradeResult) -> Self {
        Self {
            path,
            amount_in,
            amount_out: trade_res.amount_out,
            gas_cost: trade_res.gas_cost,
            cache_misses: trade_res.cache_misses,
        }
    }

    pub fn profit(&self) -> i128 {
        if self.path.coin_in_type() == SUI_COIN_TYPE {
            if self.path.coin_out_type() == SUI_COIN_TYPE {
                return self.amount_out as i128 - self.amount_in as i128 - self.gas_cost as i128;
            }
            0 - self.gas_cost as i128 - self.amount_in as i128
        } else {
            0
        }
    }
}

impl fmt::Display for PathTradeResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "PathTradeResult {{ amount_in: {}, amount_out: {}, profit: {}, path: {:?} ... }}",
            self.amount_in,
            self.amount_out,
            self.profit(),
            self.path
        )
    }
}

#[cfg(test)]
mod tests {

    use simulator::HttpSimulator;
    use tracing::info;

    use super::*;
    use crate::config::tests::TEST_HTTP_URL;

    #[tokio::test]
    async fn test_find_sell_paths() {
        mev_logger::init_console_logger_with_directives(None, &["arb=debug", "dex_indexer=debug"]);

        let simulator_pool = ObjectPool::new(1, move || {
            tokio::runtime::Runtime::new()
                .unwrap()
                .block_on(async { Box::new(HttpSimulator::new(&TEST_HTTP_URL, &None).await) as Box<dyn Simulator> })
        });

        let defi = Defi::new(TEST_HTTP_URL, Arc::new(simulator_pool)).await.unwrap();

        let coin_in_type = "0xa8816d3a6e3136e86bc2873b1f94a15cadc8af2703c075f2d546c2ae367f4df9::ocean::OCEAN";
        let paths = defi.find_sell_paths(coin_in_type).await.unwrap();
        assert!(!paths.is_empty(), "No sell paths found");

        for path in paths {
            info!(?path, "sell")
        }
    }

    #[tokio::test]
    async fn test_find_buy_paths() {
        mev_logger::init_console_logger_with_directives(None, &["arb=debug", "dex_indexer=debug"]);

        let simulator_pool = ObjectPool::new(1, move || {
            tokio::runtime::Runtime::new()
                .unwrap()
                .block_on(async { Box::new(HttpSimulator::new(&TEST_HTTP_URL, &None).await) as Box<dyn Simulator> })
        });

        let defi = Defi::new(TEST_HTTP_URL, Arc::new(simulator_pool)).await.unwrap();

        let coin_out_type = "0xa8816d3a6e3136e86bc2873b1f94a15cadc8af2703c075f2d546c2ae367f4df9::ocean::OCEAN";
        let paths = defi.find_buy_paths(coin_out_type).await.unwrap();
        assert!(!paths.is_empty(), "No buy paths found");
        for path in paths {
            info!(?path, "buy")
        }
    }
}
