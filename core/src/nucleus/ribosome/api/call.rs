use crate::{
    action::{Action, ActionWrapper},
    context::Context,
    instance::RECV_DEFAULT_TIMEOUT_MS,
    nucleus::{
        get_capability_with_zome_call, launch_zome_fn_call,
        ribosome::{api::ZomeApiResult, Runtime},
        state::NucleusState,
        ZomeFnCall,
    },
};
use holochain_core_types::{dna::capabilities::Membrane, error::HolochainError};
use holochain_wasm_utils::api_serialization::ZomeFnCallArgs;
use std::{
    convert::TryFrom,
    sync::{mpsc::channel, Arc},
};
use wasmi::{RuntimeArgs, RuntimeValue};

// ZomeFnCallArgs to ZomeFnCall
impl ZomeFnCall {
    fn from_args(args: ZomeFnCallArgs) -> Self {
        ZomeFnCall::new(&args.zome_name, &args.cap_name, &args.fn_name, args.fn_args)
    }
}

/// HcApiFuncIndex::CALL function code
/// args: [0] encoded MemoryAllocation as u32
/// expected complex argument: {zome_name: String, cap_name: String, fn_name: String, args: String}
/// args from API call are converted into a ZomeFnCall
/// Launch an Action::Call with newly formed ZomeFnCall
/// Waits for a ZomeFnResult
/// Returns an HcApiReturnCode as I32
pub fn invoke_call(runtime: &mut Runtime, args: &RuntimeArgs) -> ZomeApiResult {
    // deserialize args
    let args_str = runtime.load_json_string_from_args(&args);

    let input = match ZomeFnCallArgs::try_from(args_str.clone()) {
        Ok(input) => input,
        // Exit on error
        Err(_) => {
            println!("invoke_call failed to deserialize: {:?}", args_str);
            return ribosome_error_code!(ArgumentDeserializationFailed);
        }
    };

    // ZomeFnCallArgs to ZomeFnCall
    let zome_call = ZomeFnCall::from_args(input);

    // Don't allow recursive calls
    if zome_call.same_fn_as(&runtime.zome_call) {
        return ribosome_error_code!(RecursiveCallForbidden);
    }

    // Create Call Action
    let action_wrapper = ActionWrapper::new(Action::Call(zome_call.clone()));
    // Send Action and block
    let (sender, receiver) = channel();
    crate::instance::dispatch_action_with_observer(
        runtime.context.action_channel(),
        runtime.context.observer_channel(),
        action_wrapper.clone(),
        move |state: &crate::state::State| {
            // Observer waits for a ribosome_call_result
            let maybe_result = state.nucleus().zome_call_result(&zome_call);
            match maybe_result {
                Some(result) => {
                    // @TODO never panic in wasm
                    // @see https://github.com/holochain/holochain-rust/issues/159
                    sender
                        .send(result)
                        // the channel stays connected until the first message has been sent
                        // if this fails that means that it was called after having returned done=true
                        .expect("observer called after done");

                    true
                }
                None => false,
            }
        },
    );
    // TODO #97 - Return error if timeout or something failed
    // return Err(_);

    let result = receiver
        .recv_timeout(RECV_DEFAULT_TIMEOUT_MS)
        .expect("observer dropped before done");
    runtime.store_result(result)
}

/// Reduce Call Action
///   1. Checks for correctness of ZomeFnCall inside the Action
///   2. Checks for permission to access Capability
///   3. Execute the exposed Zome function in a separate thread
/// Send the result in a ReturnZomeFunctionResult Action on success or failure like ExecuteZomeFunction
pub(crate) fn reduce_call(
    context: Arc<Context>,
    state: &mut NucleusState,
    action_wrapper: &ActionWrapper,
) {
    // 1.Checks for correctness of ZomeFnCall
    let fn_call = match action_wrapper.action().clone() {
        Action::Call(call) => call,
        _ => unreachable!(),
    };
    // Get Capability
    if state.dna.is_none() {
        // Notify failure
        state
            .zome_calls
            .insert(fn_call.clone(), Some(Err(HolochainError::DnaMissing)));
        return;
    }
    let dna = state.dna.clone().unwrap();
    let maybe_cap = get_capability_with_zome_call(&dna, &fn_call);
    if let Err(fn_res) = maybe_cap {
        // Notify failure
        state
            .zome_calls
            .insert(fn_call.clone(), Some(fn_res.result()));
        return;
    }
    let cap = maybe_cap.unwrap().clone();

    // 2. Checks for permission to access Capability
    // TODO #301 - Do real Capability token check
    let can_call = match cap.cap_type.membrane {
        Membrane::Public => true,
        Membrane::Zome => {
            // TODO #301 - check if caller zome_name is same as called zome_name
            false
        }
        Membrane::Agent => {
            // TODO #301 - check if caller has Agent Capability
            false
        }
        Membrane::ApiKey => {
            // TODO #301 - check if caller has ApiKey Capability
            false
        }
    };
    if !can_call {
        // Notify failure
        state.zome_calls.insert(
            fn_call.clone(),
            Some(Err(HolochainError::DoesNotHaveCapabilityToken)),
        );
        return;
    }

    // 3. Get the exposed Zome function WASM and execute it in a separate thread
    let maybe_code = dna.get_wasm_from_zome_name(fn_call.zome_name.clone());
    let code =
        maybe_code.expect("zome not found, Should have failed before when getting capability.");
    state.zome_calls.insert(fn_call.clone(), None);
    launch_zome_fn_call(context, fn_call, &code, state.dna.clone().unwrap().name);
}

#[cfg(test)]
pub mod tests {
    extern crate tempfile;
    extern crate test_utils;
    extern crate wabt;

    use self::tempfile::tempdir;
    use crate::{
        context::{mock_network_config, Context},
        instance::{
            tests::{test_instance, TestLogger},
            Observer, RECV_DEFAULT_TIMEOUT_MS,
        },
        nucleus::ribosome::{
            api::{
                call::{Action, ActionWrapper, Membrane, ZomeFnCall},
                tests::{
                    test_capability, test_function_name, test_parameters,
                    test_zome_api_function_wasm, test_zome_name,
                },
                ZomeApiFunction,
            },
            Defn,
        },
        persister::SimplePersister,
    };
    use holochain_cas_implementations::{cas::file::FilesystemStorage, eav::file::EavFileStorage};
    use holochain_core_types::{
        agent::AgentId,
        dna::{capabilities::Capability, Dna},
        error::{DnaError, HolochainError},
        json::JsonString,
    };
    use holochain_wasm_utils::api_serialization::ZomeFnCallArgs;
    use serde_json;
    use std::sync::{
        mpsc::{channel, RecvTimeoutError},
        Arc, Mutex, RwLock,
    };
    use test_utils::create_test_dna_with_cap;

    /// dummy commit args from standard test entry
    #[cfg_attr(tarpaulin, skip)]
    pub fn test_bad_args_bytes() -> Vec<u8> {
        let args = ZomeFnCallArgs {
            zome_name: "zome_name".to_string(),
            cap_name: "cap_name".to_string(),
            fn_name: "fn_name".to_string(),
            fn_args: "fn_args".to_string(),
        };
        serde_json::to_string(&args)
            .expect("args should serialize")
            .into_bytes()
    }

    #[cfg_attr(tarpaulin, skip)]
    pub fn test_args_bytes() -> Vec<u8> {
        let args = ZomeFnCallArgs {
            zome_name: test_zome_name(),
            cap_name: test_capability(),
            fn_name: test_function_name(),
            fn_args: test_parameters(),
        };
        serde_json::to_string(&args)
            .expect("args should serialize")
            .into_bytes()
    }

    #[cfg_attr(tarpaulin, skip)]
    fn create_context() -> Arc<Context> {
        let file_storage = Arc::new(RwLock::new(
            FilesystemStorage::new(tempdir().unwrap().path().to_str().unwrap()).unwrap(),
        ));
        Arc::new(
            Context::new(
                AgentId::generate_fake("alex"),
                Arc::new(Mutex::new(TestLogger { log: Vec::new() })),

                Arc::new(Mutex::new(SimplePersister::new(file_storage.clone()))),
                file_storage.clone(),
                file_storage.clone(),
                Arc::new(RwLock::new(
                    EavFileStorage::new(tempdir().unwrap().path().to_str().unwrap().to_string())
                        .unwrap(),
                )),
                mock_network_config(),
            ),
        )
    }

    #[cfg_attr(tarpaulin, skip)]
    fn test_reduce_call(
        dna: Dna,
        expected: Result<Result<JsonString, HolochainError>, RecvTimeoutError>,
    ) {
        let context = create_context();

        let zome_call = ZomeFnCall::new("test_zome", "test_cap", "test", "{}");
        let zome_call_action = ActionWrapper::new(Action::Call(zome_call.clone()));

        // Set up instance and process the action
        // let instance = Instance::new();
        let instance = test_instance(dna).expect("Could not initialize test instance");
        let (sender, receiver) = channel();
        let closure = move |state: &crate::state::State| {
            // Observer waits for a ribosome_call_result
            let opt_res = state.nucleus().zome_call_result(&zome_call);
            match opt_res {
                Some(res) => {
                    // @TODO never panic in wasm
                    // @see https://github.com/holochain/holochain-rust/issues/159
                    sender
                        .send(res)
                        // the channel stays connected until the first message has been sent
                        // if this fails that means that it was called after having returned done=true
                        .expect("observer called after done");

                    true
                }
                None => false,
            }
        };

        let observer = Observer {
            sensor: Box::new(closure),
        };

        let mut state_observers: Vec<Observer> = Vec::new();
        state_observers.push(observer);
        let (_, rx_observer) = channel::<Observer>();
        instance.process_action(zome_call_action, state_observers, &rx_observer, &context);

        let action_result = receiver.recv_timeout(RECV_DEFAULT_TIMEOUT_MS);

        assert_eq!(expected, action_result);
    }

    #[test]
    fn test_call_no_token() {
        let dna = test_utils::create_test_dna_with_wat("test_zome", "test_cap", None);
        let expected = Ok(Err(HolochainError::DoesNotHaveCapabilityToken));
        test_reduce_call(dna, expected);
    }

    #[test]
    fn test_call_no_zome() {
        let dna = test_utils::create_test_dna_with_wat("bad_zome", "test_cap", None);
        let expected = Ok(Err(HolochainError::Dna(DnaError::ZomeNotFound(
            r#"Zome 'test_zome' not found"#.to_string(),
        ))));
        test_reduce_call(dna, expected);
    }

    #[test]
    fn test_call_ok() {
        let wasm = test_zome_api_function_wasm(ZomeApiFunction::Call.as_str());
        let mut capability = Capability::new();
        capability.cap_type.membrane = Membrane::Public;
        let dna = create_test_dna_with_cap(&test_zome_name(), "test_cap", &capability, &wasm);

        // Expecting timeout since there is no function in wasm to call
        let expected = Err(RecvTimeoutError::Disconnected);
        test_reduce_call(dna, expected);
    }
}
