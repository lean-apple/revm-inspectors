//! Javascript inspector

use crate::tracing::{
    config::TraceStyle,
    js::{
        bindings::{
            CallFrame, Contract, EvmDbRef, FrameResult, JsEvmContext, MemoryRef, StackRef, StepLog,
        },
        builtins::{register_builtins, to_serde_value, PrecompileList},
    },
    types::CallKind,
    utils, CallInputExt, TransactionContext,
};
use alloc::{
    format,
    string::{String, ToString},
    vec::Vec,
};
use alloy_primitives::{Address, Bytes, Log, U256};
pub use boa_engine::vm::RuntimeLimits;
use boa_engine::{js_string, Context, JsError, JsObject, JsResult, JsValue, Source};
use core::borrow::Borrow;
use revm::{
    bytecode::OpCode,
    context::JournalTr,
    context_interface::{
        result::{ExecutionResult, HaltReasonTr, Output, ResultAndState},
        Block, ContextTr, TransactTo, Transaction,
    },
    inspector::JournalExt,
    interpreter::{
        interpreter_types::{Jumps, LoopControl},
        CallInputs, CallOutcome, CallScheme, CreateInputs, CreateOutcome, Gas, InstructionResult,
        Interpreter, InterpreterAction, InterpreterResult,
    },
    DatabaseRef, Inspector,
};

pub(crate) mod bindings;
pub(crate) mod builtins;

/// The maximum number of iterations in a loop.
///
/// Once exceeded, the loop will throw an error.
// An empty loop with this limit takes around 50ms to fail.
pub const LOOP_ITERATION_LIMIT: u64 = 200_000;

/// The recursion limit for function calls.
///
/// Once exceeded, the function will throw an error.
pub const RECURSION_LIMIT: usize = 10_000;

/// A javascript inspector that will delegate inspector functions to javascript functions
///
/// See also <https://geth.ethereum.org/docs/developers/evm-tracing/custom-tracer#custom-javascript-tracing>
#[derive(Debug)]
pub struct JsInspector {
    ctx: Context,
    /// The javascript config provided to the inspector.
    _js_config_value: JsValue,
    /// The input config object.
    config: serde_json::Value,
    /// The evaluated object that contains the inspector functions.
    obj: JsObject,
    /// The context of the transaction that is being inspected.
    transaction_context: TransactionContext,

    /// The javascript function that will be called when the result is requested.
    result_fn: JsObject,
    fault_fn: JsObject,

    // EVM inspector hook functions
    /// Invoked when the EVM enters a new call that is _NOT_ the top level call.
    ///
    /// Corresponds to [Inspector::call] and [Inspector::create_end] but is also invoked on
    /// [Inspector::selfdestruct].
    enter_fn: Option<JsObject>,
    /// Invoked when the EVM exits a call that is _NOT_ the top level call.
    ///
    /// Corresponds to [Inspector::call_end] and [Inspector::create_end] but also invoked after
    /// selfdestruct.
    exit_fn: Option<JsObject>,
    /// Executed before each instruction is executed.
    step_fn: Option<JsObject>,
    /// Keeps track of the current call stack.
    call_stack: Vec<CallStackItem>,
    /// Marker to track whether the precompiles have been registered.
    precompiles_registered: bool,
    /// Tracker for PC recorded in start_step
    last_start_step_pc: Option<usize>,
}

impl JsInspector {
    /// Creates a new inspector from a javascript code snipped that evaluates to an object with the
    /// expected fields and a config object.
    ///
    /// The object must have the following fields:
    ///  - `result`: a function that will be called when the result is requested.
    ///  - `fault`: a function that will be called when the transaction fails.
    ///
    /// Optional functions are invoked during inspection:
    /// - `setup`: a function that will be called before the inspection starts.
    /// - `enter`: a function that will be called when the execution enters a new call.
    /// - `exit`: a function that will be called when the execution exits a call.
    /// - `step`: a function that will be called when the execution steps to the next instruction.
    ///
    /// This also accepts a sender half of a channel to communicate with the database service so the
    /// DB can be queried from inside the inspector.
    pub fn new(code: String, config: serde_json::Value) -> Result<Self, JsInspectorError> {
        Self::with_transaction_context(code, config, Default::default())
    }

    /// Creates a new inspector from a javascript code snippet. See also [Self::new].
    ///
    /// This also accepts a [TransactionContext] that gives the JS code access to some contextual
    /// transaction infos.
    pub fn with_transaction_context(
        code: String,
        config: serde_json::Value,
        transaction_context: TransactionContext,
    ) -> Result<Self, JsInspectorError> {
        // Instantiate the execution context
        let mut ctx = Context::default();

        // Apply the default runtime limits
        // This is a safe guard to prevent infinite loops
        ctx.runtime_limits_mut().set_loop_iteration_limit(LOOP_ITERATION_LIMIT);
        ctx.runtime_limits_mut().set_recursion_limit(RECURSION_LIMIT);

        register_builtins(&mut ctx)?;

        // evaluate the code
        let code = format!("({code})");
        let obj =
            ctx.eval(Source::from_bytes(code.as_bytes())).map_err(JsInspectorError::EvalCode)?;

        let obj = obj.as_object().cloned().ok_or(JsInspectorError::ExpectedJsObject)?;

        // ensure all the fields are callables, if present

        let result_fn = obj
            .get(js_string!("result"), &mut ctx)?
            .as_object()
            .cloned()
            .ok_or(JsInspectorError::ResultFunctionMissing)?;
        if !result_fn.is_callable() {
            return Err(JsInspectorError::ResultFunctionMissing);
        }

        let fault_fn = obj
            .get(js_string!("fault"), &mut ctx)?
            .as_object()
            .cloned()
            .ok_or(JsInspectorError::FaultFunctionMissing)?;
        if !fault_fn.is_callable() {
            return Err(JsInspectorError::FaultFunctionMissing);
        }

        let enter_fn = obj
            .get(js_string!("enter"), &mut ctx)?
            .as_object()
            .cloned()
            .filter(|o| o.is_callable());
        let exit_fn =
            obj.get(js_string!("exit"), &mut ctx)?.as_object().cloned().filter(|o| o.is_callable());
        let step_fn =
            obj.get(js_string!("step"), &mut ctx)?.as_object().cloned().filter(|o| o.is_callable());

        let _js_config_value =
            JsValue::from_json(&config, &mut ctx).map_err(JsInspectorError::InvalidJsonConfig)?;

        if let Some(setup_fn) = obj.get(js_string!("setup"), &mut ctx)?.as_object() {
            if !setup_fn.is_callable() {
                return Err(JsInspectorError::SetupFunctionNotCallable);
            }

            // call setup()
            setup_fn
                .call(&(obj.clone().into()), core::slice::from_ref(&_js_config_value), &mut ctx)
                .map_err(JsInspectorError::SetupCallFailed)?;
        }

        Ok(Self {
            ctx,
            _js_config_value,
            config,
            obj,
            transaction_context,
            result_fn,
            fault_fn,
            enter_fn,
            exit_fn,
            step_fn,
            call_stack: Default::default(),
            precompiles_registered: false,
            last_start_step_pc: None,
        })
    }

    /// Returns the config object.
    pub const fn config(&self) -> &serde_json::Value {
        &self.config
    }

    /// Returns the transaction context.
    pub const fn transaction_context(&self) -> &TransactionContext {
        &self.transaction_context
    }

    /// Sets the transaction context.
    pub fn set_transaction_context(&mut self, transaction_context: TransactionContext) {
        self.transaction_context = transaction_context;
    }

    /// Applies the runtime limits to the JS context.
    ///
    /// By default
    pub fn set_runtime_limits(&mut self, limits: RuntimeLimits) {
        self.ctx.set_runtime_limits(limits);
    }

    /// Calls the result function and returns the result as [serde_json::Value].
    ///
    /// Note: This is supposed to be called after the inspection has finished.
    pub fn json_result<DB>(
        &mut self,
        res: ResultAndState<impl HaltReasonTr>,
        tx: &impl Transaction,
        block: &impl Block,
        db: &DB,
    ) -> Result<serde_json::Value, JsInspectorError>
    where
        DB: DatabaseRef,
        <DB as DatabaseRef>::Error: core::fmt::Display,
    {
        let result = self.result(res, tx, block, db)?;
        Ok(to_serde_value(result, &mut self.ctx)?)
    }

    /// Calls the result function and returns the result.
    pub fn result<TX, DB>(
        &mut self,
        res: ResultAndState<impl HaltReasonTr>,
        tx: &TX,
        block: &impl Block,
        db: &DB,
    ) -> Result<JsValue, JsInspectorError>
    where
        TX: Transaction,
        DB: DatabaseRef,
        <DB as DatabaseRef>::Error: core::fmt::Display,
    {
        let ResultAndState { result, state } = res;
        let (db, _db_guard) = EvmDbRef::new(&state, db);

        let gas_used = result.gas_used();
        let mut to = None;
        let mut output_bytes = None;
        let mut error = None;
        match result {
            ExecutionResult::Success { output, .. } => match output {
                Output::Call(out) => {
                    output_bytes = Some(out);
                }
                Output::Create(out, addr) => {
                    to = addr;
                    output_bytes = Some(out);
                }
            },
            ExecutionResult::Revert { output, .. } => {
                error = Some("execution reverted".to_string());
                output_bytes = Some(output);
            }
            ExecutionResult::Halt { reason, .. } => {
                error = Some(format!("execution halted: {reason:?}"));
            }
        };

        if let TransactTo::Call(target) = tx.kind() {
            to = Some(target);
        }

        let ctx = JsEvmContext {
            r#type: match tx.kind() {
                TransactTo::Call(_) => "CALL",
                TransactTo::Create => "CREATE",
            }
            .to_string(),
            from: tx.caller(),
            to,
            input: tx.input().clone(),
            gas: tx.gas_limit(),
            gas_used,
            gas_price: tx
                .effective_gas_price(block.basefee() as u128)
                .try_into()
                .unwrap_or(u64::MAX),
            value: tx.value(),
            block: block.number().try_into().unwrap_or(u64::MAX),
            coinbase: block.beneficiary(),
            output: output_bytes.unwrap_or_default(),
            time: block.timestamp().to_string(),
            intrinsic_gas: 0,
            transaction_ctx: self.transaction_context,
            error,
        };
        let ctx = ctx.into_js_object(&mut self.ctx)?;
        let db = db.into_js_object(&mut self.ctx)?;
        Ok(self.result_fn.call(
            &(self.obj.clone().into()),
            &[ctx.into(), db.into()],
            &mut self.ctx,
        )?)
    }

    fn try_fault(&mut self, step: StepLog, db: EvmDbRef) -> JsResult<()> {
        let step = step.into_js_object(&mut self.ctx)?;
        let db = db.into_js_object(&mut self.ctx)?;
        self.fault_fn.call(&(self.obj.clone().into()), &[step.into(), db.into()], &mut self.ctx)?;
        Ok(())
    }

    fn try_step(&mut self, step: StepLog, db: EvmDbRef) -> JsResult<()> {
        if let Some(step_fn) = &self.step_fn {
            let step = step.into_js_object(&mut self.ctx)?;
            let db = db.into_js_object(&mut self.ctx)?;
            step_fn.call(&(self.obj.clone().into()), &[step.into(), db.into()], &mut self.ctx)?;
        }
        Ok(())
    }

    fn try_enter(&mut self, frame: CallFrame) -> JsResult<()> {
        if let Some(enter_fn) = &self.enter_fn {
            let frame = frame.into_js_object(&mut self.ctx)?;
            enter_fn.call(&(self.obj.clone().into()), &[frame.into()], &mut self.ctx)?;
        }
        Ok(())
    }

    fn try_exit(&mut self, frame: FrameResult) -> JsResult<()> {
        if let Some(exit_fn) = &self.exit_fn {
            let frame = frame.into_js_object(&mut self.ctx)?;
            exit_fn.call(&(self.obj.clone().into()), &[frame.into()], &mut self.ctx)?;
        }
        Ok(())
    }

    /// Returns the currently active call
    ///
    /// Panics: if there's no call yet
    #[track_caller]
    fn active_call(&self) -> &CallStackItem {
        self.call_stack.last().expect("call stack is empty")
    }

    #[inline]
    fn pop_call(&mut self) {
        self.call_stack.pop();
    }

    /// Returns true whether the active call is the root call.
    #[inline]
    fn is_root_call_active(&self) -> bool {
        self.call_stack.len() == 1
    }

    /// Returns true if there's an enter function and the active call is not the root call.
    #[inline]
    fn can_call_enter(&self) -> bool {
        self.enter_fn.is_some() && !self.is_root_call_active()
    }

    /// Returns true if there's an exit function and the active call is not the root call.
    #[inline]
    fn can_call_exit(&mut self) -> bool {
        self.exit_fn.is_some() && !self.is_root_call_active()
    }

    /// Pushes a new call to the stack
    fn push_call(
        &mut self,
        contract: Address,
        input: Bytes,
        value: U256,
        kind: CallKind,
        caller: Address,
        gas_limit: u64,
    ) -> &CallStackItem {
        let call = CallStackItem {
            contract: Contract { caller, contract, value, input },
            kind,
            gas_limit,
        };
        self.call_stack.push(call);
        self.active_call()
    }

    /// Registers the precompiles in the JS context
    fn register_precompiles<CTX: ContextTr<Journal: JournalExt>>(&mut self, context: &mut CTX) {
        if !self.precompiles_registered {
            return;
        }
        let precompiles = PrecompileList(context.journal().precompile_addresses().clone());

        let _ = precompiles.register_callable(&mut self.ctx);

        self.precompiles_registered = true
    }
}

impl<CTX> Inspector<CTX> for JsInspector
where
    CTX: ContextTr<Journal: JournalExt, Db: DatabaseRef>,
{
    fn step(&mut self, interp: &mut Interpreter, context: &mut CTX) {
        // if this is a revert we need to manually record this so that we can use it in the
        // step_end fn
        self.last_start_step_pc = Some(interp.bytecode.pc());

        if self.step_fn.is_none() {
            return;
        }

        let (db, _db_guard) = EvmDbRef::new(context.journal_ref().evm_state(), context.db_ref());

        let (stack, _stack_guard) = StackRef::new(&interp.stack);
        let evm_memory = interp.memory.borrow();
        let (memory, _memory_guard) = MemoryRef::new(evm_memory);
        let active_call = self.active_call();
        let step = StepLog {
            stack,
            op: interp.bytecode.opcode().into(),
            memory,
            pc: interp.bytecode.pc() as u64,
            gas_remaining: interp.gas.remaining(),
            cost: interp.gas.spent(),
            depth: context.journal_ref().depth() as u64,
            refund: interp.gas.refunded() as u64,
            error: None,
            contract: Contract {
                caller: interp.input.caller_address,
                contract: interp.input.target_address,
                value: active_call.contract.value,
                input: active_call.contract.input.clone(),
            },
        };

        if self.try_step(step, db).is_err() {
            interp
                .bytecode
                .set_action(InterpreterAction::new_halt(InstructionResult::Revert, interp.gas));
        }
    }

    fn step_end(&mut self, interp: &mut Interpreter, context: &mut CTX) {
        if self.step_fn.is_none() {
            return;
        }

        if interp
            .bytecode
            .action()
            .as_ref()
            .is_some_and(|a| a.instruction_result().map(|r| r.is_revert()).unwrap_or(false))
        {
            let (db, _db_guard) =
                EvmDbRef::new(context.journal_ref().evm_state(), context.db_ref());

            let (stack, _stack_guard) = StackRef::new(&interp.stack);
            let mem = interp.memory.borrow();
            let (memory, _memory_guard) = MemoryRef::new(mem);
            let active_call = self.active_call();
            let step = StepLog {
                stack,
                // we can use REVERT opcode here because we checked that this was a revert
                op: OpCode::REVERT.get().into(),
                // Use the recorded pc of the current step for the revert here
                pc: self.last_start_step_pc.unwrap_or_default() as u64,
                memory,
                gas_remaining: interp.gas.remaining(),
                cost: interp.gas.spent(),
                depth: context.journal_ref().depth() as u64,
                refund: interp.gas.refunded() as u64,
                error: interp
                    .bytecode
                    .action()
                    .as_ref()
                    .and_then(|i| i.instruction_result().map(|i| format!("{i:?}"))),
                contract: Contract {
                    caller: interp.input.caller_address,
                    contract: interp.input.target_address,
                    value: active_call.contract.value,
                    input: active_call.contract.input.clone(),
                },
            };

            let _ = self.try_fault(step, db);
        }
    }

    fn log(&mut self, _interp: &mut Interpreter, _context: &mut CTX, _log: Log) {}

    fn call(&mut self, context: &mut CTX, inputs: &mut CallInputs) -> Option<CallOutcome> {
        self.register_precompiles(context);

        // determine contract and caller based on the call scheme
        let (caller, contract) = match inputs.scheme {
            CallScheme::DelegateCall | CallScheme::CallCode => {
                (inputs.target_address, inputs.bytecode_address)
            }
            _ => (inputs.caller, inputs.target_address),
        };

        let value = inputs.transfer_value().unwrap_or_default();
        self.push_call(
            contract,
            inputs.input_data(context),
            value,
            inputs.scheme.into(),
            caller,
            inputs.gas_limit,
        );

        if self.can_call_enter() {
            let call = self.active_call();
            let frame = CallFrame {
                contract: call.contract.clone(),
                kind: call.kind,
                gas: inputs.gas_limit,
            };
            if let Err(err) = self.try_enter(frame) {
                return Some(CallOutcome::new(
                    js_error_to_revert(err),
                    inputs.return_memory_offset.clone(),
                ));
            }
        }

        None
    }

    fn call_end(&mut self, _context: &mut CTX, _inputs: &CallInputs, outcome: &mut CallOutcome) {
        if self.can_call_exit() {
            let frame_result = FrameResult {
                gas_used: outcome.result.gas.spent(),
                output: outcome.result.output.clone(),
                error: utils::fmt_error_msg(outcome.result.result, TraceStyle::Geth),
            };
            if let Err(err) = self.try_exit(frame_result) {
                outcome.result = js_error_to_revert(err);
            }
        }

        self.pop_call();
    }

    fn create(&mut self, context: &mut CTX, inputs: &mut CreateInputs) -> Option<CreateOutcome> {
        self.register_precompiles(context);

        let nonce = context.journal_mut().load_account(inputs.caller).unwrap().info.nonce;
        let contract = inputs.created_address(nonce);
        self.push_call(
            contract,
            inputs.init_code.clone(),
            inputs.value,
            inputs.scheme.into(),
            inputs.caller,
            inputs.gas_limit,
        );

        if self.can_call_enter() {
            let call = self.active_call();
            let frame =
                CallFrame { contract: call.contract.clone(), kind: call.kind, gas: call.gas_limit };
            if let Err(err) = self.try_enter(frame) {
                return Some(CreateOutcome::new(js_error_to_revert(err), None));
            }
        }

        None
    }

    fn create_end(
        &mut self,
        _context: &mut CTX,
        _inputs: &CreateInputs,
        outcome: &mut CreateOutcome,
    ) {
        if self.can_call_exit() {
            let frame_result = FrameResult {
                gas_used: outcome.result.gas.spent(),
                output: outcome.result.output.clone(),
                error: None,
            };
            if let Err(err) = self.try_exit(frame_result) {
                outcome.result = js_error_to_revert(err);
            }
        }

        self.pop_call();
    }

    fn selfdestruct(&mut self, _contract: Address, _target: Address, _value: U256) {
        // This is exempt from the root call constraint, because selfdestruct is treated as a
        // new scope that is entered and immediately exited.
        if self.enter_fn.is_some() {
            let call = self.active_call();
            let frame =
                CallFrame { contract: call.contract.clone(), kind: call.kind, gas: call.gas_limit };
            let _ = self.try_enter(frame);
        }

        // exit with empty frame result ref <https://github.com/ethereum/go-ethereum/blob/0004c6b229b787281760b14fb9460ffd9c2496f1/core/vm/instructions.go#L829-L829>
        if self.exit_fn.is_some() {
            let frame_result = FrameResult { gas_used: 0, output: Bytes::new(), error: None };
            let _ = self.try_exit(frame_result);
        }
    }
}

/// Represents an active call
#[derive(Debug)]
struct CallStackItem {
    contract: Contract,
    kind: CallKind,
    gas_limit: u64,
}

/// Error variants that can occur during JavaScript inspection.
#[derive(Debug, thiserror::Error)]
pub enum JsInspectorError {
    /// Error originating from a JavaScript operation.
    #[error(transparent)]
    JsError(#[from] JsError),

    /// Failure during the evaluation of JavaScript code.
    #[error("failed to evaluate JS code: {0}")]
    EvalCode(JsError),

    /// The evaluated code is not a JavaScript object.
    #[error("the evaluated code is not a JS object")]
    ExpectedJsObject,

    /// The trace object must expose a function named `result()`.
    #[error("trace object must expose a function result()")]
    ResultFunctionMissing,

    /// The trace object must expose a function named `fault()`.
    #[error("trace object must expose a function fault()")]
    FaultFunctionMissing,

    /// The setup object must be a callable function.
    #[error("setup object must be a function")]
    SetupFunctionNotCallable,

    /// Failure during the invocation of the `setup()` function.
    #[error("failed to call setup(): {0}")]
    SetupCallFailed(JsError),

    /// Invalid JSON configuration encountered.
    #[error("invalid JSON config: {0}")]
    InvalidJsonConfig(JsError),
}

/// Converts a JavaScript error into a [InstructionResult::Revert] [InterpreterResult].
#[inline]
fn js_error_to_revert(err: JsError) -> InterpreterResult {
    let output = err.to_string().as_bytes().to_vec();
    InterpreterResult { result: InstructionResult::Revert, output: output.into(), gas: Gas::new(0) }
}

#[cfg(test)]
mod tests {
    use super::*;

    use alloy_primitives::{hex, Address};
    use revm::{
        context::TxEnv,
        database::CacheDB,
        database_interface::EmptyDB,
        inspector::InspectorEvmTr,
        primitives::hardfork::SpecId,
        state::{AccountInfo, Bytecode},
        InspectEvm, MainBuilder, MainContext,
    };
    //use revm_inspector::{inspector_handler, InspectorContext, InspectorMainEvm};
    use serde_json::json;

    #[test]
    fn test_loop_iteration_limit() {
        let mut context = Context::default();
        context.runtime_limits_mut().set_loop_iteration_limit(LOOP_ITERATION_LIMIT);

        let code = "let i = 0; while (i++ < 69) {}";
        let result = context.eval(Source::from_bytes(code));
        assert!(result.is_ok());

        let code = "while (true) {}";
        let result = context.eval(Source::from_bytes(code));
        assert!(result.is_err());
    }

    #[test]
    fn test_fault_fn_not_callable() {
        let code = r#"
            {
                result: function() {},
                fault: {},
            }
        "#;
        let config = serde_json::Value::Null;
        let result = JsInspector::new(code.to_string(), config);
        assert!(matches!(result, Err(JsInspectorError::FaultFunctionMissing)));
    }

    // Helper function to run a trace and return the result
    fn run_trace(code: &str, contract: Option<Bytes>, success: bool) -> serde_json::Value {
        let addr = Address::repeat_byte(0x01);
        let mut db = CacheDB::new(EmptyDB::default());

        // Insert the caller
        db.insert_account_info(
            Address::ZERO,
            AccountInfo { balance: U256::from(1e18), ..Default::default() },
        );
        // Insert the contract
        db.insert_account_info(
            addr,
            AccountInfo {
                code: Some(Bytecode::new_legacy(
                    /* PUSH1 1, PUSH1 1, STOP */
                    contract.unwrap_or_else(|| hex!("6001600100").into()),
                )),
                ..Default::default()
            },
        );

        let insp = JsInspector::new(code.to_string(), serde_json::Value::Null).unwrap();

        let mut evm = revm::Context::mainnet()
            .modify_cfg_chained(|cfg| cfg.spec = SpecId::CANCUN)
            .with_db(db)
            .build_mainnet_with_inspector(insp);

        let res = evm
            .inspect_tx(TxEnv {
                gas_price: 1024,
                gas_limit: 1_000_000,
                gas_priority_fee: None,
                kind: TransactTo::Call(addr),
                ..Default::default()
            })
            .expect("pass without error");

        assert_eq!(res.result.is_success(), success);
        let (ctx, inspector) = evm.ctx_inspector();
        inspector.json_result(res, ctx.tx(), ctx.block(), ctx.db_ref()).unwrap()
    }

    #[test]
    fn test_general_counting() {
        let code = r#"{
            count: 0,
            step: function() { this.count += 1; },
            fault: function() {},
            result: function() { return this.count; }
        }"#;
        let res = run_trace(code, None, true);
        assert_eq!(res.as_u64().unwrap(), 3);
    }

    #[test]
    fn test_memory_access() {
        let code = r#"{
            depths: [],
            step: function(log) { this.depths.push(log.memory.slice(-1,-2)); },
            fault: function() {},
            result: function() { return this.depths; }
        }"#;
        let res = run_trace(code, None, false);
        assert_eq!(res.as_array().unwrap().len(), 0);
    }

    #[test]
    fn test_stack_peek() {
        let code = r#"{
            depths: [],
            step: function(log) { this.depths.push(log.stack.peek(-1)); },
            fault: function() {},
            result: function() { return this.depths; }
        }"#;
        let res = run_trace(code, None, false);
        assert_eq!(res.as_array().unwrap().len(), 0);
    }

    #[test]
    fn test_memory_get_uint() {
        let code = r#"{
            depths: [],
            step: function(log, db) { this.depths.push(log.memory.getUint(-64)); },
            fault: function() {},
            result: function() { return this.depths; }
        }"#;
        let res = run_trace(code, None, false);
        assert_eq!(res.as_array().unwrap().len(), 0);
    }

    #[test]
    fn test_stack_depth() {
        let code = r#"{
            depths: [],
            step: function(log) { this.depths.push(log.stack.length()); },
            fault: function() {},
            result: function() { return this.depths; }
        }"#;
        let res = run_trace(code, None, true);
        assert_eq!(res, json!([0, 1, 2]));
    }

    #[test]
    fn test_memory_length() {
        let code = r#"{
            lengths: [],
            step: function(log) { this.lengths.push(log.memory.length()); },
            fault: function() {},
            result: function() { return this.lengths; }
        }"#;
        let res = run_trace(code, None, true);
        assert_eq!(res, json!([0, 0, 0]));
    }

    #[test]
    fn test_opcode_to_string() {
        let code = r#"{
             opcodes: [],
             step: function(log) { this.opcodes.push(log.op.toString()); },
             fault: function() {},
             result: function() { return this.opcodes; }
         }"#;
        let res = run_trace(code, None, true);
        assert_eq!(res, json!(["PUSH1", "PUSH1", "STOP"]));
    }

    #[test]
    fn test_gas_used() {
        let code = r#"{
            depths: [],
            step: function() {},
            fault: function() {},
            result: function(ctx) { return ctx.gasPrice+'.'+ctx.gasUsed; }
        }"#;
        let res = run_trace(code, None, true);
        assert_eq!(res.as_str().unwrap(), "1024.21006");
    }

    #[test]
    fn test_to_word() {
        let code = r#"{
            res: null,
            step: function(log) {},
            fault: function() {},
            result: function() { return toWord('0xffaa') }
        }"#;
        let res = run_trace(code, None, true);
        assert_eq!(
            res,
            json!({
                "0": 0, "1": 0, "2": 0, "3": 0, "4": 0, "5": 0, "6": 0, "7": 0, "8": 0,
                "9": 0, "10": 0, "11": 0, "12": 0, "13": 0, "14": 0, "15": 0, "16": 0,
                "17": 0, "18": 0, "19": 0, "20": 0, "21": 0, "22": 0, "23": 0, "24": 0,
                "25": 0, "26": 0, "27": 0, "28": 0, "29": 0, "30": 255, "31": 170,
            })
        );
    }

    #[test]
    fn test_to_address() {
        let code = r#"{
            res: null,
            step: function(log) { var address = log.contract.getAddress(); this.res = toAddress(address); },
            fault: function() {},
            result: function() { return toHex(this.res) }
        }"#;
        let res = run_trace(code, None, true);
        assert_eq!(res.as_str().unwrap(), "0x0101010101010101010101010101010101010101");
    }

    #[test]
    fn test_to_address_string() {
        let code = r#"{
            res: null,
            step: function(log) { var address = '0x0000000000000000000000000000000000000000'; this.res = toAddress(address); },
            fault: function() {},
            result: function() { return this.res }
        }"#;
        let res = run_trace(code, None, true);
        assert_eq!(res.as_object().unwrap().values().map(|v| v.as_u64().unwrap()).sum::<u64>(), 0);
    }

    #[test]
    fn test_memory_slice() {
        let code = r#"{
            res: [],
            step: function(log) {
                var op = log.op.toString();
                if (op === 'MSTORE8' || op === 'STOP') {
                    this.res.push(log.memory.slice(0, 2))
                }
            },
            fault: function() {},
            result: function() { return this.res }
        }"#;
        let contract = hex!("60ff60005300"); // PUSH1, 0xff, PUSH1, 0x00, MSTORE8, STOP
        let res = run_trace(code, Some(contract.into()), false);
        assert_eq!(res, json!([]));
    }

    #[test]
    fn test_memory_limit() {
        let code = r#"{
            res: [],
            step: function(log) { if (log.op.toString() === 'STOP') { this.res.push(log.memory.slice(5, 1025 * 1024)) } },
            fault: function() {},
            result: function() { return this.res }
        }"#;
        let res = run_trace(code, None, false);
        assert_eq!(res, json!([]));
    }

    #[test]
    fn test_coinbase() {
        let code = r#"{
            lengths: [],
            step: function(log) { },
            fault: function() {},
            result: function(ctx) { var coinbase = ctx.coinbase; return toAddress(coinbase); }
        }"#;
        let res = run_trace(code, None, true);
        assert_eq!(res.as_object().unwrap().values().map(|v| v.as_u64().unwrap()).sum::<u64>(), 0);
    }
}
