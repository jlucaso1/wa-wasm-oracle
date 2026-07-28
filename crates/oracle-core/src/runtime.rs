//! Executing a captured module.
//!
//! `Runtime` owns one instance and the `Store` behind it. The host environment
//! it runs in is built in `host.rs`; what a host function can see is in
//! `state.rs`.

use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use wasmtime::{Extern, Func, Instance, Linker, Module, Ref, Store, Table, Val};

use crate::embind::EmbindRegistry;
use crate::host::{DEFAULT_FUEL, build_engine, define_hosts, define_memory, zero_value_of};
use crate::shared::SharedHost;
use crate::state::{HostState, ThreadPolicy};

/// Bytes of header the engine keeps at the start of its log ring: a write
/// cursor first, then bookkeeping, with the text beginning after it.
const RING_HEADER: u32 = 24;

/// Splits the ring's text into lines.
///
/// Entries are separated by non-printable bytes, and a short run is padding
/// rather than a message.
fn split_lines(raw: &[u8]) -> Vec<String> {
    const MIN_LINE: usize = 8;

    let mut lines = Vec::new();
    let mut current = String::new();
    for &byte in raw {
        if byte.is_ascii_graphic() || byte == b' ' {
            current.push(byte as char);
        } else if current.len() >= MIN_LINE {
            lines.push(std::mem::take(&mut current));
        } else {
            current.clear();
        }
    }
    if current.len() >= MIN_LINE {
        lines.push(current);
    }
    lines
}

/// Stops the module's guest threads when the runtime goes away.
///
/// Without this a dropped `Runtime` leaves its workers running until their fuel
/// is gone, so tests keep competing with engines that already finished.
impl Drop for Runtime {
    fn drop(&mut self) {
        let shared = Arc::clone(&self.store.data().shared);
        shared.request_shutdown();
        // Bounded: a thread that will not stop must not hold up the process.
        shared.wait_until_idle(std::time::Duration::from_secs(2));
    }
}

/// A module instantiated and ready to be called.
pub struct Runtime {
    store: Store<HostState>,
    instance: Instance,
    /// Imports that could not be stubbed automatically.
    pub unstubbable: Vec<String>,
    /// Address and size of the engine's log ring buffer, once attached.
    log_ring: Option<(u32, u32)>,
}

/// The name a module exports its memory under.
///
/// Read from the module rather than assumed: minified builds export it as a
/// single letter.
fn exported_memory_name(module: &Module) -> Option<String> {
    module.exports().find_map(|export| {
        matches!(export.ty(), wasmtime::ExternType::Memory(_)).then(|| export.name().to_owned())
    })
}

/// Records what the host environment needs to know about this module.
///
/// Failing to work either one out is not fatal: the conventional names still
/// apply to the great majority of modules, and a module that dispatches nothing
/// through its table needs neither.
fn record_module_facts(store: &Store<HostState>, module: &Module, bytes: &[u8]) {
    let shared = &store.data().shared;

    // Every export name, so a failed lookup can name the near misses instead of
    // reporting only what it wanted. See `exports.rs`.
    shared
        .exports
        .set(module.exports().map(|e| e.name().to_owned()).collect())
        .ok();

    if let Some(name) = module.exports().find_map(|export| {
        matches!(export.ty(), wasmtime::ExternType::Table(_)).then(|| export.name().to_owned())
    }) {
        shared.table_export.set(name).ok();
    }

    let Ok(by_index) = crate::abi::find_invoke_imports(bytes) else {
        return;
    };
    let names: std::collections::BTreeSet<String> = module
        .imports()
        .filter(|import| matches!(import.ty(), wasmtime::ExternType::Func(_)))
        .enumerate()
        .filter(|(index, _)| by_index.contains_key(&(*index as u32)))
        .map(|(_, import)| import.name().to_owned())
        .collect();
    shared.invoke_imports.set(names).ok();
}

impl Runtime {
    /// Instantiates `bytes`, satisfying every import.
    pub fn instantiate(bytes: &[u8]) -> Result<Self> {
        let engine = build_engine()?;
        let module = Module::new(&engine, bytes).context("loading module")?;

        let mut store = Store::new(&engine, HostState::default());
        store.set_fuel(DEFAULT_FUEL).ok();

        let mut linker = Linker::new(&engine);
        // Emscripten declares each import once, but a defensive allow keeps a
        // duplicated declaration from aborting the whole run.
        linker.allow_shadowing(true);

        // The memory has to exist before any host function can read through it.
        let memory = define_memory(&mut store, &mut linker, &module)?;
        store.data_mut().memory = memory.clone();
        store.data_mut().memory_export = exported_memory_name(&module);

        // Only a module with shared memory can have threads: a second instance
        // over a private memory would not see the first one's heap, so its
        // "thread" would operate on a different program's state.
        if let Some(memory) = memory {
            let shared = Arc::clone(&store.data().shared);
            let policy = store.data().threads;
            store.data_mut().spawner = Some(Arc::new(crate::threads::Spawner::new(
                engine.clone(),
                module.clone(),
                memory,
                shared,
                policy,
            )));
        }

        // Settle the two module facts the host environment needs before it is
        // built: which imports dispatch through the function table, and what
        // that table is called. Both are conventional in an unminified module
        // and unrecoverable from names in a minified one.
        record_module_facts(&store, &module, bytes);

        let mut unstubbable = Vec::new();
        define_hosts(&mut store, &mut linker, &module, &mut unstubbable)?;

        let instance = linker
            .instantiate(&mut store, &module)
            .context("instantiating module")?;

        let mut runtime = Self {
            store,
            instance,
            unstubbable,
            log_ring: None,
        };
        runtime.sync_memory();
        Ok(runtime)
    }

    pub fn state(&self) -> &HostState {
        self.store.data()
    }

    /// Grows the guest's memory by `pages`, returning the new size in pages.
    ///
    /// A WASI command whose libc grows the heap on demand can find its own
    /// startup allocation landing past the end of a minimum-sized memory. This
    /// gives the harness a way to hand it room up front.
    pub fn grow_memory(&mut self, pages: u64) -> Result<u64> {
        // Same lesson as `sync_memory`: the export is not always called
        // `memory`. Looking only for that name silently does nothing on a
        // minified module.
        let name = self
            .store
            .data()
            .memory_export
            .clone()
            .ok_or_else(|| anyhow!("module exports no memory to grow"))?;
        let Some(Extern::Memory(memory)) = self.instance.get_export(&mut self.store, &name) else {
            return Err(anyhow!("module exports no memory to grow"));
        };
        let before = memory
            .grow(&mut self.store, pages)
            .context("growing guest memory")?;
        self.sync_memory();
        Ok(before + pages)
    }

    /// Refreshes the host's view of an exported memory. See `sync_memory`.
    fn sync_memory(&mut self) {
        if self.store.data().memory.is_some() {
            return;
        }
        let name = self
            .store
            .data()
            .memory_export
            .clone()
            .unwrap_or_else(|| "memory".to_owned());
        let Some(Extern::Memory(memory)) = self.instance.get_export(&mut self.store, &name) else {
            return;
        };
        let window = (
            memory.data_ptr(&self.store) as usize,
            memory.data_size(&self.store),
        );
        self.store.data_mut().linear = Some(window);
    }

    /// Names of the module's exported functions.
    pub fn functions(&mut self) -> Vec<String> {
        let mut names: Vec<String> = self
            .instance
            .exports(&mut self.store)
            .filter(|export| export.clone().into_func().is_some())
            .map(|export| export.name().to_owned())
            .collect();
        names.sort();
        names
    }

    pub fn func_type(&mut self, name: &str) -> Option<wasmtime::FuncType> {
        let func = self.instance.get_func(&mut self.store, name)?;
        Some(func.ty(&self.store))
    }

    /// Calls an exported function.
    pub fn call(&mut self, name: &str, args: &[Val]) -> Result<Vec<Val>> {
        let func = self
            .instance
            .get_func(&mut self.store, name)
            .ok_or_else(|| anyhow!("no exported function `{name}`"))?;

        let ty = func.ty(&self.store);
        let expected = ty.params().len();
        if args.len() != expected {
            return Err(anyhow!(
                "`{name}` takes {expected} argument(s), got {}",
                args.len()
            ));
        }

        let mut results: Vec<Val> = ty.results().map(zero_value_of).collect();

        // Entering guest code takes a turn, and holds it until the call
        // returns. Yield points inside host calls hand it on.
        let shared = Arc::clone(&self.store.data().shared);
        shared.scheduler.acquire(0);
        let outcome = func
            .call(&mut self.store, args, &mut results)
            .with_context(|| format!("calling `{name}`"));
        shared.scheduler.release(0);
        // The call may have grown memory, invalidating the cached window.
        self.sync_memory();
        outcome?;
        Ok(results)
    }

    /// Runs the C++ static constructors, which is what makes an emscripten
    /// module register its embind API and initialise its runtime.
    ///
    /// Both entry points are called when present. Which one performs the embind
    /// registrations differs by module: the VoIP engine registers from its
    /// static constructors, while the VOPRF module leaves them to
    /// `_embind_initialize_bindings`. Calling only the first finds an empty API
    /// and looks like a module with no embind surface at all.
    pub fn run_ctors(&mut self) -> Result<()> {
        let mut ran = false;

        for name in ["__wasm_call_ctors", "_initialize"] {
            if self.instance.get_func(&mut self.store, name).is_some() {
                self.call(name, &[])?;
                ran = true;
                break;
            }
        }

        if self
            .instance
            .get_func(&mut self.store, "_embind_initialize_bindings")
            .is_some()
        {
            self.call("_embind_initialize_bindings", &[])?;
            ran = true;
        }

        if ran {
            Ok(())
        } else {
            Err(anyhow!("module exports no constructor entry point"))
        }
    }

    /// Allocates `len` bytes inside the module using its own allocator, so the
    /// pointer is valid for code that will later free it.
    pub fn malloc(&mut self, len: u32) -> Result<u32> {
        let results = self.call("malloc", &[Val::I32(len as i32)])?;
        match results.first() {
            Some(Val::I32(ptr)) if *ptr != 0 => Ok(*ptr as u32),
            _ => Err(anyhow!("malloc({len}) failed")),
        }
    }

    /// How many entries this instance's function table has.
    ///
    /// Threads are separate instances and each builds its own table, which is
    /// sound only while nothing changes at runtime. This module changes it —
    /// `table.grow`, `table.fill`, `table.set` — so comparing this against what
    /// a worker reports says whether their tables have drifted apart, and a
    /// worker calling an index only this one has would trap.
    pub fn table_size(&mut self) -> Option<u64> {
        let name = self.store.data().shared.table_export.get()?.clone();
        match self.instance.get_export(&mut self.store, &name) {
            Some(Extern::Table(table)) => Some(table.size(&self.store)),
            _ => None,
        }
    }

    pub fn free(&mut self, ptr: u32) -> Result<()> {
        self.call("free", &[Val::I32(ptr as i32)])?;
        Ok(())
    }

    /// Copies `bytes` into freshly allocated module memory.
    pub fn write_bytes(&mut self, bytes: &[u8]) -> Result<u32> {
        let ptr = self.malloc(bytes.len() as u32)?;
        self.write_bytes_at(ptr, bytes)?;
        Ok(ptr)
    }

    pub fn read(&self, ptr: u32, len: u32) -> Result<Vec<u8>> {
        self.store.data().read(ptr, len)
    }

    /// Reads an exported `i32` global.
    ///
    /// Most modules export none, and the interesting ones are usually among
    /// them: the VoIP engine keeps its call context in a global that only guest
    /// code can see, which is why so much about that context had to be inferred
    /// from disassembly rather than read. `scripts/export_globals.py` patches a
    /// copy of a module to export its globals, and this reads them back.
    pub fn global_i32(&mut self, name: &str) -> Result<i32> {
        let global = self
            .instance
            .get_global(&mut self.store, name)
            .ok_or_else(|| anyhow::anyhow!("module exports no global named `{name}`"))?;

        match global.get(&mut self.store) {
            wasmtime::Val::I32(value) => Ok(value),
            other => Err(anyhow::anyhow!("global `{name}` is {other:?}, not an i32")),
        }
    }

    pub fn read_cstr(&self, ptr: u32) -> Result<String> {
        self.store.data().read_cstr(ptr)
    }

    /// Drops the recorded trace, so a test can isolate one operation.
    pub fn clear_calls(&mut self) {
        self.store.data().shared.clear_trace();
    }

    /// Finalises and returns the embind API the module registered.
    ///
    /// Call after `run_ctors`; before that the registry is empty.
    pub fn embind(&mut self) -> EmbindRegistry {
        let mut registry = self.store.data().embind.clone();
        registry.settle();
        registry
    }

    pub(crate) fn embind_function(&self, name: &str) -> Option<crate::embind::EmbindFunction> {
        self.store
            .data()
            .embind
            .functions
            .iter()
            .find(|function| function.name == name)
            .cloned()
    }

    pub(crate) fn embind_class(&self, type_id: u32) -> Option<crate::embind::EmbindClass> {
        // The registry is only settled on demand, so look through both the
        // registered classes and the pending method/constructor registrations.
        let mut registry = self.store.data().embind.clone();
        registry.settle();
        registry.classes.get(&type_id).cloned()
    }

    /// The value behind an emval handle, if the table still holds it.
    pub(crate) fn emval_value(&self, handle: u32) -> Option<crate::Value> {
        self.store.data().emval.get(handle).cloned()
    }

    pub(crate) fn emval_release(&mut self, handle: u32) {
        self.store.data_mut().emval.decref(handle);
    }

    /// How many emval handles are currently live. A count that grows across
    /// repeated calls means handles are leaking.
    pub fn emval_live(&self) -> usize {
        self.store.data().emval.len()
    }

    pub(crate) fn type_name(&self, id: u32) -> String {
        self.store.data().embind.type_name(id)
    }

    pub(crate) fn export(&mut self, name: &str) -> Option<Extern> {
        self.instance.get_export(&mut self.store, name)
    }

    pub(crate) fn table_get(&mut self, table: Table, index: u64) -> Option<Ref> {
        table.get(&mut self.store, index)
    }

    pub(crate) fn call_func(
        &mut self,
        func: Func,
        args: &[Val],
        results: &mut [Val],
    ) -> Result<()> {
        func.call(&mut self.store, args, results)
            .map_err(|error| anyhow!("{error}"))
    }

    /// Writes bytes at an address the caller already owns.
    pub fn write_bytes_at(&mut self, ptr: u32, bytes: &[u8]) -> Result<()> {
        self.store.data().write(ptr, bytes)
    }

    /// Refills the execution budget, so one exhausted call does not poison the
    /// rest of a session.
    pub fn refuel(&mut self) {
        let _ = self.store.set_fuel(DEFAULT_FUEL);
    }

    /// Chooses how `pthread_create` is answered.
    ///
    /// Must be set before the module initialises, since that is when the
    /// threads are requested. The spawner is rebuilt so it carries the new
    /// policy into any thread it starts.
    pub fn set_thread_policy(&mut self, policy: ThreadPolicy) {
        self.store.data_mut().threads = policy;
        if policy == ThreadPolicy::Spawn {
            // Only worth its cost once more than one thread can run.
            self.store.data().shared.scheduler.enable();
        }

        let rebuilt = self
            .store
            .data()
            .spawner
            .as_ref()
            .map(|spawner| Arc::new(spawner.with_policy(policy)));
        if let Some(spawner) = rebuilt {
            self.store.data_mut().spawner = Some(spawner);
        }
    }

    /// Execution budget left on this instance's own store.
    ///
    /// A call that fails for no visible reason has usually run out; without
    /// this the difference between "the module refused" and "the harness cut it
    /// off" is invisible.
    pub fn fuel_remaining(&self) -> Option<u64> {
        self.store.get_fuel().ok()
    }

    /// The guest's current wall-clock time, in Unix seconds.
    ///
    /// Anything handed to a module that it will compare against "now" has to
    /// come from here. The host's clock is virtual and starts in 2021, so a
    /// real timestamp looks like it is from the future — which is how a
    /// perfectly good call offer arrives already expired.
    pub fn virtual_unix_time(&self) -> u64 {
        let millis = crate::emscripten::EPOCH_MS + self.store.data().shared.wall_clock();
        (millis / 1000.0) as u64
    }

    /// Reads a 32-bit word out of guest memory.
    pub fn read_u32_at(&self, ptr: u32) -> Result<u32> {
        self.state().read_u32(ptr)
    }

    /// Whether `index` names a callable entry in the function table.
    ///
    /// Trampolines load an index out of an object and call through it, so this
    /// is how to tell an initialised object from an uninitialised one before
    /// calling and trapping.
    pub fn table_entry_exists(&mut self, index: u32) -> bool {
        let Some(Extern::Table(table)) = self.export("__indirect_function_table") else {
            return false;
        };
        matches!(
            self.table_get(table, index as u64),
            Some(Ref::Func(Some(_)))
        )
    }

    pub fn logs(&self) -> Vec<String> {
        self.store.data().logs()
    }

    /// Size of the guest's linear memory, in bytes.
    pub fn memory_size(&self) -> usize {
        let state = self.store.data();
        state
            .memory
            .as_ref()
            .map(|memory| memory.data_size())
            .or_else(|| state.linear.map(|(_, size)| size))
            .unwrap_or(0)
    }

    /// The shared state, for observing threads other than this one.
    pub fn shared(&self) -> &Arc<SharedHost> {
        &self.store.data().shared
    }

    /// Waits for every guest thread to finish.
    ///
    /// Thread *interleaving* is not reproducible, but the state after all of
    /// them have finished usually is, so this is what makes an observation
    /// meaningful under `ThreadPolicy::Spawn`. Returns false on timeout, which
    /// means a worker is still running and anything read next is a race.
    pub fn quiesce(&self, timeout: std::time::Duration) -> bool {
        self.store.data().shared.wait_until_idle(timeout)
    }

    /// Registers the instantiating thread as emscripten's main runtime thread.
    ///
    /// Required before `process_queued_calls` can do anything — it asserts
    /// `emscripten_is_main_runtime_thread()` internally — and not the default,
    /// because a main thread that blocks inside a call cannot answer the guest
    /// code that then waits on it. `HostState::register_main_thread` has the
    /// full trade-off.
    ///
    /// Must be set before `run_ctors`: the registration happens during startup.
    pub fn set_main_thread_registration(&mut self, register: bool) {
        self.store.data_mut().register_main_thread = register;
    }

    /// Every import that got a stub, whether or not it was called.
    ///
    /// A stub records each call; anything defined some other way may not. This
    /// is how to tell "never called" apart from "called but not instrumented",
    /// which look identical from the trace.
    pub fn stubbed_imports(&self) -> std::collections::BTreeSet<String> {
        self.store
            .data()
            .shared
            .stubbed
            .get()
            .cloned()
            .unwrap_or_default()
    }

    /// Imports the guest called that have no real implementation.
    ///
    /// The list to work through when behaviour is wrong in a way that points
    /// nowhere: each entry is a call the host answered with a zero it made up.
    /// A stub nobody calls is fine and is not listed.
    pub fn stubs_called(&self) -> Vec<(String, usize)> {
        let shared = &self.store.data().shared;
        let Some(stubbed) = shared.stubbed.get() else {
            return Vec::new();
        };

        let mut counts: std::collections::BTreeMap<String, usize> =
            std::collections::BTreeMap::new();
        for call in shared.calls() {
            let symbol = format!("{}::{}", call.module, call.name);
            if stubbed.contains(&symbol) {
                *counts.entry(symbol).or_default() += 1;
            }
        }

        let mut out: Vec<(String, usize)> = counts.into_iter().collect();
        // Most-called first: the hot ones are where a made-up answer does the
        // most damage.
        out.sort_by_key(|(_, count)| std::cmp::Reverse(*count));
        out
    }

    /// Every host call made, from any thread.
    ///
    /// `state().calls_to()` only sees the instantiating thread's trace, and the
    /// engine's outbound callbacks are made from its worker pool — so counting
    /// them there reports zero for calls that did happen.
    pub fn all_calls_to(&self, symbol: &str) -> Vec<crate::state::HostCall> {
        self.store
            .data()
            .shared
            .calls()
            .into_iter()
            .filter(|call| format!("{}::{}", call.module, call.name) == symbol)
            .collect()
    }

    /// Every name this module exports.
    ///
    /// Exposed so a test can assert against the real export list rather than
    /// against an assumption about it — the assumption is what went wrong; see
    /// `exports.rs`.
    pub fn export_names(&self) -> std::collections::BTreeSet<String> {
        self.store
            .data()
            .shared
            .exports
            .get()
            .cloned()
            .unwrap_or_default()
    }

    /// Runs the work worker threads have queued for the main thread.
    ///
    /// Emscripten proxies a call to the main thread by queueing it; the main
    /// thread runs it when its event loop next turns. There is no event loop
    /// here, so nothing ever turns, and the queued work simply accumulates —
    /// which for the VoIP engine means an offer it has decided to answer whose
    /// answer is never sent.
    ///
    /// Returns whether the module had the entry point at all; a single-threaded
    /// module has nothing to drain and reports `false`.
    pub fn process_queued_calls(&mut self) -> bool {
        const LEGACY: &str = "emscripten_main_thread_process_queued_calls";
        const MAILBOX: &str = "_emscripten_check_mailbox";

        // Two mechanisms, and which one a module has depends on how it was
        // built. Emscripten's newer proxying hands work over through a
        // per-thread mailbox, and `_emscripten_check_mailbox` runs whatever
        // arrived while asserting nothing. The older entry point opens by
        // asserting `emscripten_is_main_runtime_thread()` and traps on
        // `unreachable` when that does not hold — which is the default here,
        // for the reason in `HostState`.
        //
        // Calling the legacy one unconditionally therefore failed on every
        // single tick, and the VoIP engine dispatches its outgoing signaling
        // through exactly this queue.
        //
        // The mailbox is preferred because it does not assert *that* — but it
        // is not a way out: `_emscripten_check_mailbox` opens by asserting
        // `pthread_self()` and traps on `unreachable` when the calling thread
        // has no pthread, which the unregistered main thread does not. So both
        // entry points fail today, and swapping between them only changes
        // which name appears in the log. Draining cannot work until the main
        // thread is registered, and registering it breaks startup — the real
        // fix is to drain off the blocking path. `draining_the_proxy_queue_*`
        // in `tests/threading.rs` pins this down.
        let mut drained = false;

        if let Some(mailbox) = self.instance.get_func(&mut self.store, MAILBOX) {
            // Best-effort by nature: a failure here is the guest's, so it is
            // logged rather than propagated.
            if let Err(error) = mailbox.call(&mut self.store, &[], &mut []) {
                self.store.data().log(format!("{MAILBOX} failed: {error}"));
            }
            drained = true;
        }

        if self.store.data().register_main_thread
            && let Some(func) = self.instance.get_func(&mut self.store, LEGACY)
        {
            if let Err(error) = func.call(&mut self.store, &[], &mut []) {
                self.store.data().log(format!("{LEGACY} failed: {error}"));
            }
            drained = true;
        }

        self.sync_memory();
        drained
    }

    /// Waits for threads to go idle, draining the main thread's queue as it
    /// goes.
    ///
    /// This is what a browser does while a call is in flight, and without it
    /// `quiesce` can return "idle" with work still queued: the workers really
    /// are done, and what remains is waiting on a main thread that never runs.
    pub fn settle(&mut self, timeout: std::time::Duration) -> bool {
        /// Short enough that a reply is not left sitting, long enough that
        /// draining does not dominate the run.
        const TICK: std::time::Duration = std::time::Duration::from_millis(20);

        let deadline = std::time::Instant::now() + timeout;
        loop {
            self.process_queued_calls();
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                return false;
            }
            if self.quiesce(TICK.min(remaining)) {
                // One last drain: a thread that finished during the wait may
                // have queued its reply on the way out.
                self.process_queued_calls();
                return true;
            }
        }
    }

    pub fn live_threads(&self) -> usize {
        self.store.data().shared.live_threads()
    }

    /// Times a guest thread ran without being granted a turn.
    ///
    /// Non-zero means guest code waited on something it never signalled through
    /// a host call, so the host had to break serialisation to stay live.
    pub fn forced_turns(&self) -> u64 {
        self.store.data().shared.scheduler.forced_turns()
    }
}

impl Runtime {
    /// Sets the arguments the guest sees. `argv[0]` is supplied automatically,
    /// because a C `main` that reads its own program name would otherwise see
    /// the first real argument there.
    pub fn set_args(&mut self, args: &[String]) {
        let state = self.store.data_mut();
        state.wasi.args = std::iter::once("module".to_owned())
            .chain(args.iter().cloned())
            .collect();
    }

    pub fn set_env(&mut self, env: &[(String, String)]) {
        self.store.data_mut().wasi.env = env.to_vec();
    }

    /// Places a file in the guest filesystem before it runs.
    pub fn add_file(&mut self, path: &str, contents: Vec<u8>) {
        self.store.data_mut().wasi.add_file(path, contents);
    }

    pub fn wasi(&self) -> &crate::wasi::WasiState {
        &self.store.data().wasi
    }

    /// Runs a WASI command module's entry point.
    ///
    /// `proc_exit` unwinds rather than returning, so a normal exit arrives here
    /// as an error carrying the status; that is reported as the exit code, not
    /// as a failure.
    pub fn run_main(&mut self) -> Result<i32> {
        let entry = ["_start", "__main_void", "main"]
            .into_iter()
            .find(|name| self.instance.get_func(&mut self.store, name).is_some())
            .ok_or_else(|| anyhow!("module has no WASI entry point"))?;

        let arity = self
            .func_type(entry)
            .map(|ty| ty.params().len())
            .unwrap_or(0);
        // `main` may be declared as either `()` or `(argc, argv)`.
        let args: Vec<Val> = (0..arity).map(|_| Val::I32(0)).collect();

        match self.call(entry, &args) {
            Ok(results) => Ok(match results.first() {
                Some(Val::I32(code)) => *code,
                _ => self.store.data().exit_code().unwrap_or(0),
            }),
            Err(error) => match self.store.data().exit_code() {
                Some(code) => Ok(code),
                None => Err(error),
            },
        }
    }
}

/// Reading the engine's own diagnostics.
///
/// The VoIP engine writes structured log lines into a ring buffer the host
/// supplies. That output names exactly why it rejected an input — the parser
/// error, the failing subsystem, the status code — which turns "the call
/// returned void" into a diagnosis.
impl Runtime {
    /// Allocates a log ring buffer and hands it to the engine.
    ///
    /// Must be called before the subsystem being investigated initialises, or
    /// its startup lines are lost.
    pub fn attach_log_ring(&mut self, bytes: u32) -> Result<()> {
        let buffer = self.malloc(bytes)?;
        self.call_embind(
            "initLogRingBuffer",
            &[
                crate::Value::Int(buffer as i64),
                crate::Value::Int(bytes as i64),
            ],
        )
        .context("initLogRingBuffer")?;
        self.log_ring = Some((buffer, bytes));
        Ok(())
    }

    /// Sets how much the engine is willing to log, and returns the old level.
    ///
    /// Every line goes through one threshold compare: the dispatcher emits only
    /// when the level stored at `LOG_LEVEL` is at least the line's own. The
    /// logging wrappers pick the level — 8414 logs at 3, 8416 at 4 — and this
    /// module starts at **4**, so both are already on.
    ///
    /// Read it before concluding anything from a missing line. A subsystem's
    /// "finished" line is level 4, and if the threshold were lower its absence
    /// would say nothing about whether the subsystem ran. Here it is not lower,
    /// which is what makes such an absence evidence. No module patching is
    /// involved: the threshold is a plain word in memory.
    pub fn set_engine_log_level(&mut self, level: i32) -> Result<i32> {
        /// The dispatcher's threshold. Both logging wrappers reduce to
        /// `if (*(i32*)LOG_LEVEL >= level) emit(...)`.
        const LOG_LEVEL: u32 = 1_263_116;

        let previous = self.state().read_u32(LOG_LEVEL)? as i32;
        self.write_bytes_at(LOG_LEVEL, &level.to_le_bytes())?;
        Ok(previous)
    }

    /// Reads back the lines the engine has written so far.
    ///
    /// The buffer is not opaque: a 24-byte header carries the number of bytes
    /// written, and the text follows. Reading `used` rather than scanning the
    /// whole allocation is what makes this a log reader instead of a memory
    /// scan — the previous version searched the entire buffer for printable
    /// runs, so it reported whatever unrelated bytes happened to sit past the
    /// end of the log as extra lines.
    pub fn engine_log(&self) -> Vec<String> {
        let Some((buffer, size)) = self.log_ring else {
            return Vec::new();
        };
        let Some(used) = self.ring_used(buffer, size) else {
            return Vec::new();
        };
        let Ok(raw) = self.read(buffer + RING_HEADER, used) else {
            return Vec::new();
        };
        split_lines(&raw)
    }

    /// Bytes of log text currently in the ring, clamped to what fits.
    fn ring_used(&self, buffer: u32, size: u32) -> Option<u32> {
        let capacity = size.saturating_sub(RING_HEADER);
        let used = self.state().read_u32(buffer).ok()?;
        Some(used.min(capacity))
    }

    /// Whether the engine has overwritten log it had already written.
    ///
    /// Once this is true the transcript is missing its oldest lines, and any
    /// index into it from an earlier read points somewhere else. Callers that
    /// diff two reads have to treat it as "this comparison is invalid" rather
    /// than as a gap.
    pub fn engine_log_overflowed(&mut self) -> bool {
        if self.log_ring.is_none() {
            return false;
        }
        matches!(
            self.call_embind("getLogRingBufferOverflowCount", &[]),
            Ok(crate::Value::Int(count)) if count > 0
        )
    }

    /// Engine log lines produced after the first `mark` lines.
    ///
    /// Takes a count rather than the previous lines: the engine repeats itself
    /// verbatim across operations, so filtering by content silently hides the
    /// second occurrence — which looks exactly like an operation that logged
    /// nothing.
    ///
    /// Only valid while the ring has not overflowed; see
    /// `engine_log_overflowed`.
    pub fn engine_log_from(&self, mark: usize) -> Vec<String> {
        self.engine_log().into_iter().skip(mark).collect()
    }
}
