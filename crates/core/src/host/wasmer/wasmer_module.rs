use super::wasm_instance_env::WasmInstanceEnv;
use super::Mem;
use crate::host::instance_env::InstanceEnv;
use crate::host::wasm_common::module_host_actor::{AbiVersionError, DescribeError, InitializationError};
use crate::host::wasm_common::*;
use crate::host::{EnergyQuanta, Timestamp};
use bytes::Bytes;
use spacetimedb_lib::{Address, Identity, VersionTuple};
use wasmer::{
    imports, AsStoreMut, Engine, ExternType, Function, FunctionEnv, Imports, Instance, Module, RuntimeError, Store,
    TypedFunction, WasmPtr,
};
use wasmer_middlewares::metering as wasmer_metering;

fn get_remaining_points(ctx: &mut impl AsStoreMut, instance: &Instance) -> u64 {
    let remaining_points = wasmer_metering::get_remaining_points(ctx, instance);
    match remaining_points {
        wasmer_metering::MeteringPoints::Remaining(x) => x,
        wasmer_metering::MeteringPoints::Exhausted => 0,
    }
}

fn log_traceback(func_type: &str, func: &str, e: &RuntimeError) {
    let frames = e.trace();
    let frames_len = frames.len();

    log::info!("{} \"{}\" runtime error: {}", func_type, func, e.message());
    for (i, frame) in frames.iter().enumerate().take(frames_len) {
        log::info!(
            "  Frame #{}: {:?}::{}",
            frames_len - i,
            frame.module_name(),
            rustc_demangle::demangle(frame.function_name().unwrap_or("<func>"))
        );
    }
}

#[derive(Clone)]
pub struct WasmerModule {
    module: Module,
    engine: Engine,
}

impl WasmerModule {
    pub fn new(module: Module, engine: Engine) -> Self {
        WasmerModule { module, engine }
    }

    pub const IMPLEMENTED_ABI: VersionTuple = VersionTuple::new(5, 0);

    fn imports(&self, store: &mut Store, env: &FunctionEnv<WasmInstanceEnv>) -> Imports {
        const _: () = assert!(WasmerModule::IMPLEMENTED_ABI.eq(spacetimedb_lib::MODULE_ABI_VERSION));
        imports! {
            "spacetime" => {
                "_schedule_reducer" => Function::new_typed_with_env(store, env, WasmInstanceEnv::schedule_reducer),
                "_cancel_reducer" => Function::new_typed_with_env(store, env, WasmInstanceEnv::cancel_reducer),
                "_delete_by_col_eq" => Function::new_typed_with_env(
                    store,
                    env,
                    WasmInstanceEnv::delete_by_col_eq,
                ),
                /*
                "_delete_pk" => Function::new_typed_with_env(
                    store,
                    env,
                    WasmInstanceEnv::delete_pk,
                ),
                "_delete_value" => Function::new_typed_with_env(
                    store,
                    env,
                    WasmInstanceEnv::delete_value,
                ),
                "_delete_range" => Function::new_typed_with_env(
                    store,
                    env,
                    WasmInstanceEnv::delete_range,
                ),
                */
                "_insert" => Function::new_typed_with_env(
                    store,
                    env,
                    WasmInstanceEnv::insert,
                ),
                /*
                "_create_table" => Function::new_typed_with_env(
                    store,
                    env,
                    WasmInstanceEnv::create_table,
                ),
                */
                "_get_table_id" => Function::new_typed_with_env(
                    store,
                    env,
                    WasmInstanceEnv::get_table_id,
                ),
                "_create_index" => Function::new_typed_with_env(
                    store,
                    env,
                    WasmInstanceEnv::create_index,
                ),
                "_iter_by_col_eq" => Function::new_typed_with_env(
                    store,
                    env,
                    WasmInstanceEnv::iter_by_col_eq,
                ),
                "_iter_start" => Function::new_typed_with_env(
                    store,
                    env,
                    WasmInstanceEnv::iter_start
                ),
                "_iter_start_filtered" => Function::new_typed_with_env(
                    store,
                    env,
                    WasmInstanceEnv::iter_start_filtered
                ),
                "_iter_next" => Function::new_typed_with_env(
                    store,
                    env,
                    WasmInstanceEnv::iter_next
                ),
                "_iter_drop" => Function::new_typed_with_env(
                    store,
                    env,
                    WasmInstanceEnv::iter_drop
                ),
                "_console_log" => Function::new_typed_with_env(
                    store,
                    env,
                    WasmInstanceEnv::console_log
                ),
                "_buffer_len" => Function::new_typed_with_env(store, env, WasmInstanceEnv::buffer_len),
                "_buffer_consume" => Function::new_typed_with_env(store, env, WasmInstanceEnv::buffer_consume),
                "_buffer_alloc" => Function::new_typed_with_env(store, env, WasmInstanceEnv::buffer_alloc),
                "_span_start" => Function::new_typed_with_env(store, env, WasmInstanceEnv::span_start),
                "_span_end" => Function::new_typed_with_env(store, env, WasmInstanceEnv::span_end),
            }
        }
    }
}

impl module_host_actor::WasmModule for WasmerModule {
    type Instance = WasmerInstance;
    type InstancePre = Self;

    type ExternType = ExternType;

    fn get_export(&self, s: &str) -> Option<Self::ExternType> {
        self.module
            .exports()
            .find(|exp| exp.name() == s)
            .map(|exp| exp.ty().clone())
    }

    fn for_each_export<E>(&self, mut f: impl FnMut(&str, &Self::ExternType) -> Result<(), E>) -> Result<(), E> {
        self.module.exports().try_for_each(|exp| f(exp.name(), exp.ty()))
    }

    fn instantiate_pre(&self) -> Result<Self::InstancePre, InitializationError> {
        Ok(self.clone())
    }
}

impl module_host_actor::WasmInstancePre for WasmerModule {
    type Instance = WasmerInstance;

    fn instantiate(&self, env: InstanceEnv, func_names: &FuncNames) -> Result<Self::Instance, InitializationError> {
        let mut store = Store::new(self.engine.clone());
        let env = WasmInstanceEnv {
            instance_env: env,
            mem: None,
            buffers: Default::default(),
            iters: Default::default(),
            timing_spans: Default::default(),
        };
        let env = FunctionEnv::new(&mut store, env);
        let imports = self.imports(&mut store, &env);
        let instance = Instance::new(&mut store, &self.module, &imports)
            .map_err(|err| InitializationError::Instantiation(err.into()))?;

        let mem = Mem::extract(&instance.exports).unwrap();

        // We could (and did in the past) parse the ABI version manually before the instantiation,
        // but it gets complicated in presence of wasm-opt optimisations which might split encoded
        // versions like `[...other data...]\00\00\03\00[...other data...]` by zeroes
        // into several segments, so there is no single data segment containing the entire version.
        // Instead, it's more reliable to extract the version from an instantiated module
        // when all the data segments are loaded into the flat memory at correct offsets.
        let abi_version = instance
            .exports
            .get_global(STDB_ABI_SYM)
            .map_err(|_| AbiVersionError::NoVersion)?;

        let mut abi_version = match abi_version.get(&mut store) {
            wasmer::Value::I32(x) => x as u32,
            _ => return Err(AbiVersionError::Malformed.into()),
        };

        let abi_is_addr = instance.exports.get_global(STDB_ABI_IS_ADDR_SYM).is_ok();
        if abi_is_addr {
            abi_version = u32::from_le_bytes(
                mem.read_bytes(&store, WasmPtr::new(abi_version), 4)
                    .ok()
                    .and_then(|bytes| bytes.try_into().ok())
                    .ok_or(AbiVersionError::Malformed)?,
            );
        }

        let abi_version = VersionTuple::from_u32(abi_version);

        if !WasmerModule::IMPLEMENTED_ABI.supports(abi_version) {
            return Err(AbiVersionError::UnsupportedVersion {
                implement: WasmerModule::IMPLEMENTED_ABI,
                got: abi_version,
            }
            .into());
        }

        env.as_mut(&mut store).mem = Some(mem);

        // Note: this budget is just for initializers
        let budget = EnergyQuanta::DEFAULT_BUDGET.as_points();
        wasmer_metering::set_remaining_points(&mut store, &instance, budget);

        for preinit in &func_names.preinits {
            let func = instance.exports.get_typed_function::<(), ()>(&store, preinit).unwrap();
            func.call(&mut store).map_err(|err| InitializationError::RuntimeError {
                err: err.into(),
                func: preinit.clone(),
            })?;
        }

        let init = instance.exports.get_typed_function::<(), u32>(&store, SETUP_DUNDER);
        if let Ok(init) = init {
            match init.call(&mut store).map(BufferIdx) {
                Ok(errbuf) if errbuf.is_invalid() => {}
                Ok(errbuf) => {
                    let errbuf = env
                        .as_mut(&mut store)
                        .buffers
                        .take(errbuf)
                        .unwrap_or_else(|| "unknown error".as_bytes().into());
                    let errbuf = crate::util::string_from_utf8_lossy_owned(errbuf.into()).into();
                    // TODO: catch this and return the error message to the http client
                    return Err(InitializationError::Setup(errbuf));
                }
                Err(err) => {
                    return Err(InitializationError::RuntimeError {
                        err: err.into(),
                        func: SETUP_DUNDER.to_owned(),
                    });
                }
            }
        }

        Ok(WasmerInstance { store, env, instance })
    }
}

pub struct WasmerInstance {
    store: Store,
    env: FunctionEnv<WasmInstanceEnv>,
    instance: Instance,
}

impl WasmerInstance {
    fn call_describer(&mut self, describer: &Function, describer_func_name: &str) -> Result<Bytes, DescribeError> {
        let start = std::time::Instant::now();
        log::trace!("Start describer \"{}\"...", describer_func_name);

        let store = &mut self.store;
        let describer = describer
            .typed::<(), u32>(store)
            .map_err(|_| DescribeError::Signature)?;
        let result = describer.call(store).map(BufferIdx);
        let duration = start.elapsed();
        log::trace!("Describer \"{}\" ran: {} us", describer_func_name, duration.as_micros(),);
        let buf = result.map_err(|err| {
            log_traceback("describer", describer_func_name, &err);
            DescribeError::RuntimeError(err.into())
        })?;
        let bytes = self
            .env
            .as_mut(store)
            .buffers
            .take(buf)
            .ok_or(DescribeError::BadBuffer)?;
        self.env.as_mut(store).buffers.clear();
        Ok(bytes)
    }
}

impl module_host_actor::WasmInstance for WasmerInstance {
    fn extract_descriptions(&mut self) -> Result<Bytes, DescribeError> {
        let describer = self.instance.exports.get_function(DESCRIBE_MODULE_DUNDER).unwrap();
        let describer = describer.clone();
        self.call_describer(&describer, DESCRIBE_MODULE_DUNDER)
    }

    fn instance_env(&self) -> &InstanceEnv {
        &self.env.as_ref(&self.store).instance_env
    }

    type Trap = wasmer::RuntimeError;

    fn call_reducer(
        &mut self,
        reducer_id: usize,
        budget: EnergyQuanta,
        sender_identity: &Identity,
        sender_address: &Address,
        timestamp: Timestamp,
        arg_bytes: Bytes,
    ) -> module_host_actor::ExecuteResult<Self::Trap> {
        self.call_tx_function::<(u32, u32, u32, u64, u32), 3>(
            CALL_REDUCER_DUNDER,
            budget,
            [
                Bytes::copy_from_slice(sender_identity.as_bytes()),
                Bytes::copy_from_slice(sender_address.as_slice()),
                arg_bytes,
            ],
            |func, store, [sender_identity, sender_address, args]| {
                func.call(
                    store,
                    reducer_id as u32,
                    sender_identity.0,
                    sender_address.0,
                    timestamp.0,
                    args.0,
                )
            },
        )
    }

    fn log_traceback(func_type: &str, func: &str, trap: &Self::Trap) {
        log_traceback(func_type, func, trap)
    }
}

impl WasmerInstance {
    fn call_tx_function<Args: wasmer::WasmTypeList, const N_BUFS: usize>(
        &mut self,
        reducer_symbol: &str,
        budget: EnergyQuanta,
        bufs: [Bytes; N_BUFS],
        // would be nicer if there was a TypedFunction::call_tuple(&self, store, ArgsTuple)
        call: impl FnOnce(TypedFunction<Args, u32>, &mut Store, [BufferIdx; N_BUFS]) -> Result<u32, RuntimeError>,
    ) -> module_host_actor::ExecuteResult<RuntimeError> {
        let store = &mut self.store;
        let instance = &self.instance;
        let budget = budget.as_points();
        wasmer_metering::set_remaining_points(store, instance, budget);

        let reduce = instance
            .exports
            .get_typed_function::<Args, u32>(store, reducer_symbol)
            .expect("invalid reducer");

        let bufs = bufs.map(|data| self.env.as_mut(store).buffers.insert(data));

        // let guard = pprof::ProfilerGuardBuilder::default().frequency(2500).build().unwrap();

        let start = std::time::Instant::now();
        log::trace!("Start reducer \"{}\"...", reducer_symbol);
        // pass ownership of the `ptr` allocation into the reducer
        let result = call(reduce, store, bufs).and_then(|errbuf| {
            let errbuf = BufferIdx(errbuf);
            Ok(if errbuf.is_invalid() {
                Ok(())
            } else {
                let errmsg = self
                    .env
                    .as_mut(store)
                    .buffers
                    .take(errbuf)
                    .ok_or_else(|| RuntimeError::new("invalid buffer handle"))?;
                Err(crate::util::string_from_utf8_lossy_owned(errmsg.into()).into())
            })
        });
        self.env.as_mut(store).buffers.clear();
        // .call(store, sender_buf.ptr.cast(), timestamp, args_buf.ptr, args_buf.len)
        // .and_then(|_| {});
        let duration = start.elapsed();
        let remaining = get_remaining_points(store, instance);
        let energy = module_host_actor::EnergyStats {
            used: EnergyQuanta::from_points(budget) - EnergyQuanta::from_points(remaining),
            remaining: EnergyQuanta::from_points(remaining),
        };
        module_host_actor::ExecuteResult {
            energy,
            execution_duration: duration,
            call_result: result,
        }
    }
}
