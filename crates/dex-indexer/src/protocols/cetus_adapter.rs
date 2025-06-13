use std::sync::{Arc, OnceLock as StdOnceLock};
use std::str::FromStr;
use std::collections::HashSet;

use async_trait::async_trait;
use eyre::{ensure, eyre, OptionExt, Result};
use move_core_types::{
    annotated_value::{MoveStruct, MoveStructLayout},
    identifier::Identifier,
    language_storage::TypeTag,
};
use rayon::prelude::*;
use serde::Deserialize;
use serde_json::Value;
use shio::ShioEvent;
use simulator::{SimulateCtx, Simulator, TradeResult as SimulateTradeResult};
use sui_sdk::{
    rpc_types::{SuiData, SuiEvent, SuiObjectDataOptions, SuiTransactionBlockEvents, SuiMoveStruct, SuiMoveValue}, // Added SuiMoveStruct, SuiMoveValue
    SuiClient,
};
use sui_types::{
    base_types::{ObjectID, ObjectRef, SuiAddress, SequenceNumber},
    dynamic_field::derive_dynamic_field_id,
    gas_coin::GasCoin,
    move_object::MoveObject as SuiMoveObject,
    object::Object as SuiObject,
    object::Owner,
    parse_sui_struct_tag,
    programmable_transaction_builder::ProgrammableTransactionBuilder,
    transaction::{Argument, Command, ObjectArg, ProgrammableTransaction, TransactionData},
    Identifier as SuiIdentifier,
    SUI_CLOCK_OBJECT_ID,
};
use tokio::sync::OnceCell as TokioOnceCell;

// Local crate imports
use crate::{
    normalize_coin_type,
    protocols::{
        utils::{get_coin_decimals, get_pool_coins_type, get_coin_in_out_v2},
        FlashResult, ProtocolAdapter, TradeCtx, CloneBoxedProtocolAdapter,
    },
    types::{Pool, PoolExtra, Protocol, SwapEvent, Token},
};

use utils::{coin, object::*};


const CETUS_DEX_PACKAGE_ID: &str = "0xeffc8ae61f439bb34c9b905ff8f29ec56873dcedf81c7123ff2f1f67c45ec302";
const CETUS_CONFIG_ID: &str = "0xdaa46292632c3c4d8f31f23ea0f9b36a28ff3677e9684980e4438403a67a3d8f";
const CETUS_PARTNER_ID: &str = "0x639b5e433da31739e800cd085f356e64cae222966d0f1b11bd9dc76b322ff58b";

const CETUS_EVENT_PACKAGE_ID: &str = "0x1eabed72c53feb3805120a081dc15963c204dc8d091542592abaf7a35689b2fb";
const CETUS_POOL_CREATED_EVENT_TYPE: &str =
    "0x1eabed72c53feb3805120a081dc15963c204dc8d091542592abaf7a35689b2fb::factory::CreatePoolEvent";
const CETUS_SWAP_EVENT_TYPE: &str =
    "0x1eabed72c53feb3805120a081dc15963c204dc8d091542592abaf7a35689b2fb::pool::SwapEvent";
const CETUS_FETCHER_SCRIPT_PACKAGE_ID: &str = "0x3a5aa90ffa33d09100d7b6941ea1c0ffe6ab66e77062ddd26320c1b073aabb10";
const TICK_BOUND: i64 = 443636;

static CETUS_POOL_LAYOUT_CACHE: StdOnceLock<MoveStructLayout> = StdOnceLock::new();

#[derive(Clone, Debug)]
pub struct CetusObjectArgs {
    config: ObjectArg,
    partner: ObjectArg,
    clock: ObjectArg,
}

static CETUS_OBJ_ARGS_CACHE: TokioOnceCell<CetusObjectArgs> = TokioOnceCell::const_new();

async fn get_cetus_object_args(simulator: Arc<dyn Simulator>) -> Result<CetusObjectArgs> {
    CETUS_OBJ_ARGS_CACHE
        .get_or_try_init(|| async {
            let config_id = ObjectID::from_hex_literal(CETUS_CONFIG_ID)?;
            let partner_id = ObjectID::from_hex_literal(CETUS_PARTNER_ID)?;

            let config_obj = simulator.get_object(&config_id).await?
                .ok_or_else(|| eyre!("Cetus CONFIG object {} not found", CETUS_CONFIG_ID))?;
            let partner_obj = simulator.get_object(&partner_id).await?
                .ok_or_else(|| eyre!("Cetus PARTNER object {} not found", CETUS_PARTNER_ID))?;
            let clock_obj = simulator.get_object(&SUI_CLOCK_OBJECT_ID).await?
                .ok_or_else(|| eyre!("SUI_CLOCK_OBJECT_ID {} not found", SUI_CLOCK_OBJECT_ID))?;

            Ok(CetusObjectArgs {
                config: shared_obj_arg(&config_obj, false),
                partner: shared_obj_arg(&partner_obj, true),
                clock: shared_obj_arg(&clock_obj, false),
            })
        })
        .await
        .cloned()
}

#[derive(Clone, Debug)]
pub struct CetusAdapter {
    pool_data: Pool,
    pool_obj_arg: ObjectArg,
    liquidity_val: u128,
    coin_in_type_str: String,
    coin_out_type_str: String,
    type_params_vec: Vec<TypeTag>,
    object_args: CetusObjectArgs,
    simulator_arc: Arc<dyn Simulator>, // Stored simulator
}

impl CetusAdapter {
    pub async fn new(
        simulator: Arc<dyn Simulator>,
        pool_info: &crate::types::Pool,
        coin_in_type: &str,
    ) -> Result<Self> {
        ensure!(pool_info.protocol == Protocol::Cetus, "Not a Cetus pool");

        let pool_object_sui = simulator
            .get_object(&pool_info.pool)
            .await?
            .ok_or_else(|| eyre!("Cetus pool object {} not found via simulator", pool_info.pool))?;

        let layout = simulator.get_object_layout(&pool_info.pool).await?
            .ok_or_else(|| eyre!("Layout not found for Cetus pool {}", pool_info.pool))?;

        let move_object_data = pool_object_sui.data.try_as_move()
            .ok_or_else(|| eyre!("Pool {} is not a Move object", pool_info.pool))?;

        let parsed_pool_struct = MoveStruct::simple_deserialize(move_object_data.contents(), &layout)
            .map_err(|e| eyre!("Failed to deserialize Cetus pool {}: {}", pool_info.pool, e))?;

        let is_pause = extract_bool_from_move_struct(&parsed_pool_struct, "is_pause")?;
        ensure!(!is_pause, "Cetus pool {} is paused", pool_info.pool);

        let liquidity_val = extract_u128_from_move_struct(&parsed_pool_struct, "liquidity")?;

        let determined_coin_out_type = if pool_info.token0_type() == coin_in_type {
            pool_info.token1_type().to_string()
        } else {
            ensure!(pool_info.token1_type() == coin_in_type, "coin_in_type {} not found in pool tokens {}/{}", coin_in_type, pool_info.token0_type(), pool_info.token1_type());
            pool_info.token0_type().to_string()
        };

        let type_params_vec = vec![
            TypeTag::from_str(pool_info.token0_type())?,
            TypeTag::from_str(pool_info.token1_type())?,
        ];

        let pool_obj_arg = shared_obj_arg(&pool_object_sui, true);
        let object_args = get_cetus_object_args(simulator.clone()).await?;

        Ok(Self {
            pool_data: pool_info.clone(),
            pool_obj_arg,
            liquidity_val,
            coin_in_type_str: coin_in_type.to_string(),
            coin_out_type_str: determined_coin_out_type,
            type_params_vec,
            object_args,
            simulator_arc: simulator,
        })
    }

    fn build_swap_args_internal(&self, ctx: &mut TradeCtx, coin_in_arg: Argument) -> Result<Vec<Argument>> {
        let config_arg = ctx.obj_arg(self.object_args.config.clone())?;
        let pool_arg = ctx.obj_arg(self.pool_obj_arg.clone())?;
        let partner_arg = ctx.obj_arg(self.object_args.partner.clone())?;
        let clock_arg = ctx.obj_arg(self.object_args.clock.clone())?;
        Ok(vec![config_arg, pool_arg, partner_arg, coin_in_arg, clock_arg])
    }

    fn build_flashloan_args_internal(&self, ctx: &mut TradeCtx, amount_in: u64) -> Result<Vec<Argument>> {
        let config_arg = ctx.obj_arg(self.object_args.config.clone())?;
        let pool_arg = ctx.obj_arg(self.pool_obj_arg.clone())?;
        let partner_arg = ctx.obj_arg(self.object_args.partner.clone())?;
        let amount_arg = ctx.pure_arg(amount_in)?;
        let by_amount_in_arg = ctx.pure_arg(true)?;
        let clock_arg = ctx.obj_arg(self.object_args.clock.clone())?;
        Ok(vec![config_arg, pool_arg, partner_arg, amount_arg, by_amount_in_arg, clock_arg])
    }

    fn build_repay_args_internal(&self, ctx: &mut TradeCtx, coin_arg: Argument, receipt_arg: Argument) -> Result<Vec<Argument>> {
        let config_arg = ctx.obj_arg(self.object_args.config.clone())?;
        let pool_arg = ctx.obj_arg(self.pool_obj_arg.clone())?;
        let partner_arg = ctx.obj_arg(self.object_args.partner.clone())?;
        Ok(vec![config_arg, pool_arg, partner_arg, coin_arg, receipt_arg])
    }
}

#[async_trait]
impl ProtocolAdapter for CetusAdapter {
    fn support_flashloan(&self) -> bool { true }

    async fn extend_flashloan_tx(&self, ctx: &mut TradeCtx, amount_in: u64) -> Result<FlashResult> {
        let function_name_str = if self.is_a2b() { "flash_swap_a2b" } else { "flash_swap_b2a" };
        let package_id = ObjectID::from_hex_literal(CETUS_DEX_PACKAGE_ID)?;
        let module_name = Identifier::new("cetus")?;
        let function_name = Identifier::new(function_name_str)?;
        let arguments = self.build_flashloan_args_internal(ctx, amount_in)?;
        ctx.add_command(Command::move_call(package_id, module_name, function_name, self.type_params_vec.clone(), arguments));
        let last_idx = ctx.last_command_idx();
        Ok(FlashResult {
            coin_out: Argument::NestedResult(last_idx, 0),
            receipt: Argument::NestedResult(last_idx, 1),
            pool: None,
        })
    }

    async fn extend_repay_tx(&self, ctx: &mut TradeCtx, coin_in_arg: Argument, flash_res: FlashResult) -> Result<Argument> {
        let function_name_str = if self.is_a2b() { "repay_flash_swap_a2b" } else { "repay_flash_swap_b2a" };
        let package_id = ObjectID::from_hex_literal(CETUS_DEX_PACKAGE_ID)?;
        let module_name = Identifier::new("cetus")?;
        let function_name = Identifier::new(function_name_str)?;
        let arguments = self.build_repay_args_internal(ctx, coin_in_arg, flash_res.receipt)?;
        ctx.add_command(Command::move_call(package_id, module_name, function_name, self.type_params_vec.clone(), arguments));
        Ok(Argument::Result(ctx.last_command_idx()))
    }

    async fn extend_trade_tx(&self, ctx: &mut TradeCtx, _sender: SuiAddress, coin_in_arg: Argument, _amount_in: Option<u64>) -> Result<Argument> {
        let function_name_str = if self.is_a2b() { "swap_a2b" } else { "swap_b2a" };
        let package_id = ObjectID::from_hex_literal(CETUS_DEX_PACKAGE_ID)?;
        let module_name = Identifier::new("cetus")?;
        let function_name = Identifier::new(function_name_str)?;
        let arguments = self.build_swap_args_internal(ctx, coin_in_arg)?;
        ctx.add_command(Command::move_call(package_id, module_name, function_name, self.type_params_vec.clone(), arguments));
        Ok(Argument::Result(ctx.last_command_idx()))
    }

    fn coin_in_type(&self) -> String { self.coin_in_type_str.clone() }
    fn coin_out_type(&self) -> String { self.coin_out_type_str.clone() }
    fn protocol(&self) -> Protocol { Protocol::Cetus }
    fn liquidity(&self) -> u128 { self.liquidity_val }
    fn object_id(&self) -> ObjectID { self.pool_data.pool }

    fn flip(&mut self) {
        std::mem::swap(&mut self.coin_in_type_str, &mut self.coin_out_type_str);
    }

    fn is_a2b(&self) -> bool {
        self.pool_data.token_index(&self.coin_in_type_str) == Some(0)
    }

    async fn swap_tx(&self, sender: SuiAddress, recipient: SuiAddress, amount_in: u64) -> Result<TransactionData> {
        let sui_client = SuiClient::new_for_testing().await?;
        let coin_in_obj = coin::get_coin(&sui_client, sender, &self.coin_in_type_str, amount_in).await?;
        let mut trade_ctx = TradeCtx::new_ptb();
        let coin_in_arg = trade_ctx.split_coin_arg(coin_in_obj.object_ref(), amount_in)?;
        let coin_out_arg = self.extend_trade_tx(&mut trade_ctx, sender, coin_in_arg, Some(amount_in)).await?;
        trade_ctx.transfer_arg(recipient, coin_out_arg);
        let pt = trade_ctx.finish_ptb();
        let gas_coins = coin::get_gas_coin_refs(&sui_client, sender, Some(coin_in_obj.coin_object_id)).await?;
        let gas_price = sui_client.read_api().get_reference_gas_price().await?;
        const GAS_BUDGET: u64 = 200_000_000;
        Ok(TransactionData::new_programmable(sender, gas_coins, pt, GAS_BUDGET, gas_price))
    }

    async fn parse_pool_created_event(&self, event: &SuiEvent, sui_client: &SuiClient) -> Result<Pool> {
        let internal_event_data = CetusPoolCreatedInternal::try_from_sui_event(event)?;
        internal_event_data.to_pool_type(sui_client).await
    }

    async fn parse_swap_event(&self, event: &SuiEvent, simulator: Arc<dyn Simulator>) -> Result<SwapEvent> {
        let internal_event_data = CetusSwapEventInternal::try_from_sui_event(event)?;
        internal_event_data.to_swap_event_type(simulator).await
    }

    async fn get_related_object_ids(&self) -> Result<HashSet<String>> {
        Ok(vec![
            CETUS_DEX_PACKAGE_ID, CETUS_CONFIG_ID, CETUS_PARTNER_ID, SUI_CLOCK_OBJECT_ID.to_string().as_str(),
            CETUS_EVENT_PACKAGE_ID, CETUS_FETCHER_SCRIPT_PACKAGE_ID,
            "0xeffc8ae61f439bb34c9b905ff8f29ec56873dcedf81c7123ff2f1f67c45ec302",
            "0x11451575c775a3e633437b827ecbc1eb51a5964b0302210b28f5b89880be21a2",
            "0x70968826ad1b4ba895753f634b0aea68d0672908ca1075a2abdf0fc9e0b2fc6a",
            "0x714a63a0dba6da4f017b42d5d0fb78867f18bcde904868e51d951a5a6f5b7f57",
            "0xbe21a06129308e0495431d12286127897aff07a8ade3970495a4404d97f9eaaa",
            "0xe2b515f0052c0b3f83c23db045d49dbe1732818ccfc5d4596c9482f7f2e76a85",
            "0xe93247b408fe44ed0ee5b6ac508b36325b239d6333e44ffa240dcc0c1a69cdd8",
            "0x74bb5afd49dddf13007101238012c033a5138474e00338126b318b5e3e4603a9",
            "0xbfda3feb64a496c8d7fbb39a152d632ec1d1cefb2010b349adc3460937a592fe"
        ]
        .into_iter()
        .map(|s| s.to_string())
        .collect::<HashSet<String>>())
    }

    async fn get_pool_children_ids(&self, pool_data_from_arg: &Pool, sui_client: &SuiClient) -> Result<Vec<String>> {
        let mut result_ids_set = HashSet::new();

        let pool_obj_response = sui_client.read_api().get_object_with_options(
            pool_data_from_arg.pool,
            SuiObjectDataOptions::new().with_content().with_owner().with_bcs() // Request BCS for layout derivation
        ).await?;

        let pool_sui_object_data = pool_obj_response.data.ok_or_else(|| eyre!("Pool object {} not found via sui_client", pool_data_from_arg.pool))?;
        let pool_sui_object_owner = pool_sui_object_data.owner.as_ref().ok_or_else(|| eyre!("Pool object {} has no owner", pool_data_from_arg.pool))?;


        let layout = get_cetus_pool_layout_internal(pool_data_from_arg.pool, sui_client, &pool_sui_object_data.bcs).await?;

        let move_object_data = pool_sui_object_data.content.ok_or_eyre("Pool data has no content")?.try_as_move()
            .ok_or_else(|| eyre!("Pool {} is not a Move object", pool_data_from_arg.pool))?;
        let parsed_pool_struct = MoveStruct::simple_deserialize(move_object_data.contents(), &layout)
            .map_err(|e| eyre!("Failed to deserialize Cetus pool {}: {}", pool_data_from_arg.pool, e))?;

        let tick_manager_struct = extract_struct_from_move_struct(&parsed_pool_struct, "tick_manager")?;
        let position_manager_struct = extract_struct_from_move_struct(&parsed_pool_struct, "position_manager")?;

        let positions_table_struct = extract_struct_from_move_struct(&position_manager_struct, "positions")?;
        let positions_table_id_struct = extract_struct_from_move_struct(&positions_table_struct, "id")?;
        let positions_table_object_id = extract_object_id_from_move_struct(&positions_table_id_struct, "id")?;

        let mut next_cursor = None;
        loop {
            let df_page = sui_client.read_api().get_dynamic_fields(positions_table_object_id, next_cursor, None).await
                .map_err(|e| eyre!("Failed to get dynamic fields for positions table {}: {:?}", positions_table_object_id, e))?;

            for field_info in df_page.data {
                result_ids_set.insert(field_info.object_id.to_string());
            }
            next_cursor = df_page.next_cursor;
            if next_cursor.is_none() { break; }
        }

        let ticks_table_struct = extract_struct_from_move_struct(&tick_manager_struct, "ticks")?;
        let ticks_table_id_struct = extract_struct_from_move_struct(&ticks_table_struct, "id")?;
        let ticks_table_object_id = extract_object_id_from_move_struct(&ticks_table_id_struct, "id")?;

        let mut next_cursor_ticks = None;
        loop {
            let df_page_ticks = sui_client.read_api().get_dynamic_fields(ticks_table_object_id, next_cursor_ticks, None).await
                 .map_err(|e| eyre!("Failed to get dynamic fields for ticks table {}: {:?}", ticks_table_object_id, e))?;

            for field_info in df_page_ticks.data {
                result_ids_set.insert(field_info.object_id.to_string());
            }
            next_cursor_ticks = df_page_ticks.next_cursor;
            if next_cursor_ticks.is_none() { break; }
        }

        let initial_shared_version = match pool_sui_object_owner {
            Owner::Shared { initial_shared_version } => *initial_shared_version,
            _ => return Err(eyre!("Pool object {} is not shared, required for ObjectArg in simulation", pool_data_from_arg.pool)),
        };

        let pool_obj_arg_for_simulation = ObjectArg::SharedObject {
            id: pool_data_from_arg.pool,
            initial_shared_version,
            mutable: true,
        };

        match fetch_tick_scores_via_cetus_simulation(pool_data_from_arg, pool_obj_arg_for_simulation, self.simulator_arc.clone()).await {
            Ok(tick_scores) => {
                let key_tag_type = TypeTag::U64;
                for tick_score_val in tick_scores {
                    if tick_score_val == 0 { continue; }
                    let key_bytes = bcs::to_bytes(&tick_score_val)?;
                    let tick_dynamic_field_id = derive_dynamic_field_id(ticks_table_object_id, &key_tag_type, &key_bytes)?;
                    result_ids_set.insert(tick_dynamic_field_id.to_string());
                }
            },
            Err(e) => {
                eprintln!("Could not fetch tick scores via simulation for pool {}: {:?}", pool_data_from_arg.pool, e);
            }
        }

        Ok(result_ids_set.into_iter().collect())
    }
}

impl CloneBoxedProtocolAdapter for CetusAdapter {
    fn clone_boxed(&self) -> Box<dyn ProtocolAdapter> {
        Box::new(self.clone())
    }
}

async fn get_cetus_pool_layout_internal(pool_id: ObjectID, sui_client: &SuiClient, object_bcs_opt: &Option<SuiMoveStruct>) -> Result<MoveStructLayout> {
    if let Some(layout) = CETUS_POOL_LAYOUT_CACHE.get() {
        return Ok(layout.clone());
    }

    let bcs_data = object_bcs_opt.as_ref().ok_or_else(|| eyre!("BCS data not available for object {}", pool_id))?;
    let layout = SuiMoveObject::layout_from_struct(bcs_data) // Use layout_from_struct on SuiMoveStruct
        .map_err(|e| eyre!("Error deriving layout from BCS for pool {}: {}", pool_id, e))?;


    let _ = CETUS_POOL_LAYOUT_CACHE.set(layout.clone());
    Ok(layout)
}

fn parse_tick_scores_from_cetus_fetch_event(event: &SuiEvent) -> Result<Vec<u64>> {
    let parsed_json = &event.parsed_json;
    let ticks_json_array = parsed_json["ticks"].as_array()
        .ok_or_eyre("Missing 'ticks' array in Cetus FetchTicksEvent JSON")?;

    let result = ticks_json_array
        .par_iter()
        .filter_map(|tick_value| {
            let index_obj = tick_value["index"].as_object().ok_or_eyre("Missing 'index' object in tick").ok()?;
            let index_bits = index_obj["bits"].as_u64().ok_or_eyre("Missing 'bits' in index object").ok()?;
            let index_i32 = index_bits as i32;
            let tick_score = (index_i32 as i64 + TICK_BOUND) as u64;
            Some(tick_score)
        })
        .collect::<Vec<_>>();
    Ok(result)
}

async fn fetch_tick_scores_via_cetus_simulation(
    pool_data: &Pool,
    pool_obj_arg_for_ptb: ObjectArg,
    simulator: Arc<dyn Simulator>
) -> Result<Vec<u64>> {
    let mut ptb = ProgrammableTransactionBuilder::new();

    let package_id = ObjectID::from_hex_literal(CETUS_FETCHER_SCRIPT_PACKAGE_ID)?;
    let module_name = Identifier::new("fetcher_script").map_err(|e| eyre!(e))?;
    let function_name = Identifier::new("fetch_ticks").map_err(|e| eyre!(e))?;

    let type_args = vec![
        TypeTag::from_str(pool_data.token0_type()).map_err(|e| eyre!(e))?,
        TypeTag::from_str(pool_data.token1_type()).map_err(|e| eyre!(e))?,
    ];

    let args = {
        let pool_arg_in_ptb = ptb.obj(pool_obj_arg_for_ptb)?;
        let start_vec: Vec<u32> = vec![];
        let start_arg_in_ptb = ptb.pure(start_vec)?;
        let limit_arg_in_ptb = ptb.pure(512u64)?;
        vec![pool_arg_in_ptb, start_arg_in_ptb, limit_arg_in_ptb]
    };

    ptb.command(Command::move_call(package_id, module_name, function_name, type_args, args));
    let pt = ptb.finish();

    let sender = SuiAddress::random_for_testing_only();
    let gas_object_id = ObjectID::random();
    let gas_version = SequenceNumber::FOR_TESTING;
    let gas_digest = GasCoin::type_().digest();
    let gas_coins = vec![ObjectRef::new(gas_object_id, gas_version, gas_digest)];

    let tx_data = TransactionData::new_programmable(sender, gas_coins, pt, 1_000_000_000, 1);

    let sim_ctx = SimulateCtx::default();
    let sim_response: SimulateTradeResult = simulator.simulate_transaction(tx_data, sim_ctx).await?;

    let mut tick_scores = Vec::new();
    if let Some(events_data) = sim_response.events {
        for sui_event_item in events_data.data {
            if sui_event_item.type_.address.to_hex_literal() == CETUS_FETCHER_SCRIPT_PACKAGE_ID &&
               sui_event_item.type_.module.as_str() == "fetcher_script" &&
               sui_event_item.type_.name.as_str() == "FetchTicksEvent" {
                 match parse_tick_scores_from_cetus_fetch_event(&sui_event_item) {
                    Ok(scores) => tick_scores.extend(scores),
                    Err(e) => { eprintln!("Error parsing tick scores from Cetus simulated event: {:?}", e); }
                }
            }
        }
    }
    Ok(tick_scores)
}

#[cfg(feature = "dummy_trade_ctx_impls")]
impl TradeCtx {
    pub fn new_ptb() -> Self { Self {} }
    pub fn split_coin_arg(&mut self, _coin_ref: sui_types::base_types::ObjectRef, _amount: u64) -> Result<Argument> { Ok(Argument::GasCoin) }
    pub fn add_command(&mut self, _command: sui_types::transaction::Command) {}
    pub fn last_command_idx(&self) -> u16 { 0 }
    pub fn finish_ptb(self) -> sui_types::transaction::ProgrammableTransaction { sui_types::transaction::ProgrammableTransaction { inputs: vec![], commands: vec![] } }
    pub fn obj_arg(&mut self, _obj_arg: sui_types::transaction::ObjectArg) -> Result<Argument> { Ok(Argument::GasCoin) }
    pub fn pure_arg<T: sui_types::transaction::MoveValue>(&mut self, _val: T) -> Result<Argument> {Ok(Argument::GasCoin)}
}

#[cfg(test)]
impl SuiClient {
    pub async fn new_for_testing() -> Result<Self> {
        eyre::bail!("SuiClient::new_for_testing() is a placeholder and needs actual implementation for tests or be mocked.")
    }
}

#[derive(Debug, Clone, Deserialize)]
struct CetusPoolCreatedInternal {
    pool_id_str: String,
    coin_type_a_str: String,
    coin_type_b_str: String,
}

impl CetusPoolCreatedInternal {
    fn try_from_sui_event(event: &SuiEvent) -> Result<Self> {
        ensure!(event.type_.to_string() == CETUS_POOL_CREATED_EVENT_TYPE, "Event type mismatch for CetusPoolCreated");
        let parsed_json = &event.parsed_json;
        let pool_id_str = parsed_json["pool_id"].as_str().ok_or_eyre("Missing pool_id in CetusPoolCreated event")?.to_string();
        let coin_type_a_str = parsed_json["coin_type_a"].as_str().ok_or_eyre("Missing coin_type_a in CetusPoolCreated event")?.to_string();
        let coin_type_b_str = parsed_json["coin_type_b"].as_str().ok_or_eyre("Missing coin_type_b in CetusPoolCreated event")?.to_string();
        Ok(Self { pool_id_str, coin_type_a_str, coin_type_b_str })
    }

    async fn to_pool_type(&self, sui_client: &SuiClient) -> Result<Pool> {
        let pool_object_id = ObjectID::from_str(&self.pool_id_str)?;
        let token0_type = format!("0x{}", self.coin_type_a_str);
        let token1_type = format!("0x{}", self.coin_type_b_str);

        let token0_decimals = get_coin_decimals(sui_client, &token0_type).await?;
        let token1_decimals = get_coin_decimals(sui_client, &token1_type).await?;

        let opts = SuiObjectDataOptions::new().with_content();
        let pool_obj_response = sui_client.read_api().get_object_with_options(pool_object_id, opts).await?;
        let pool_sui_object_data = pool_obj_response.data.ok_or_eyre("Pool object data not found after creation event handling")?;

        let fee_rate_str = pool_sui_object_data.content.ok_or_eyre("Pool has no content")?
            .try_into_move().ok_or_eyre("Pool content is not MoveObject")?
            .fields["fee_rate"].as_str().ok_or_eyre("Missing or invalid fee_rate field")?;
        let fee_rate = fee_rate_str.parse::<u64>()?;

        let tokens = vec![
            Token::new(&token0_type, token0_decimals),
            Token::new(&token1_type, token1_decimals),
        ];
        let extra = PoolExtra::Cetus { fee_rate };

        Ok(Pool {
            protocol: Protocol::Cetus,
            pool: pool_object_id,
            tokens,
            extra,
        })
    }
}

#[derive(Debug, Clone, Deserialize)]
struct CetusSwapEventInternal {
    pool_id_str: String,
    amount_in_str: String,
    amount_out_str: String,
    is_a2b: bool,
}

impl CetusSwapEventInternal {
     fn try_from_sui_event(event: &SuiEvent) -> Result<Self> {
        ensure!(event.type_.to_string() == CETUS_SWAP_EVENT_TYPE, "Event type mismatch for CetusSwapEvent");
        Self::from_value(&event.parsed_json)
    }

    fn from_value(parsed_json: &Value) -> Result<Self> {
        let pool_id_str = parsed_json["pool"].as_str().ok_or_eyre("Missing pool_id in CetusSwapEvent")?.to_string();
        let amount_in_str = parsed_json["amount_in"].as_str().ok_or_eyre("Missing amount_in in CetusSwapEvent")?.to_string();
        let amount_out_str = parsed_json["amount_out"].as_str().ok_or_eyre("Missing amount_out in CetusSwapEvent")?.to_string();
        let is_a2b = parsed_json["atob"].as_bool().ok_or_eyre("Missing or invalid atob field in CetusSwapEvent")?;
        Ok(Self { pool_id_str, amount_in_str, amount_out_str, is_a2b })
    }

    async fn to_swap_event_type(&self, simulator: Arc<dyn Simulator>) -> Result<SwapEvent> {
        let pool_object_id = ObjectID::from_str(&self.pool_id_str)?;
        let amount_in = self.amount_in_str.parse::<u64>()?;
        let amount_out = self.amount_out_str.parse::<u64>()?;

        let (coin_in_type, coin_out_type) = get_coin_in_out_v2!(pool_object_id, simulator.as_ref(), self.is_a2b).await?;

        Ok(SwapEvent {
            protocol: Protocol::Cetus,
            pool: Some(pool_object_id),
            coins_in: vec![coin_in_type],
            coins_out: vec![coin_out_type],
            amounts_in: vec![amount_in],
            amounts_out: vec![amount_out],
        })
    }
}

```
