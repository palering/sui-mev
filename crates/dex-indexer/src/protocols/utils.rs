use eyre::{bail, ensure, eyre, Result, OptionExt};
use sui_sdk::{
    rpc_types::SuiObjectDataOptions,
    types::{base_types::ObjectID, layout::TypeLayout, TypeTag as SuiTypeTag, move_package::MoveStruct}, // Renamed to avoid conflict
    SuiClient
};
use std::str::FromStr;
use simulator::Simulator; // For the macro
use crate::normalize_coin_type; // Assuming normalize_coin_type is pub in crate root or lib.rs
use sui_types::object::Object as SuiObject; // Added for the macro
use sui_types::parse_sui_struct_tag; // Added for parsing struct tag if necessary

// Define SUI_RPC_NODE here or ensure it's configured globally
// For now, using a placeholder. This should be configurable.
pub const SUI_RPC_NODE: &str = "https://fullnode.mainnet.sui.io:443"; // Placeholder

/// Fetches coin decimals.
pub async fn get_coin_decimals(sui_client: &SuiClient, coin_type_str: &str) -> Result<u8> {
    let coin_metadata = sui_client
        .coin_read_api()
        .get_coin_metadata(coin_type_str.to_string())
        .await?;

    Ok(coin_metadata.ok_or_else(|| eyre::eyre!("Coin metadata not found for {}", coin_type_str))?.decimals)
}

/// Fetches children object IDs for a given parent object.
/// Note: This implementation currently fetches Dynamic Field Object IDs.
/// The exact definition of "children" might vary.
pub async fn get_children_ids(parent_id: ObjectID, sui_client: &SuiClient) -> Result<Vec<String>> {
    // Removed client creation, using passed-in sui_client
    let dynamic_fields = sui_client
        .read_api()
        .get_dynamic_fields(parent_id, None, None)
        .await?;

    let children_ids = dynamic_fields.data
        .into_iter()
        .map(|field_info| field_info.object_id.to_string())
        .collect();

    Ok(children_ids)
}

/// Fetches and normalizes the first two type parameters from a given pool object's type.
/// Assumes these are the coin types.
pub async fn get_pool_coins_type(sui: &SuiClient, pool_id: ObjectID) -> Result<(String, String)> {
    let options = SuiObjectDataOptions::new().with_type();
    let pool_object_response = sui.read_api().get_object_with_options(pool_id, options).await?;

    let pool_object_data = pool_object_response.data.ok_or_eyre(format!("No data found for pool object {}", pool_id))?;
    let pool_struct_tag = pool_object_data.type_.ok_or_eyre(format!("Object {} has no type", pool_id))?.clone().into_struct_tag().ok_or_eyre(format!("Object {} is not a struct", pool_id))?;

    ensure!(pool_struct_tag.type_params.len() >= 2,
            "Pool {} type parameters are less than 2. Found: {:?}", pool_id, pool_struct_tag.type_params);

    let coin_type_1 = pool_struct_tag.type_params[0].to_string();
    let coin_type_2 = pool_struct_tag.type_params[1].to_string();

    Ok((
        normalize_coin_type(&coin_type_1),
        normalize_coin_type(&coin_type_2),
    ))
}

/// Macro to get coin_in_type and coin_out_type from a pool ObjectID using a Simulator provider.
/// It fetches the pool object and its type information, then orders the coins based on the a2b flag.
#[macro_export]
macro_rules! get_coin_in_out_v2 {
    ($pool_id:expr, $provider:expr, $a2b:expr) => {
        async {
            // Ensure imports are available within the async block if not globally in macro scope
            use $crate::normalize_coin_type;
            use sui_types::object::Object as SuiObjectMacro; // Alias to avoid conflict if SuiObject is used outside macro
            use sui_types::parse_sui_struct_tag; // For StructTag parsing, if needed, though type_() is preferred

            let pool_id_obj: ObjectID = $pool_id;
            // The Simulator trait's get_object method returns Result<Option<Object>, Error>
            // Error type needs to be compatible with eyre::Report if using ?
            // Assuming Simulator's Error is compatible or handled.
            let obj_option = $provider.get_object(&pool_id_obj).await.map_err(|e| eyre::eyre!("Simulator get_object error: {:?}", e))?;

            let obj_inner: &SuiObjectMacro = obj_option.as_ref() // Get &Object from Option<Object>
                .ok_or_else(|| eyre::eyre!("Pool object {} not found via provider", pool_id_obj))?;

            let move_obj = obj_inner.data.try_as_move()
                .ok_or_else(|| eyre::eyre!("Object {} is not a Move object", pool_id_obj))?;

            // .type_() on MoveObject returns &StructTag
            let struct_tag = move_obj.type_();
            let type_params = &struct_tag.type_params;

            eyre::ensure!(type_params.len() >= 2,
                "Pool {} type parameters are less than 2. Found: {:?}", pool_id_obj, type_params);

            let coin1_type = normalize_coin_type(&type_params[0].to_string());
            let coin2_type = normalize_coin_type(&type_params[1].to_string());

            let (coin_in_type, coin_out_type) = if $a2b {
                (coin1_type, coin2_type)
            } else {
                (coin2_type, coin1_type)
            };
            Ok((coin_in_type, coin_out_type))
        }
    };
}


// Test for get_pool_coins_type (requires a running Sui node and a valid pool ObjectID)
#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;
    use std::sync::Arc; // For Arc<dyn Simulator>
    use async_trait::async_trait; // For async trait in DummySimulator

    // This test needs a real Sui node and a known pool object ID.
    // The SUI_RPC_NODE const would be used by new_rpc_client.
    #[tokio::test]
    #[ignore] // Ignored because it requires an external service and specific object ID.
    async fn test_fetch_pool_coin_types() {
        let client = SuiClient::new_rpc_client(SUI_RPC_NODE, None).await.unwrap();
        // Replace with an actual pool ID from your test environment or a known mainnet/testnet pool
        let pool_id = ObjectID::from_str("0x0000000000000000000000000000000000000000000000000000000000000001").unwrap();

        match get_pool_coins_type(&client, pool_id).await {
            Ok((type1, type2)) => {
                println!("Coin Type 1: {}", type1);
                println!("Coin Type 2: {}", type2);
                // Add assertions here based on the known types of the pool_id
                // e.g., assert_eq!(type1, "0x2::sui::SUI");
            }
            Err(e) => {
                eprintln!("Error fetching pool coin types: {:?}", e);
                panic!("Test failed: {:?}", e);
            }
        }
    }

    // Dummy simulator for macro testing
    #[cfg(test)]
    #[derive(Clone)] // Clone is often needed for simulators if they are passed around
    struct DummySimulator;

    #[cfg(test)]
    #[async_trait]
    impl Simulator for DummySimulator {
         async fn get_object(&self, _object_id: &ObjectID) -> Result<Option<SuiObject>> {
            // Return a mock SuiObject here for testing the macro
            // This is non-trivial to mock correctly with all necessary fields for type_().
            // For a simple test, you might construct a MoveObject with a specific StructTag.
            // Example:
            // use sui_types::gas_coin::GasCoin;
            // use sui_types::move_object::MoveObject;
            // use sui_types::parse_sui_struct_tag;
            // let struct_tag = parse_sui_struct_tag("0x123::pool::Pool<0x2::sui::SUI, 0xCOIN::coin::COIN>").unwrap();
            // let move_obj = MoveObject::new_gas_coin_with_balance_and_type_for_testing(struct_tag, 100);
            // Ok(Some(SuiObject::new_from_move(move_obj, sui_types::object::Owner::Shared{initial_shared_version: sui_types::base_types::SequenceNumber::new()} , sui_types::storage::DeleteKind::Single)))
            eyre::bail!("DummySimulator::get_object needs a mock SuiObject response");
         }
         async fn get_objects(&self, _object_ids: &[ObjectID]) -> Result<Vec<SuiObject>> { unimplemented!() }
         async fn get_object_with_options(&self, _object_id: ObjectID, _options: SuiObjectDataOptions) -> Result<sui_sdk::rpc_types::SuiObjectResponse> { unimplemented!() }
         async fn get_object_layout(&self, _object_id: &ObjectID) -> Result<Option<TypeLayout>> { unimplemented!() }
         async fn simulate_transaction(&self, _tx_data: sui_sdk::types::transaction::TransactionData, _extra_args: simulator::SimulateCtx) -> Result<simulator::TradeResult> { unimplemented!() }
         fn clone_box(&self) -> Box<dyn Simulator> { Box::new(self.clone()) }
    }


    // Test for get_coin_in_out_v2 macro (also needs a running node or a sophisticated mock)
    #[tokio::test]
    #[ignore] // Ignored due to needing a functional DummySimulator or real service
    async fn test_macro_get_coins() {
        let simulator = Arc::new(DummySimulator);
        let pool_id = ObjectID::from_str("0x0000000000000000000000000000000000000000000000000000000000000001").unwrap(); // Dummy ID
        let a2b = true;

        // This test will fail because DummySimulator::get_object is not fully implemented.
        match get_coin_in_out_v2!(pool_id, simulator.as_ref(), a2b).await {
            Ok((coin_in, coin_out)) => {
                println!("Macro - Coin In: {}, Coin Out: {}", coin_in, coin_out);
            }
            Err(e) => {
                eprintln!("Macro error: {:?}", e);
                // For this test, we expect an error from the unimplemented DummySimulator
                // assert!(e.to_string().contains("DummySimulator::get_object needs a mock SuiObject response"));
            }
        }
    }
}
```
