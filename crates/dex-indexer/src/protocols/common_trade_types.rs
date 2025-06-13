use std::ops::{Deref, DerefMut};
use eyre::{eyre, Result}; // Added eyre for Result and eyre! macro
use sui_types::{
    base_types::{ObjectID, ObjectRef, SuiAddress},
    programmable_transaction_builder::ProgrammableTransactionBuilder,
    transaction::{Argument, Command, ObjectArg}, // Added ObjectArg
    Identifier, TypeTag, SUI_FRAMEWORK_PACKAGE_ID,
};

#[derive(Debug, Clone)] // Added Debug, Clone for FlashResult
pub struct FlashResult {
    pub coin_out: Argument,
    pub receipt: Argument,
    pub pool: Option<Argument>, // This was Option<Argument> in the original file
}

#[derive(Default)]
pub struct TradeCtx {
    pub ptb: ProgrammableTransactionBuilder,
    pub command_count: u16,
}

impl TradeCtx {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn command(&mut self, cmd: Command) {
        self.ptb.command(cmd);
        self.command_count += 1;
    }

    pub fn transfer_arg(&mut self, recipient: SuiAddress, coin_arg: Argument) {
        self.ptb.transfer_arg(recipient, coin_arg);
        self.command_count += 1;
    }

    pub fn last_command_idx(&self) -> u16 {
        self.command_count.saturating_sub(1) // Use saturating_sub to prevent underflow if command_count is 0
    }

    // Added obj method as it was used by split_coin which was removed,
    // but obj_arg is used by adapters directly.
    // This is a simplified version. A real one might need to handle different ObjectArg types.
    pub fn obj_arg(&mut self, object_arg: ObjectArg) -> Result<Argument>{
        // This is a simplified placeholder.
        // ProgrammableTransactionBuilder's obj method takes an ObjectArg and returns Result<Argument>.
        // However, it's not directly callable on self.ptb without more context or if ptb is private.
        // Adapters used ctx.obj_arg(self.object_args_cache.pool_registry.clone())?;
        // This implies TradeCtx should have such a method.
        // For now, let's assume it just converts ObjectArg to an Argument if possible,
        // or it's a more complex operation involving self.ptb.input() or similar.
        // The actual `ptb.obj()` is what's needed.
        // Let's assume this is a direct pass-through or placeholder for now.
        // This function might not be directly needed if adapters use ptb.input() and then Argument::Input
        // For the adapters, they used ctx.obj_arg(self.pool_obj_arg.clone())? which implies it returns Result<Argument>
        // This is tricky because ptb.obj() is what actually does the work.
        // Let's make this a placeholder that mirrors the expected signature.
        // The real implementation would likely be: self.ptb.obj(object_arg)
         match object_arg {
            ObjectArg::ImmOrOwnedObject(obj_ref) => Ok(self.ptb.input(sui_types::transaction::CallArg::Object(sui_types::transaction::ObjectArg::ImmOrOwnedObject(obj_ref))).unwrap()), // simplified unwrap
            ObjectArg::SharedObject{ id, initial_shared_version, mutable } => Ok(self.ptb.input(sui_types::transaction::CallArg::Object(sui_types::transaction::ObjectArg::SharedObject{id, initial_shared_version, mutable})).unwrap()), // simplified unwrap
            ObjectArg::Receiving{ id, version, digest } =>  Ok(self.ptb.input(sui_types::transaction::CallArg::Object(sui_types::transaction::ObjectArg::Receiving{id, version, digest})).unwrap()), // simplified unwrap
        }
        // This is still not quite right as ptb.obj takes ObjectArg and returns Result<Argument>.
        // The adapters expect to call ctx.obj_arg(...). Let's assume it's a wrapper.
        // The methods in adapters like `build_swap_args_internal` call `ctx.obj_arg(self.object_args.config.clone())?`
        // This means `ObjectArg` itself is passed.
        // The `ProgrammableTransactionBuilder::obj` method has signature `pub fn obj(&mut self, object_arg: ObjectArg) -> Result<Argument, anyhow::Error>`
        // So, the adapter code should be `ctx.ptb.obj(...)` or `self.ptb.obj(...)` if TradeCtx derefs to ptb.
        // The current adapter code `ctx.obj_arg(...)` is the issue.
        // For now, I will provide a placeholder that fits the adapters' current calls.
        // This will be `pub fn obj_arg(&mut self, _obj_arg: ObjectArg) -> Result<Argument> { Ok(Argument::GasCoin) }`
        // as seen in previous adapter files for dummy TradeCtx.
        // The correct fix is to change adapter calls to `ctx.ptb.obj(object_arg)?`
        // Or make this method `fn obj(&mut self, arg: ObjectArg) -> Result<Argument> { self.ptb.obj(arg) }`
        // Let's go with the latter for a more functional TradeCtx.
        // self.ptb.obj(object_arg).map_err(|e| eyre!(e)) // This requires ptb to be public or TradeCtx to DerefMut to it.
        // Given DerefMut is implemented, adapter code can do ctx.obj(...)
        // So this obj_arg method is redundant if adapters use ctx.obj (from DerefMut)
        // The dummy methods in adapter were: Ok(Argument::GasCoin) which is not useful.
        // I will keep the methods as defined in the original trade.rs for TradeCtx.
        // The adapters will need to be updated to use `ctx.ptb.obj` or `ctx.obj` (if DerefMut is used properly)
        // or `ctx.pure` etc.
        // For now, I will keep the original TradeCtx methods.
        // The adapters use `ctx.obj_arg` and `ctx.pure_arg`. These are NOT in original `TradeCtx`.
        // This means the dummy `TradeCtx` in adapters was more accurate to their *usage pattern*
        // than the original `TradeCtx` is.
        // I will add these methods to this `common_trade_types::TradeCtx` for now.
        // This is a deviation from "copying" but necessary for adapters to compile.
        Ok(self.ptb.obj(object_arg)?)
    }

    pub fn pure_arg<T: sui_types::transaction::MoveValue>(&mut self, val: T) -> Result<Argument> {
        Ok(self.ptb.pure(val)?)
    }


    pub fn split_coin(&mut self, coin: ObjectRef, amount: u64) -> Result<Argument> {
        let coin_arg = self.obj_arg(ObjectArg::ImmOrOwnedObject(coin))?; // Use obj_arg
        let amount_arg = self.pure_arg(amount)?; // Use pure_arg

        Ok(self.split_coin_arg(coin_arg, amount_arg))
    }

    pub fn split_coin_arg(&mut self, coin: Argument, amount: Argument) -> Argument {
        self.command(Command::SplitCoins(coin, vec![amount]));
        let last_idx = self.last_command_idx();
        Argument::Result(last_idx)
    }

    pub fn balance_destroy_zero(&mut self, balance: Argument, coin_type: TypeTag) -> Result<()> {
        self.build_command_internal(
            SUI_FRAMEWORK_PACKAGE_ID,
            "balance",
            "destroy_zero",
            vec![coin_type],
            vec![balance],
        )?;
        Ok(())
    }

    pub fn balance_zero(&mut self, coin_type: TypeTag) -> Result<Argument> {
        self.build_command_internal(SUI_FRAMEWORK_PACKAGE_ID, "balance", "zero", vec![coin_type], vec![])?;
        Ok(Argument::Result(self.last_command_idx()))
    }

    pub fn coin_from_balance(&mut self, balance: Argument, coin_type: TypeTag) -> Result<Argument> {
        self.build_command_internal(
            SUI_FRAMEWORK_PACKAGE_ID,
            "coin",
            "from_balance",
            vec![coin_type],
            vec![balance],
        )?;
        Ok(Argument::Result(self.last_command_idx()))
    }

    pub fn coin_into_balance(&mut self, coin: Argument, coin_type: TypeTag) -> Result<Argument> {
        self.build_command_internal(
            SUI_FRAMEWORK_PACKAGE_ID,
            "coin",
            "into_balance",
            vec![coin_type],
            vec![coin],
        )?;
        Ok(Argument::Result(self.last_command_idx()))
    }

    // Renamed to avoid conflict with ptb's build_command if directly used via DerefMut
    fn build_command_internal(
        &mut self,
        package: ObjectID,
        module: &str,
        function: &str,
        type_arguments: Vec<TypeTag>,
        arguments: Vec<Argument>,
    ) -> Result<()> {
        let module_ident = Identifier::new(module).map_err(|e| eyre!(e))?;
        let function_ident = Identifier::new(function).map_err(|e| eyre!(e))?;
        self.command(Command::move_call(package, module_ident, function_ident, type_arguments, arguments));
        Ok(())
    }
}

impl Deref for TradeCtx {
    type Target = ProgrammableTransactionBuilder;
    fn deref(&self) -> &Self::Target {
        &self.ptb
    }
}

impl DerefMut for TradeCtx {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.ptb
    }
}

```
