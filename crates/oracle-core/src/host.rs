//! Building the host environment a module runs in.
//!
//! The engine configuration, the linker carrying every host implementation, and
//! the stubs standing in for whatever is left. A spawned thread rebuilds all of
//! it, so this has to be reusable rather than inlined into startup.

use anyhow::{Context, Result, anyhow};
use wasmtime::{
    Caller, Config, Engine, Extern, ExternType, Func, FuncType, Global, Linker, Memory, Module,
    Ref, SharedMemory, Store, Table, Val, ValType,
};

use crate::state::{HostState, sync_memory};

/// How much a module may execute before being cut off, so a runaway startup
/// cannot hang a test run.
///
/// Generous on purpose: a guest worker spinning on a queue burns fuel while
/// waiting, and the whole budget can go to waiting rather than to work when the
/// machine is loaded. Too small shows up as a call that "failed" for no visible
/// reason.
pub const DEFAULT_FUEL: u64 = 20_000_000_000;

/// Defines every host implementation, then stubs whatever is left.
///
/// Order matters: `define_stubs` skips anything already defined, so the real
/// implementations have to come first.
pub(crate) fn define_hosts(
    store: &mut Store<HostState>,
    linker: &mut Linker<HostState>,
    module: &Module,
    unstubbable: &mut Vec<String>,
) -> Result<()> {
    crate::embind::define(store, linker, module)?;
    crate::emval::define(store, linker, module)?;
    crate::emscripten::define(store, linker)?;
    crate::emscripten::define_time(store, linker, module)?;
    crate::cxa::define(store, linker, module)?;
    crate::wasi::define(store, linker, module)?;
    crate::emscripten::define_invokes(store, linker, module)?;
    define_stubs(store, linker, module, unstubbable)?;
    Ok(())
}

/// Builds a linker carrying the same host environment as the main instance.
///
/// A spawned thread instantiates the module again and must see exactly the same
/// imports; anything missing here would show up as a link error at thread start
/// rather than as a difference in behaviour.
pub fn build_linker(store: &mut Store<HostState>, module: &Module) -> Result<Linker<HostState>> {
    let mut linker = Linker::new(store.engine());
    linker.allow_shadowing(true);
    let mut unstubbable = Vec::new();
    define_hosts(store, &mut linker, module, &mut unstubbable)?;
    Ok(linker)
}

/// Builds a host function that refreshes the memory window before running.
///
/// Every host function must do this, and adding the call by hand at each
/// definition site is how it gets forgotten: WASI was defined without it and
/// silently read a stale window for as long as the guest did not grow memory —
/// which the media modules do during startup, so their arguments landed outside
/// the window the host believed in and were dropped.
pub fn host_func<F>(store: &mut Store<HostState>, ty: FuncType, handler: F) -> Func
where
    F: Fn(&mut Caller<'_, HostState>, &[Val], &mut [Val]) -> Result<(), wasmtime::Error>
        + Send
        + Sync
        + 'static,
{
    Func::new(store, ty, move |mut caller, params, results| {
        // Every host call is both a cancellation point and a yield point. A
        // guest worker loop reaches one constantly — it polls the clock — which
        // is what makes them the right place for each.
        let thread = caller.data().thread_id;
        if thread != 0 && caller.data().shared.is_shutting_down() {
            return Err(anyhow!("host is shutting down"));
        }

        // Keep a spawned thread fuelled while the host still wants it.
        //
        // A worker was given a fixed budget on the assumption that its loop
        // waits for work that never arrives. That is no longer true: the VoIP
        // engine's worker polls constantly and *does* get work, and the budget
        // ran out between bringing the stack up and placing a call — leaving
        // zero live threads at exactly the moment the engine needed one. What
        // bounds a thread is the shutdown check above, not an instruction
        // count, so the budget is topped up rather than spent down.
        if thread != 0 {
            const LOW: u64 = 100_000_000;
            const TOP_UP: u64 = 1_000_000_000;

            if caller.get_fuel().is_ok_and(|fuel| fuel < LOW) {
                let _ = caller.set_fuel(TOP_UP);
            }
        }

        caller.data().shared.scheduler.yield_point(thread);
        sync_memory(&mut caller);
        handler(&mut caller, params, results)
    })
}

pub(crate) fn build_engine() -> Result<Engine> {
    let mut config = Config::new();
    // Threads is the reason wasmtime is here at all: the VoIP module imports a
    // shared memory and will not load without it.
    config.wasm_threads(true);
    // wasmtime gates shared-memory creation behind a second switch, separate
    // from the threads proposal itself.
    config.shared_memory(true);
    config.wasm_simd(true);
    config.wasm_relaxed_simd(true);
    config.wasm_bulk_memory(true);
    config.wasm_multi_memory(true);
    config.wasm_tail_call(true);
    config.consume_fuel(true);
    // Epoch interruption is what makes shutdown reliable. A guest worker only
    // notices `request_shutdown` at a host call, and its fuel is topped up
    // rather than spent down, so a thread that computes for a long time between
    // host calls outlives the `Runtime` that spawned it — along with its
    // `Store` and the module's memory. Twenty-three sequential tests
    // accumulated eighteen gigabytes that way and the suite was killed by the
    // OOM killer. Bumping the epoch interrupts guest code from outside, with no
    // cooperation from the guest.
    config.epoch_interruption(true);

    // Compiled modules are cached on disk, keyed by their bytes and the
    // compiler settings. The captured modules never change, so after the first
    // run every instantiation skips Cranelift entirely — which matters because
    // the harness builds a fresh instance per test, and a second one per guest
    // thread.
    // A machine without a usable cache directory still runs, just slower.
    if let Ok(cache) = wasmtime::Cache::new(wasmtime::CacheConfig::new()) {
        config.cache(Some(cache));
    }
    // These modules are megabytes of code and the oracle compiles them on every
    // run. Optimising the generated code costs far more than it saves for a
    // test harness that runs each function a handful of times.
    config.cranelift_opt_level(wasmtime::OptLevel::None);
    Engine::new(&config).context("building wasmtime engine")
}

/// Creates and defines the imported memory, returning it for the host state.
pub(crate) fn define_memory(
    store: &mut Store<HostState>,
    linker: &mut Linker<HostState>,
    module: &Module,
) -> Result<Option<SharedMemory>> {
    for import in module.imports() {
        let ExternType::Memory(ty) = import.ty() else {
            continue;
        };

        if ty.is_shared() {
            let memory = SharedMemory::new(module.engine(), ty.clone())
                .context("allocating shared memory")?;
            linker
                .define(&*store, import.module(), import.name(), memory.clone())
                .context("defining shared memory import")?;
            return Ok(Some(memory));
        }

        let memory = Memory::new(&mut *store, ty.clone()).context("allocating memory")?;
        linker
            .define(&*store, import.module(), import.name(), memory)
            .context("defining memory import")?;
    }
    Ok(None)
}

/// Defines a recording stub for every import the linker does not already have.
pub(crate) fn define_stubs(
    store: &mut Store<HostState>,
    linker: &mut Linker<HostState>,
    module: &Module,
    unstubbable: &mut Vec<String>,
) -> Result<()> {
    let mut stubbed = std::collections::BTreeSet::new();

    for import in module.imports() {
        let symbol = format!("{}::{}", import.module(), import.name());
        if linker
            .get(&mut *store, import.module(), import.name())
            .is_some()
        {
            continue;
        }

        let external: Extern = match import.ty() {
            ExternType::Func(ty) => {
                let module_name = import.module().to_owned();
                let name = import.name().to_owned();
                let results: Vec<Val> = ty.results().map(zero_value_of).collect();

                stubbed.insert(symbol.clone());
                host_func(&mut *store, ty.clone(), move |caller, params, outputs| {
                    let args = params.iter().map(scalar_of).collect();
                    caller.data().record(&module_name, &name, args);
                    outputs.clone_from_slice(&results);
                    Ok(())
                })
                .into()
            }
            ExternType::Memory(_) => continue, // handled by define_memory
            ExternType::Table(ty) => {
                let Some(init) = zero_ref_of(ty.element()) else {
                    unstubbable.push(symbol);
                    continue;
                };
                Table::new(&mut *store, ty.clone(), init)
                    .context("allocating stub table")?
                    .into()
            }
            ExternType::Global(ty) => {
                let init = zero_value_of(ty.content().clone());
                Global::new(&mut *store, ty.clone(), init)
                    .context("allocating stub global")?
                    .into()
            }
            _ => {
                unstubbable.push(symbol);
                continue;
            }
        };

        linker
            .define(&*store, import.module(), import.name(), external)
            .with_context(|| format!("defining stub for {symbol}"))?;
    }

    store.data().shared.stubbed.set(stubbed).ok();
    Ok(())
}

/// Widens any numeric wasm value to `i64` so a trace can hold it uniformly.
/// Pointers, lengths and handles are all i32 in these modules, which is what
/// makes this lossless in practice.
fn scalar_of(value: &Val) -> i64 {
    match value {
        Val::I32(value) => *value as i64,
        Val::I64(value) => *value,
        Val::F32(bits) => f32::from_bits(*bits) as i64,
        Val::F64(bits) => f64::from_bits(*bits) as i64,
        _ => 0,
    }
}

pub fn zero_value_of(ty: ValType) -> Val {
    match ty {
        ValType::I32 => Val::I32(0),
        ValType::I64 => Val::I64(0),
        ValType::F32 => Val::F32(0),
        ValType::F64 => Val::F64(0),
        ValType::V128 => Val::V128(0u128.into()),
        ValType::Ref(ty) => Val::null_ref(ty.heap_type()),
    }
}

/// The null reference of a reference type, used to initialise a stub table.
/// Only the two hierarchies a table can hold in these modules are supported.
fn zero_ref_of(ty: &wasmtime::RefType) -> Option<Ref> {
    let heap = ty.heap_type();
    if heap.is_func() {
        Some(Ref::Func(None))
    } else if heap.is_extern() {
        Some(Ref::Extern(None))
    } else {
        None
    }
}
