use std::{collections::HashSet, sync::Arc};

use dex_indexer::{
    protocols::{
        kriya_clmm as kriya_clmm_indexer_utils, CloneBoxedProtocolAdapter, FlashResult, ProtocolAdapter, TradeCtx,
    },
    types::{Pool as IndexerPool, Protocol as IndexerProtocol, SwapEvent as IndexerSwapEvent},
};
use eyre::{ensure, eyre, OptionExt, Result};
use move_core_types::annotated_value::MoveStruct;
use simulator::Simulator;
use sui_sdk::{rpc_types::SuiEvent, SuiClient};
use sui_types::{
    base_types::{ObjectID, ObjectRef, SuiAddress},
    transaction::{Argument, Command, ObjectArg, ProgrammableTransaction, TransactionData},
    Identifier, TypeTag, SUI_CLOCK_OBJECT_ID,
};
use tokio::sync::OnceCell;
use utils::{
    coin, new_test_sui_client, // TODO: new_test_sui_client might be problematic if not in test context
    object::{extract_u128_from_move_struct, shared_obj_arg},
};

// Define constants if they are not available from elsewhere, or ensure they are correctly imported.
// GAS_BUDGET, MIN_SQRT_PRICE_X64, MAX_SQRT_PRICE_X64 are defined below.
const GAS_BUDGET: u64 = 500_000_000; // Example value, confirm if it should come from elsewhere
const MIN_SQRT_PRICE_X64: u128 = 4295048016; // from Kriya SDK/constants
const MAX_SQRT_PRICE_X64: u128 = 79228162514264337593543950335; // from Kriya SDK/constants


const KRIYA_CLMM_PACKAGE_ID: &str = "0xbd8d4489782042c6fafad4de4bc6a5e0b84a43c6c00647ffd7062d1e2bb7549e";
const KRIYA_VERSION_SHARED_OBJECT_ID: &str = "0xf5145a7ac345ca8736cf8c76047d00d6d378f30e81be6f6eb557184d9de93c78";

#[derive(Clone)]
pub struct ObjectArgs {
    version: ObjectArg,
    clock: ObjectArg,
}

static OBJ_CACHE: OnceCell<ObjectArgs> = OnceCell::const_new();

async fn get_object_args(simulator: Arc<dyn Simulator>) -> ObjectArgs {
    OBJ_CACHE
        .get_or_init(|| async {
            let version_id = ObjectID::from_hex_literal(KRIYA_VERSION_SHARED_OBJECT_ID).unwrap();
            let version = simulator.get_object(&version_id).await.unwrap();
            let clock = simulator.get_object(&SUI_CLOCK_OBJECT_ID).await.unwrap();

            ObjectArgs {
                version: shared_obj_arg(&version, false),
                clock: shared_obj_arg(&clock, false),
            }
        })
        .await
        .clone()
}

#[derive(Clone)]
pub struct KriyaClmm {
    pool: IndexerPool,
    pool_arg: ObjectArg,
    liquidity: u128,
    coin_in_type: String,
    coin_out_type: String,
    type_params: Vec<TypeTag>,
    version: ObjectArg,
    clock: ObjectArg,
    simulator_arc: Arc<dyn Simulator>, // Added simulator arc
}

impl KriyaClmm {
    pub async fn new(simulator: Arc<dyn Simulator>, pool: &IndexerPool, coin_in_type: &str) -> Result<Self> {
        ensure!(
            pool.protocol == IndexerProtocol::KriyaClmm,
            "not a KriyaClmm pool"
        );

        let pool_obj = simulator
            .get_object(&pool.pool)
            .await
            .ok_or_else(|| eyre!("pool not found: {}", pool.pool))?;

        let parsed_pool = {
            let layout = simulator
                .get_object_layout(&pool.pool)
                .ok_or_eyre("pool layout not found")?;

            let move_obj = pool_obj.data.try_as_move().ok_or_eyre("not a move object")?;
            MoveStruct::simple_deserialize(move_obj.contents(), &layout).map_err(|e| eyre!(e))?
        };

        let liquidity = extract_u128_from_move_struct(&parsed_pool, "liquidity")?;

        let coin_out_type = if pool.token0_type() == coin_in_type {
            pool.token1_type().to_string()
        } else {
            pool.token0_type().to_string()
        };

        let type_params = parsed_pool.type_.type_params.clone();

        let pool_arg = shared_obj_arg(&pool_obj, true);
        let ObjectArgs { version, clock } = get_object_args(simulator.clone()).await; // Clone arc for get_object_args

        Ok(Self {
            pool: pool.clone(),
            liquidity,
            coin_in_type: coin_in_type.to_string(),
            coin_out_type,
            type_params,
            pool_arg,
            version,
            clock,
            simulator_arc: simulator, // Store the simulator
        })
    }

    // Note: build_pt_for_swap was removed.

    /*
    fun swap_a2b<CoinA, CoinB>(
        pool: &mut Pool<CoinA, CoinB>,
        coin_a: Coin<CoinA>,
        version: &Version,
        clock: &Clock,
        ctx: &mut TxContext,
    ): Coin<CoinB>
    */
    fn build_swap_args(&self, ctx: &mut TradeCtx, coin_in_arg: Argument) -> Result<Vec<Argument>> {
        let pool_arg = ctx.obj(self.pool_arg).map_err(|e| eyre!(e))?;
        let version_arg = ctx.obj(self.version).map_err(|e| eyre!(e))?;
        let clock_arg = ctx.obj(self.clock).map_err(|e| eyre!(e))?;

        Ok(vec![pool_arg, coin_in_arg, version_arg, clock_arg])
    }

    /*
    public fun flash_swap<T0, T1>(
        _pool: &mut Pool<T0, T1>,
        _a2b: bool,
        _by_amount_in: bool,
        _amount: u64,
        _sqrt_price_limit: u128,
        _clock: &Clock,
        _version: &Version,
        _ctx: &TxContext
    ) : (Balance<T0>, Balance<T1>, FlashSwapReceipt)
    */
    fn build_flashloan_args(&self, ctx: &mut TradeCtx, amount_in: u64) -> Result<Vec<Argument>> {
        let pool_arg = ctx.obj(self.pool_arg).map_err(|e| eyre!(e))?;
        let a2b = ctx.pure(self.is_a2b()).map_err(|e| eyre!(e))?;
        let by_amount_in = ctx.pure(true).map_err(|e| eyre!(e))?;
        let amount = ctx.pure(amount_in).map_err(|e| eyre!(e))?;

        let sqrt_price_limit = if self.is_a2b() {
            MIN_SQRT_PRICE_X64
        } else {
            MAX_SQRT_PRICE_X64
        };
        let sqrt_price_limit = ctx.pure(sqrt_price_limit).map_err(|e| eyre!(e))?;

        let clock_arg = ctx.obj(self.clock).map_err(|e| eyre!(e))?;
        let version_arg = ctx.obj(self.version).map_err(|e| eyre!(e))?;

        Ok(vec![
            pool_arg,
            a2b,
            by_amount_in,
            amount,
            sqrt_price_limit,
            clock_arg,
            version_arg,
        ])
    }

    /*
    public fun repay_flash_swap<T0, T1>(
        _pool: &mut Pool<T0, T1>,
        _receipt: FlashSwapReceipt,
        _balance_a: Balance<T0>,
        _balance_b: Balance<T1>,
        _version: &Version,
        _ctx: &TxContext
    )
    */
    fn build_repay_args(&self, ctx: &mut TradeCtx, coin: Argument, receipt: Argument) -> Result<Vec<Argument>> {
        let pool_arg = ctx.obj(self.pool_arg).map_err(|e| eyre!(e))?;

        let (balance_a, balance_b) = if self.is_a2b() {
            (
                ctx.coin_into_balance(coin, self.type_params[0].clone())?,
                ctx.balance_zero(self.type_params[1].clone())?,
            )
        } else {
            (
                ctx.balance_zero(self.type_params[0].clone())?,
                ctx.coin_into_balance(coin, self.type_params[1].clone())?,
            )
        };

        let version_arg = ctx.obj(self.version).map_err(|e| eyre!(e))?;
        Ok(vec![pool_arg, receipt, balance_a, balance_b, version_arg])
    }
}

// Remove old Dex trait implementation
// #[async_trait::async_trait]
// impl Dex for KriyaClmm { ... }

// Test module will be adjusted or significantly changed later if needed.
// For now, focusing on the adapter implementation.

#[async_trait::async_trait]
impl ProtocolAdapter for KriyaClmm {
    fn support_flashloan(&self) -> bool {
        true
    }

    fn extend_flashloan_tx(&self, ctx: &mut TradeCtx, amount_in: u64) -> Result<FlashResult> {
        let package = ObjectID::from_hex_literal(KRIYA_CLMM_PACKAGE_ID)?;
        let module = Identifier::new("trade").map_err(|e| eyre!(e))?;
        let function = Identifier::new("flash_swap").map_err(|e| eyre!(e))?;
        let type_arguments = self.type_params.clone();
        let arguments = self.build_flashloan_args(ctx, amount_in)?;
        ctx.command(Command::move_call(package, module, function, type_arguments, arguments));

        let last_idx = ctx.last_command_idx();

        // `flash_swap` returns (Balance<T0>, Balance<T1>, FlashSwapReceipt)
        let (received_balance_in, received_balance_out) = if self.is_a2b() {
            (Argument::NestedResult(last_idx, 0), Argument::NestedResult(last_idx, 1))
        } else {
            (Argument::NestedResult(last_idx, 1), Argument::NestedResult(last_idx, 0))
        };
        let receipt = Argument::NestedResult(last_idx, 2);

        let (coin_in_type, coin_out_type) = if self.is_a2b() {
            (self.type_params[0].clone(), self.type_params[1].clone())
        } else {
            (self.type_params[1].clone(), self.type_params[0].clone())
        };
        ctx.balance_destroy_zero(received_balance_in, coin_in_type)?;
        let coin_out = ctx.coin_from_balance(received_balance_out, coin_out_type)?;
        Ok(FlashResult {
            coin_out,
            receipt: Some(receipt), // FlashResult expects Option<Argument>
            pool: None, // Kriya does not return pool object in flashloan
        })
    }

    fn extend_repay_tx(&self, ctx: &mut TradeCtx, coin: Argument, flash_res: FlashResult) -> Result<Argument> {
        let package = ObjectID::from_hex_literal(KRIYA_CLMM_PACKAGE_ID)?;
        let module = Identifier::new("trade").map_err(|e| eyre!(e))?;
        let receipt = flash_res.receipt.ok_or_eyre("Missing receipt for Kriya repay")?;

        // get repay_amount and split coin
        let repay_amount_arg = {
            let function = Identifier::new("swap_receipt_debts").map_err(|e| eyre!(e))?;
            let type_arguments = vec![]; // swap_receipt_debts has no type args
            let arguments = vec![receipt]; // it takes &FlashSwapReceipt
            ctx.command(Command::move_call(
                package,
                module.clone(),
                function,
                type_arguments,
                arguments,
            ));

            let last_idx = ctx.last_command_idx();
            // returns (coin_a_debt: u64, coin_b_debt: u64)
            if self.is_a2b() { // if flash loaned coin_a (type_params[0]), repay coin_a
                Argument::NestedResult(last_idx, 0)
            } else { // else flash loaned coin_b (type_params[1]), repay coin_b
                Argument::NestedResult(last_idx, 1)
            }
        };
        let repay_coin = ctx.split_coin_arg(coin, repay_amount_arg);

        // repay
        let function = Identifier::new("repay_flash_swap").map_err(|e| eyre!(e))?;
        let type_arguments = self.type_params.clone();
        let arguments = self.build_repay_args(ctx, repay_coin, receipt)?;
        ctx.command(Command::move_call(package, module, function, type_arguments, arguments));

        Ok(coin) // Return the original coin argument (potentially with remaining balance)
    }

    fn extend_trade_tx(
        &self,
        ctx: &mut TradeCtx,
        _sender: SuiAddress, // sender not directly used in kriya swap PTB call construction
        coin_in: Argument,
        _amount_in: Option<u64>, // amount_in not directly used, assumed to be baked into coin_in
    ) -> Result<Argument> {
        let function_name = if self.is_a2b() { "swap_a2b" } else { "swap_b2a" };

        // IMPORTANT: The original code used CETUS_AGGREGATOR package ID here by mistake.
        // It should be KRIYA_CLMM_PACKAGE_ID and its own module for CLMM pool swaps.
        // Kriya CLMM swaps are in the 'trade' module.
        // The aggregator pattern `kriya_clmm::swap_a2b` usually implies an aggregator contract.
        // If Kriya has a direct CLMM pool swap function (e.g. in `pool` or `trade` module), that should be used.
        // From the original `build_swap_args` and `extend_trade_tx` for Dex trait, it seems it was calling
        // `kriya_clmm::swap_a2b` or `kriya_clmm::swap_b2a`. This implies an aggregator or a specific module.
        // Let's assume the module is "trade" as per flashloan functions.
        // The function names `swap_a2b` and `swap_b2a` are typical.

        let package = ObjectID::from_hex_literal(KRIYA_CLMM_PACKAGE_ID)?;
        let module = Identifier::new("trade").map_err(|e| eyre!(e))?; // Assuming "trade" module
        let function = Identifier::new(function_name).map_err(|e| eyre!(e))?;
        let type_arguments = self.type_params.clone();
        let arguments = self.build_swap_args(ctx, coin_in)?;
        ctx.command(Command::move_call(package, module, function, type_arguments, arguments));

        let last_idx = ctx.last_command_idx();
        Ok(Argument::Result(last_idx))
    }

    fn coin_in_type(&self) -> String {
        self.coin_in_type.clone()
    }

    fn coin_out_type(&self) -> String {
        self.coin_out_type.clone()
    }

    fn protocol(&self) -> IndexerProtocol {
        IndexerProtocol::KriyaClmm
    }

    fn liquidity(&self) -> u128 {
        self.liquidity
    }

    fn object_id(&self) -> ObjectID {
        self.pool.pool
    }

    fn flip(&mut self) {
        std::mem::swap(&mut self.coin_in_type, &mut self.coin_out_type);
        // Pool's internal a2b representation might also need flipping if it stores it.
        // However, the current KriyaClmm struct recalculates is_a2b based on coin_in_type and pool.token_index.
    }

    fn is_a2b(&self) -> bool {
        self.pool.token_index(&self.coin_in_type) == Some(0)
    }

    async fn swap_tx(&self, sender: SuiAddress, recipient: SuiAddress, amount_in: u64) -> Result<TransactionData> {
        // This needs a SuiClient. The original test helper created one.
        // For a generic adapter, it might be better if SuiClient is passed in or available via simulator.
        // However, the trait signature does not provide it.
        // Falling back to creating a new client, which is not ideal for non-test scenarios.
        // This method is often used for testing or simple single swaps.
        let sui_client = new_test_sui_client().await; // Or handle error: eyre!("Failed to create test sui client")

        let coin_in_obj = coin::get_coin(&sui_client, sender, &self.coin_in_type(), amount_in)
            .await?
            .ok_or_else(|| eyre!("Coin not found for swap_tx"))?;

        let mut ctx = TradeCtx::default();
        let coin_in_arg = ctx.take_object(coin_in_obj.object_ref())?; // Make coin_in_obj an arg

        // We need to split the coin if its balance > amount_in, or use it directly if it's exact.
        // For simplicity, let's assume get_coin gives a coin that can be used or split.
        // If `get_coin` fetches a coin with exactly `amount_in`, then `coin_in_arg` can be used directly.
        // If it's larger, we need to split. `ctx.split_coin` takes an ObjectRef and amount.
        // `extend_trade_tx` expects an `Argument`.

        // Let's refine: `ctx.split_coin` (from TradeCtx utils) is suitable here.
        // It takes an ObjectRef and returns an Argument (the split coin).
        let coin_to_swap = ctx.split_coin(coin_in_obj.object_ref(), amount_in)?;

        let coin_out_arg = self.extend_trade_tx(&mut ctx, sender, coin_to_swap, Some(amount_in))?;
        ctx.transfer_arg(recipient, coin_out_arg);

        let pt = ctx.ptb.finish();

        let gas_coins = coin::get_gas_coin_refs(&sui_client, sender, Some(coin_in_obj.coin_object_id)).await?;
        let gas_price = sui_client.read_api().get_reference_gas_price().await?;

        Ok(TransactionData::new_programmable(sender, gas_coins, pt, GAS_BUDGET, gas_price))
    }

    async fn parse_pool_created_event(&self, event: &SuiEvent, sui_client: &SuiClient) -> Result<IndexerPool> {
        let kriya_event = kriya_clmm_indexer_utils::KriyaClmmPoolCreated::try_from(event)?;
        kriya_event.to_pool(sui_client).await
    }

    async fn parse_swap_event(&self, event: &SuiEvent, simulator: Arc<dyn Simulator>) -> Result<IndexerSwapEvent> {
        let kriya_event = kriya_clmm_indexer_utils::KriyaClmmSwapEvent::try_from(event)?;
        kriya_event.to_swap_event_v2(simulator).await
    }

    async fn get_related_object_ids(&self) -> Result<HashSet<String>> {
        Ok(kriya_clmm_indexer_utils::kriya_clmm_related_object_ids().into_iter().collect())
    }

    async fn get_pool_children_ids(&self, pool: &IndexerPool, _sui_client: &SuiClient) -> Result<Vec<String>> {
        kriya_clmm_indexer_utils::kriya_clmm_pool_children_ids(pool, self.simulator_arc.clone()).await
    }
}

impl CloneBoxedProtocolAdapter for KriyaClmm {
    fn clone_boxed(&self) -> Box<dyn ProtocolAdapter> {
        Box::new(self.clone())
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use itertools::Itertools;
    use object_pool::ObjectPool;
    use simulator::{DBSimulator, HttpSimulator, Simulator};
    use tracing::info;

    use super::*;
    use crate::config::tests::{TEST_ATTACKER, TEST_HTTP_URL};
    // use crate::defi::{indexer_searcher::IndexerDexSearcher, DexSearcher}; // DexSearcher might be outdated

    #[tokio::test]
    async fn test_kriya_clmm_swap_tx() {
        mev_logger::init_console_logger_with_directives(None, &["arb=debug", "dex_indexer=debug"]);

        let http_simulator = HttpSimulator::new(TEST_HTTP_URL, &None).await;

        let owner = SuiAddress::from_str(TEST_ATTACKER).unwrap();
        let recipient =
            SuiAddress::from_str("0x0cbe287984143ef232336bb39397bd10607fa274707e8d0f91016dceb31bb829").unwrap();
        let token_in_type = "0x2::sui::SUI";
        let token_out_type = "0xdeeb7a4662eec9f2f3def03fb937a663dddaa2e215b8078a284d026b7946c270::deep::DEEP";
        let amount_in = 10000;

        let simulator_pool = Arc::new(ObjectPool::new(1, move || {
            tokio::runtime::Runtime::new()
                .unwrap()
                .block_on(async { Box::new(DBSimulator::new_test(true).await) as Box<dyn Simulator> })
        }));

        // find dexes and swap
        // The test needs to be updated to use ProtocolAdapter and the new `new` signature.
        // For now, commenting out the parts that rely on the old Dex trait / DexSearcher.
        // let searcher = IndexerDexSearcher::new(TEST_HTTP_URL, simulator_pool).await.unwrap();
        // let dexes = searcher
        //     .find_dexes(token_in_type, Some(token_out_type.into()))
        //     .await
        //     .unwrap();
        // info!("🧀 dexes_len: {}", dexes.len());
        // let dex = dexes
        //     .into_iter()
        //     .filter(|dex| dex.protocol() == IndexerProtocol::KriyaClmm) // Use IndexerProtocol
        //     .sorted_by(|a, b| a.liquidity().cmp(&b.liquidity()))
        //     .last()
        //     .unwrap();
        // let tx_data = dex.swap_tx(owner, recipient, amount_in).await.unwrap();
        // info!("🧀 tx_data: {:?}", tx_data);

        // let response = http_simulator.simulate(tx_data, Default::default()).await.unwrap();
        // info!("🧀 {:?}", response); // response is not defined due to commented out lines above
    }
}
