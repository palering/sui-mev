pub mod aftermath_adapter;
pub mod cetus_adapter;
pub mod haedal_adapter; // Newly added
pub mod scallop_adapter;
pub mod common_trade_types;
pub mod utils;

use std::{collections::HashSet, sync::Arc};

use async_trait::async_trait;
use eyre::Result;
use simulator::Simulator;
use sui_sdk::{rpc_types::SuiEvent, SuiClient};
use sui_types::{
    base_types::{ObjectID, SuiAddress},
    transaction::{Argument, TransactionData},
};

use crate::types::{Pool, Protocol, SwapEvent};

// Re-export the common types
pub use common_trade_types::{FlashResult, TradeCtx};

#[async_trait]
pub trait ProtocolAdapter: Send + Sync + CloneBoxedProtocolAdapter {
    // Methods from Dex trait
    fn support_flashloan(&self) -> bool {
        false
    }

    async fn extend_flashloan_tx(
        &self,
        _ctx: &mut TradeCtx,
        _amount: u64,
    ) -> Result<FlashResult> {
        eyre::bail!("flashloan not supported")
    }

    async fn extend_repay_tx(
        &self,
        _ctx: &mut TradeCtx,
        _coin: Argument,
        _flash_res: FlashResult,
    ) -> Result<Argument> {
        eyre::bail!("flashloan not supported")
    }

    async fn extend_trade_tx(
        &self,
        ctx: &mut TradeCtx,
        sender: SuiAddress,
        coin_in: Argument,
        amount_in: Option<u64>,
    ) -> Result<Argument>;

    fn coin_in_type(&self) -> String;
    fn coin_out_type(&self) -> String;
    fn protocol(&self) -> Protocol;
    fn liquidity(&self) -> u128;
    fn object_id(&self) -> ObjectID;

    fn flip(&mut self);

    fn is_a2b(&self) -> bool;
    async fn swap_tx(
        &self,
        sender: SuiAddress,
        recipient: SuiAddress,
        amount_in: u64,
    ) -> Result<TransactionData>;

    // New methods for protocol-specific indexing tasks
    async fn parse_pool_created_event(
        &self,
        event: &SuiEvent,
        sui_client: &SuiClient,
    ) -> Result<Pool>;

    async fn parse_swap_event(
        &self,
        event: &SuiEvent,
        simulator: Arc<dyn Simulator>,
    ) -> Result<SwapEvent>;

    async fn get_related_object_ids(&self) -> Result<HashSet<String>>;

    async fn get_pool_children_ids(
        &self,
        pool: &Pool,
        sui_client: &SuiClient, // Changed from simulator: Arc<dyn Simulator>
    ) -> Result<Vec<String>>;
}

pub trait CloneBoxedProtocolAdapter {
    fn clone_boxed(&self) -> Box<dyn ProtocolAdapter>;
}

impl<T> CloneBoxedProtocolAdapter for T
where
    T: 'static + ProtocolAdapter + Clone,
{
    fn clone_boxed(&self) -> Box<dyn ProtocolAdapter> {
        Box::new(self.clone())
    }
}

impl Clone for Box<dyn ProtocolAdapter> {
    fn clone(&self) -> Box<dyn ProtocolAdapter> {
        self.clone_boxed()
    }
}
