use std::{
    collections::HashSet,
    fmt,
    ops::{Deref, DerefMut},
    str::FromStr,
    sync::Arc,
};

use ::utils::coin;
use eyre::{ensure, eyre, Result};
use object_pool::ObjectPool;
use simulator::{SimulateCtx, Simulator};
use sui_json_rpc_types::SuiExecutionStatus;
use sui_sdk::rpc_types::SuiTransactionBlockEffectsAPI;
use sui_types::{
    base_types::{ObjectID, ObjectRef, SuiAddress},
    object::{Object, Owner},
    programmable_transaction_builder::ProgrammableTransactionBuilder,
    transaction::{Argument, Command, ObjectArg, TransactionData},
    Identifier, TypeTag, SUI_FRAMEWORK_PACKAGE_ID,
};
use tracing::instrument;

// Import moved types from dex_indexer
use dex_indexer::protocols::{FlashResult, TradeCtx, ProtocolAdapter, CloneBoxedProtocolAdapter}; // Added ProtocolAdapter, CloneBoxedProtocolAdapter

use super::{navi::Navi, shio::Shio}; // Dex removed, ProtocolAdapter will be used.
use crate::{config::*, types::Source};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TradeType {
    Swap,
    Flashloan,
}

// FlashResult is now imported

#[derive(Clone)]
pub struct Trader {
    simulator_pool: Arc<ObjectPool<Box<dyn Simulator>>>,
    shio: Arc<Shio>,
    navi: Arc<Navi>,
}

// TradeCtx is now imported

#[derive(Default, Debug, Clone)]
pub struct TradeResult {
    pub amount_out: u64,
    pub gas_cost: i64,
    pub cache_misses: u64,
}

impl Trader {
    pub async fn new(simulator_pool: Arc<ObjectPool<Box<dyn Simulator>>>) -> Result<Self> {
        let shio = Arc::new(Shio::new().await?);
        let simulator = simulator_pool.get();
        let navi = Arc::new(Navi::new(simulator).await?);

        Ok(Self {
            simulator_pool,
            shio,
            navi,
        })
    }

    #[instrument(name = "result", skip_all, fields(
        len = %format!("{:<2}", path.path.len()),
        paths = %path.path.iter().map(|d| { // This d will be Box<dyn ProtocolAdapter> soon
            let coin_in = d.coin_in_type().split("::").last().unwrap().to_string();
            let coin_out = d.coin_out_type().split("::").last().unwrap().to_string();
            format!("{:?}:{}:{}", d.protocol(), coin_in, coin_out) // protocol() is on ProtocolAdapter
        }).collect::<Vec<_>>().join(" ")
    ))]
    pub async fn get_trade_result(
        &self,
        path: &Path,
        sender: SuiAddress,
        amount_in: u64,
        trade_type: TradeType,
        gas_coins: Vec<ObjectRef>,
        mut sim_ctx: SimulateCtx,
    ) -> Result<TradeResult> {
        ensure!(!path.is_empty(), "empty path");
        let gas_price = sim_ctx.epoch.gas_price;

        let (tx_data, mocked_coin_in) = match trade_type {
            TradeType::Swap => {
                self.get_swap_trade_tx(path, sender, amount_in, gas_coins, gas_price)
                    .await?
            }
            TradeType::Flashloan => {
                self.get_flashloan_trade_tx(path, sender, amount_in, gas_coins, gas_price, Source::Public)
                    .await?
            }
        };

        if let Some(mocked_coin_in) = mocked_coin_in {
            sim_ctx.with_borrowed_coin((mocked_coin_in, amount_in));
        }

        let resp = self.simulator_pool.get().simulate(tx_data.clone(), sim_ctx).await?;
        let status = resp.effects.status();

        match status {
            SuiExecutionStatus::Success => {}
            SuiExecutionStatus::Failure { error } => {
                // ignore "MoveAbort"
                if !error.contains("MoveAbort") && !error.contains("InsufficientCoinBalance") {
                    tracing::error!("status: {:?}", status);
                }
            }
        }

        ensure!(status.is_ok(), "{:?}", status);

        let gas_cost = resp.effects.gas_cost_summary().net_gas_usage();
        let coin_in = TypeTag::from_str(&path.coin_in_type()).map_err(|_| eyre!("invalid coin_in_type"))?;
        let coin_out = TypeTag::from_str(&path.coin_out_type()).map_err(|_| eyre!("invalid coin_out_type"))?;
        let out_is_native = coin::is_native_coin(&path.coin_out_type());

        let mut amount_out = i128::MIN;
        for bc in &resp.balance_changes {
            if bc.owner == Owner::AddressOwner(sender) && bc.coin_type == coin_out {
                amount_out = bc.amount;
                if coin_in == coin_out && out_is_native {
                    amount_out = amount_out + amount_in as i128 + gas_cost as i128;
                }

                ensure!(amount_out >= 0, "negative amount_out {}", amount_out);
                break;
            }
        }
        ensure!(amount_out != i128::MIN, "no balance change for owner: {:?}", sender);

        Ok(TradeResult {
            amount_out: amount_out as u64,
            gas_cost,
            cache_misses: resp.cache_misses,
        })
    }

    pub async fn get_swap_trade_tx(
        &self,
        path: &Path,
        sender: SuiAddress,
        amount_in: u64,
        gas_coins: Vec<ObjectRef>,
        gas_price: u64,
    ) -> Result<(TransactionData, Option<Object>)> {
        ensure!(!path.is_empty(), "empty path");
        let mut ctx = TradeCtx::default();

        // 1. prepare coin_in
        let mocked_sui = coin::mocked_sui(sender, amount_in);
        let coin_in = mocked_sui.compute_object_reference();

        // 2. swap
        let mut coin_in_arg = ctx.split_coin(coin_in, amount_in)?;
        for (i, dex) in path.path.iter().enumerate() {
            let amount_in = if i == 0 { Some(amount_in) } else { None };
            coin_in_arg = dex.extend_trade_tx(&mut ctx, sender, coin_in_arg, amount_in).await?;
        }

        // 3. transfer the coin_out to recipient
        ctx.transfer_arg(sender, coin_in_arg);
        let tx = ctx.ptb.finish();

        let tx_data = TransactionData::new_programmable(sender, gas_coins, tx, GAS_BUDGET, gas_price);

        Ok((tx_data, Some(mocked_sui)))
    }

    pub async fn get_flashloan_trade_tx(
        &self,
        path: &Path,
        sender: SuiAddress,
        amount_in: u64,
        gas_coins: Vec<ObjectRef>,
        gas_price: u64,
        source: Source,
    ) -> Result<(TransactionData, Option<Object>)> {
        ensure!(!path.is_empty(), "empty path");
        let first_dex = &path.path[0];

        let mut ctx = TradeCtx::default();

        // 1. flashloan
        let flash_res = if first_dex.support_flashloan() {
            first_dex.extend_flashloan_tx(&mut ctx, amount_in).await?
        } else {
            self.navi.extend_flashloan_tx(&mut ctx, amount_in)?
        };

        // 2. swap
        let mut coin_in_arg = flash_res.coin_out;
        let dex_iter: Box<dyn Iterator<Item = &Box<dyn ProtocolAdapter>> + Send> = if first_dex.support_flashloan() { // ProtocolAdapter has support_flashloan
            Box::new(path.path.iter().skip(1))
        } else {
            Box::new(path.path.iter())
        };
        for (i, dex) in dex_iter.enumerate() {
            let amount_in = if i == 0 { Some(amount_in) } else { None };
            coin_in_arg = dex.extend_trade_tx(&mut ctx, sender, coin_in_arg, amount_in).await?; // ProtocolAdapter has extend_trade_tx
        }

        // 3. repay flashloan
        let coin_profit = if first_dex.support_flashloan() { // ProtocolAdapter has support_flashloan
            first_dex.extend_repay_tx(&mut ctx, coin_in_arg, flash_res).await? // ProtocolAdapter has extend_repay_tx
        } else {
            // Assuming self.navi.extend_repay_tx is compatible or Navi becomes an adapter.
            // For now, this part remains, but Navi might need to conform to ProtocolAdapter if used in paths.
            // If Navi is not part of the path, this is fine.
            self.navi.extend_repay_tx(&mut ctx, coin_in_arg, flash_res)?
        };

        // 4. submit bid
        if source.is_shio() {
            let amount_arg = ctx.pure(source.bid_amount()).map_err(|e| eyre!(e))?;
            let coin_bid = ctx.split_coin_arg(coin_profit, amount_arg);
            self.shio.submit_bid(&mut ctx, coin_bid, source.bid_amount())?;
        }

        // 5. transfer the profit to recipient
        ctx.transfer_arg(sender, coin_profit);

        let tx = ctx.ptb.finish();

        // 6. finalize
        let mut tx_data =
            TransactionData::new_programmable(sender, gas_coins.clone(), tx.clone(), GAS_BUDGET, gas_price);

        if let Some(opp_tx_digest) = source.opp_tx_digest() {
            // A Bid MUST have a lexicologically larger transaction digest comparing to opportunity transaction's.
            let mut gas_budget = GAS_BUDGET;
            while tx_data.digest() <= opp_tx_digest {
                gas_budget += 1;
                tx_data =
                    TransactionData::new_programmable(sender, gas_coins.clone(), tx.clone(), gas_budget, gas_price);
            }
        };

        Ok((tx_data, None))
    }
}

// TradeCtx struct and impl block removed, now imported.
// Deref and DerefMut for TradeCtx were part of its impl block, so they are also effectively removed from here.


impl PartialEq for TradeResult {
    fn eq(&self, other: &Self) -> bool {
        self.amount_out == other.amount_out
    }
}

impl PartialOrd for TradeResult {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        self.amount_out.partial_cmp(&other.amount_out)
    }
}

#[derive(Default, Clone)]
pub struct Path {
    pub path: Vec<Box<dyn ProtocolAdapter>>,
}

impl Path {
    pub fn new(path: Vec<Box<dyn ProtocolAdapter>>) -> Self {
        Self { path }
    }

    pub fn is_empty(&self) -> bool {
        self.path.is_empty()
    }

    pub fn is_disjoint(&self, other: &Self) -> bool {
        let a = self.path.iter().collect::<HashSet<_>>();
        let b = other.path.iter().collect::<HashSet<_>>();
        a.is_disjoint(&b)
    }

    pub fn coin_in_type(&self) -> String {
        self.path[0].coin_in_type()
    }

    pub fn coin_out_type(&self) -> String {
        self.path.last().unwrap().coin_out_type()
    }

    pub fn contains_pool(&self, pool_id: Option<ObjectID>) -> bool {
        if let Some(pool_id) = pool_id {
            self.path.iter().any(|dex| dex.object_id() == pool_id)
        } else {
            false
        }
    }
}

impl fmt::Debug for Path {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let path_str: Vec<String> = self.path.iter().map(|dex| format!("{:?}", dex)).collect();
        write!(f, "[{}]", path_str.join(", "))
    }
}
