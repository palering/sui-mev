// crates/dex-indexer/src/protocols/haedal_adapter.rs
use std::collections::HashSet;
use std::sync::Arc;
use async_trait::async_trait;
use eyre::{Result, eyre, ensure};
use move_core_types::annotated_value::MoveStruct;
use move_core_types::identifier::Identifier;
use sui_sdk::{SuiClient, rpc_types::SuiEvent};
use sui_types::{
    base_types::{ObjectID, SuiAddress},
    transaction::{Argument, Command, TransactionData},
    TypeTag, // Added for self.type_params
};
use simulator::Simulator;

use crate::protocols::{common_trade_types::{FlashResult, TradeCtx}, ProtocolAdapter, CloneBoxedProtocolAdapter, utils as protocol_utils};
use crate::types::{Pool as IndexerPool, Protocol as IndexerProtocol, SwapEvent as IndexerSwapEvent};
use crate::utils::normalize_coin_type;

// TODO: Replace with actual Haedal package ID
const HAEDAL_PACKAGE_ID_PLACEHOLDER: &str = "0xHAEDAL_PACKAGE_ID_PLACEHOLDER";

#[derive(Clone, Debug)]
pub struct HaedalAdapter {
    pub pool_id: ObjectID,
    pub coin_in_type: String,
    pub coin_out_type: String,
    pub type_params: Vec<TypeTag>, // For coin_a, coin_b type tags
    pub liquidity: u128,           // Actual field name from Haedal pool needed
    pub simulator: Arc<dyn Simulator>, // Store simulator for potential use
    // Add any Haedal-specific cached ObjectArgs if applicable
}

impl HaedalAdapter {
    pub async fn new(simulator: Arc<dyn Simulator>, pool: &IndexerPool, coin_in_type: &str) -> Result<Self> {
        ensure!(pool.protocol == IndexerProtocol::Haedal, "Invalid protocol for HaedalAdapter");

        let pool_obj = simulator.get_object(&pool.pool)
            .await?
            .ok_or_else(|| eyre!("Haedal pool object {} not found", pool.pool))?;

        let layout = simulator.get_object_layout(&pool.pool)
            .await?
            .ok_or_else(|| eyre!("Layout not found for Haedal pool {}", pool.pool))?;

        let move_struct = MoveStruct::simple_deserialize(
            pool_obj.data.try_as_move().ok_or_eyre("Not a move object")?.contents(),
            &layout
        )?;

        // Placeholder logic for type_params and coin_out_type
        // TODO: Replace with actual Haedal pool structure inspection
        ensure!(move_struct.type_.type_params.len() >= 2, "Haedal pool type parameters missing coin types");
        let actual_coin_a_type_tag = move_struct.type_.type_params[0].clone();
        let actual_coin_b_type_tag = move_struct.type_.type_params[1].clone();

        let actual_coin_a_type = normalize_coin_type(&actual_coin_a_type_tag.to_string());
        let actual_coin_b_type = normalize_coin_type(&actual_coin_b_type_tag.to_string());

        let normalized_coin_in_type = normalize_coin_type(coin_in_type);

        let (coin_in_type_final, coin_out_type_final, type_params_final) = if actual_coin_a_type == normalized_coin_in_type {
            (actual_coin_a_type, actual_coin_b_type, vec![actual_coin_a_type_tag, actual_coin_b_type_tag])
        } else if actual_coin_b_type == normalized_coin_in_type {
            (actual_coin_b_type, actual_coin_a_type, vec![actual_coin_b_type_tag, actual_coin_a_type_tag])
        } else {
            return Err(eyre!("Input coin type {} not found in Haedal pool {} ({}, {})",
                coin_in_type, pool.pool, actual_coin_a_type, actual_coin_b_type));
        };

        // TODO: Replace "liquidity_field_placeholder" with actual field name for liquidity
        let liquidity = protocol_utils::extract_u128_from_move_struct(&move_struct, "liquidity_field_placeholder").unwrap_or(0);

        Ok(Self {
            pool_id: pool.pool,
            coin_in_type: coin_in_type_final,
            coin_out_type: coin_out_type_final,
            type_params: type_params_final,
            liquidity,
            simulator,
        })
    }
}

#[async_trait]
impl ProtocolAdapter for HaedalAdapter {
    fn support_flashloan(&self) -> bool {
        // TODO: Determine if Haedal supports flashloans
        false
    }

    async fn extend_flashloan_tx(&self, _ctx: &mut TradeCtx, _amount: u64) -> Result<FlashResult> {
        eyre::bail!("Flashloan not supported by HaedalAdapter (default or not yet implemented)")
    }

    async fn extend_repay_tx(&self, _ctx: &mut TradeCtx, _coin: Argument, _flash_res: FlashResult) -> Result<Argument> {
        eyre::bail!("Flashloan not supported by HaedalAdapter (default or not yet implemented)")
    }

    async fn extend_trade_tx(&self, ctx: &mut TradeCtx, _sender: SuiAddress, coin_in: Argument, _amount_in: Option<u64>) -> Result<Argument> {
        // TODO: Replace with actual Haedal swap logic
        let package_id = ObjectID::from_hex_literal(HAEDAL_PACKAGE_ID_PLACEHOLDER)?;
        let module_name = Identifier::new("swap_module_placeholder")?;
        let function_name = Identifier::new("swap_function_placeholder")?;

        // TODO: Build actual arguments for Haedal swap
        let arguments = vec![
            ctx.obj_mut(self.pool_id)?,
            coin_in,
        ];

        ctx.add_command(Command::move_call(
            package_id,
            module_name,
            function_name,
            self.type_params.clone(),
            arguments,
        ));
        Ok(Argument::Result(ctx.last_command_idx()))
    }

    fn coin_in_type(&self) -> String { self.coin_in_type.clone() }
    fn coin_out_type(&self) -> String { self.coin_out_type.clone() }

    fn protocol(&self) -> IndexerProtocol {
        IndexerProtocol::Haedal
    }

    fn liquidity(&self) -> u128 { self.liquidity }
    fn object_id(&self) -> ObjectID { self.pool_id }

    fn flip(&mut self) {
        std::mem::swap(&mut self.coin_in_type, &mut self.coin_out_type);
        if self.type_params.len() == 2 {
            self.type_params.swap(0, 1);
        }
    }

    fn is_a2b(&self) -> bool {
        if let Some(first_type_param) = self.type_params.get(0) {
            normalize_coin_type(&first_type_param.to_string()) == self.coin_in_type
        } else {
            true
        }
    }

    async fn swap_tx(&self, sender: SuiAddress, recipient: SuiAddress, amount_in: u64) -> Result<TransactionData> {
        // TODO: Replace with actual Haedal swap_tx logic
        let sui_client = SuiClient::new_for_testing(None).await?;

        let gas_coins = protocol_utils::get_gas_coins_for_testing(&sui_client, sender, None).await?;
        let gas_price = sui_client.reference_gas_price().await?;

        let mut ctx = TradeCtx::default();
        let coin_in_obj = protocol_utils::get_coin_object_arg_for_testing(&sui_client, sender, &self.coin_in_type, amount_in, &mut ctx).await?;

        let coin_out_arg = self.extend_trade_tx(&mut ctx, sender, coin_in_obj, Some(amount_in)).await?;
        ctx.transfer_arg(recipient, coin_out_arg);

        Ok(TransactionData::new_programmable(
            sender,
            gas_coins,
            ctx.into_programmable_transaction(),
            protocol_utils::DEFAULT_GAS_BUDGET,
            gas_price,
        ))
    }

    async fn parse_pool_created_event(&self, event: &SuiEvent, sui_client: &SuiClient) -> Result<IndexerPool> {
        // TODO: Replace with actual Haedal PoolCreatedEvent parsing
        let expected_event_type = format!("{}::pool_events::PoolCreatedEventPlaceholder", HAEDAL_PACKAGE_ID_PLACEHOLDER);
        ensure!(event.type_.to_string() == expected_event_type, "Event type mismatch for Haedal PoolCreatedEvent");

        let pool_id_str = event.parsed_json.get("pool_id").and_then(|v| v.as_str()).ok_or_eyre("Missing pool_id in event")?;
        let pool_id = ObjectID::from_hex_literal(pool_id_str)?;

        let token_a_type_str = event.parsed_json.get("token_a_type").and_then(|v| v.as_str()).ok_or_eyre("Missing token_a_type in event")?;
        let token_b_type_str = event.parsed_json.get("token_b_type").and_then(|v| v.as_str()).ok_or_eyre("Missing token_b_type in event")?;

        let (decimals_a, decimals_b) = protocol_utils::get_coin_decimals_pair(sui_client, token_a_type_str, token_b_type_str).await?;

        Ok(IndexerPool {
            pool: pool_id,
            protocol: IndexerProtocol::Haedal,
            token0: token_a_type_str.to_string(),
            token1: token_b_type_str.to_string(),
            token0_decimals: decimals_a,
            token1_decimals: decimals_b,
            ..Default::default()
        })
    }

    async fn parse_swap_event(&self, event: &SuiEvent, _simulator: Arc<dyn Simulator>) -> Result<IndexerSwapEvent> {
        // TODO: Replace with actual Haedal SwapEvent parsing
        let expected_event_type = format!("{}::pool_events::SwapEventPlaceholder", HAEDAL_PACKAGE_ID_PLACEHOLDER);
        ensure!(event.type_.to_string() == expected_event_type, "Event type mismatch for Haedal SwapEvent");

        let pool_id_str = event.parsed_json.get("pool_id").and_then(|v| v.as_str()).ok_or_eyre("Missing pool_id in event")?;
        let pool_id = ObjectID::from_hex_literal(pool_id_str)?;

        let input_amount_str = event.parsed_json.get("input_amount").and_then(|v| v.as_str()).ok_or_eyre("Missing input_amount")?;
        let input_amount = input_amount_str.parse::<u64>()?;

        let output_amount_str = event.parsed_json.get("output_amount").and_then(|v| v.as_str()).ok_or_eyre("Missing output_amount")?;
        let output_amount = output_amount_str.parse::<u64>()?;

        let input_coin_type = event.parsed_json.get("input_coin_type").and_then(|v| v.as_str()).ok_or_eyre("Missing input_coin_type")?.to_string();
        let output_coin_type = event.parsed_json.get("output_coin_type").and_then(|v| v.as_str()).ok_or_eyre("Missing output_coin_type")?.to_string();

        Ok(IndexerSwapEvent {
            pool_id,
            input_amount,
            output_amount,
            a_to_b: true, // TODO: Determine direction
            sender: event.sender,
            active_liquidity: None,
            protocol: IndexerProtocol::Haedal,
            coin_in_type: input_coin_type,
            coin_out_type: output_coin_type,
            timestamp_ms: event.timestamp_ms.unwrap_or(0),
            tx_digest: event.id.tx_digest,
            ..Default::default()
        })
    }

    async fn get_related_object_ids(&self) -> Result<HashSet<String>> {
        Ok(HashSet::from([HAEDAL_PACKAGE_ID_PLACEHOLDER.to_string()]))
    }

    async fn get_pool_children_ids(&self, _pool: &IndexerPool, _sui_client: &SuiClient) -> Result<Vec<String>> {
        Ok(Vec::new())
    }
}

impl CloneBoxedProtocolAdapter for HaedalAdapter {
    fn clone_boxed(&self) -> Box<dyn ProtocolAdapter> {
        Box::new(self.clone())
    }
}
