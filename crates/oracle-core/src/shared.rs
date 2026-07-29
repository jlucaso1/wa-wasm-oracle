//! State shared by every thread of one module.
//!
//! A wasm thread is a separate instance of the same module over the same linear
//! memory, so each one has its own `Store` and its own `HostState`. Anything
//! that must be coherent across them — the trace and the clock — lives here
//! behind a lock.
//!
//! Which state is shared and which is per-thread is a correctness question, not
//! a convenience one. An in-flight C++ exception belongs to the thread
//! unwinding. A monotonic clock that ran backwards between threads would be
//! worse than no clock. And the PRNG is deliberately *not* shared — see
//! `seed_for`.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex};

use crate::state::HostCall;

/// Milliseconds the *monotonic* clock advances per observation.
///
/// The step has to be non-zero — a clock that never moves makes the engine
/// busy-wait forever — and it is charged per observation. The VoIP engine polls
/// this millions of times while waiting on a futex, so the total is large by
/// design: that is what makes a spin terminate.
const CLOCK_STEP_MS: f64 = 0.01;

/// Milliseconds the *wall* clock advances per reading.
///
/// A browser has two clocks and the guest uses them for different things:
/// `performance.now()` to measure how long something took, `Date.now()` to
/// stamp a protocol message. Deriving both from one counter here conflated
/// them, and the busy-wait's millions of monotonic observations dragged the
/// wall clock forward with them — 345 seconds during a single call. The engine
/// then compared an offer's timestamp against `wa_call_is_offer_expired`'s
/// 45-second threshold and dropped it as `Missed`, which looked like the engine
/// rejecting the offer and was the harness ageing it.
///
/// Wall time therefore advances only when it is *read as wall time*, which the
/// guest does a handful of times per call rather than millions.
const WALL_STEP_MS: f64 = 1.0;

/// Seed for the host PRNG. Any constant works; it only has to be stable.
const RNG_SEED: u64 = 0x5741_5F4F_5241_434C;

/// How many individual calls are kept with their arguments.
const MAX_TRACE: usize = 8192;

/// How many log lines are kept, and how long each one may be.
///
/// Both bounds exist because an unbounded log is a memory leak waiting for a
/// hot failing path, and this host found one: a mailbox drain that fails is
/// polled thousands of times, and each failure was recorded with its full wasm
/// backtrace. One test reached 43192 lines totalling 8.5 GB, the longest line
/// 1.1 MB, and the suite was killed by the OOM killer. A truncated line still
/// says which call failed and where, which is what the log is for.
const MAX_LOG_LINES: usize = 8192;
const MAX_LOG_LINE: usize = 2048;

/// One line of host-visible output, tagged with where it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogLine {
    /// Ordinal in a single global sequence. Wall-clock ordering between threads
    /// is not reproducible, but this is: it is what makes a trace comparable
    /// across runs even when the OS interleaves differently.
    pub seq: u64,
    /// 0 is the thread that instantiated the module.
    pub thread: u64,
    pub text: String,
}

#[derive(Debug, Default)]
struct Trace {
    calls: Vec<HostCall>,
    counts: BTreeMap<String, u64>,
    logs: Vec<LogLine>,
    dropped_logs: u64,
}

/// The cross-thread half of the host state.
#[derive(Debug)]
pub struct SharedHost {
    trace: Mutex<Trace>,
    /// Virtual monotonic clock, in milliseconds, advanced under a lock so no
    /// thread ever observes it going backwards.
    clock_ms: Mutex<f64>,
    /// Virtual wall clock, in milliseconds. Separate from `clock_ms` so that
    /// measuring a duration does not age a timestamp — see `WALL_STEP_MS`.
    wall_ms: Mutex<f64>,

    next_seq: AtomicU64,
    next_thread_id: AtomicU64,
    /// Threads started and not yet finished.
    live: AtomicUsize,
    /// Signalled whenever a thread finishes, so a waiter can be woken.
    idle: Condvar,
    idle_lock: Mutex<()>,
    /// Set when any thread calls `exit` or `proc_exit`.
    exit_code: Mutex<Option<i32>>,
    /// Raised when the owning `Runtime` goes away.
    shutting_down: std::sync::atomic::AtomicBool,
    /// Decides which guest thread may execute. See `schedule.rs`.
    pub scheduler: crate::schedule::Scheduler,
    /// Imports that dispatch through the function table, by name.
    ///
    /// A fact about the module rather than about a thread, so it is settled once
    /// at instantiation and every spawned thread links against the same set.
    /// See `abi::find_invoke_imports` for why the name alone is not enough.
    pub invoke_imports: std::sync::OnceLock<std::collections::BTreeSet<String>>,
    /// Export name of the guest's function table, when it is not the
    /// conventional `__indirect_function_table`.
    pub table_export: std::sync::OnceLock<String>,
    /// Threads that have been told to check their mailbox and have not yet.
    ///
    /// Emscripten wakes a thread by posting it a `checkMailbox` message; there
    /// are no messages here, so the notification is recorded and the thread
    /// acts on it when it next waits. Without this the proxying queue is filled
    /// and never drained, which is why the engine's callbacks never fired.
    pub mailboxes: Mutex<std::collections::BTreeSet<u64>>,
    /// Imports that got a zero-returning stub rather than an implementation.
    ///
    /// A stub that is never called is harmless. One the guest *does* call is a
    /// lie waiting to surface frames away — `emscripten_resize_heap` was one,
    /// and it cost days. Recording the set lets a run report which lies it
    /// actually told.
    pub stubbed: std::sync::OnceLock<std::collections::BTreeSet<String>>,
    /// Every name the module exports.
    ///
    /// Kept so that a failed lookup can say what the module *does* have. See
    /// `exports.rs` for the bug that motivated it.
    pub exports: std::sync::OnceLock<std::collections::BTreeSet<String>>,
}

impl Default for SharedHost {
    fn default() -> Self {
        Self {
            trace: Mutex::new(Trace::default()),
            clock_ms: Mutex::new(0.0),
            wall_ms: Mutex::new(0.0),
            next_seq: AtomicU64::new(0),
            next_thread_id: AtomicU64::new(1),
            live: AtomicUsize::new(0),
            idle: Condvar::new(),
            idle_lock: Mutex::new(()),
            invoke_imports: std::sync::OnceLock::new(),
            table_export: std::sync::OnceLock::new(),
            exports: std::sync::OnceLock::new(),
            mailboxes: Mutex::new(std::collections::BTreeSet::new()),
            stubbed: std::sync::OnceLock::new(),
            exit_code: Mutex::new(None),
            shutting_down: std::sync::atomic::AtomicBool::new(false),
            scheduler: crate::schedule::Scheduler::default(),
        }
    }
}

impl SharedHost {
    /// Advances and returns the virtual clock.
    ///
    /// Every thread reads the same clock, so time cannot appear to move
    /// backwards when execution crosses threads — which is what a busy-wait
    /// loop with a deadline would notice first.
    pub fn tick_clock(&self) -> f64 {
        let mut clock = self.clock_ms.lock().unwrap_or_else(|e| e.into_inner());
        *clock += CLOCK_STEP_MS;
        *clock
    }

    /// Advances and reads the wall clock, in milliseconds since the epoch
    /// origin. Independent of the monotonic clock; see `WALL_STEP_MS`.
    pub fn tick_wall_clock(&self) -> f64 {
        let mut wall = self.wall_ms.lock().unwrap_or_else(|e| e.into_inner());
        *wall += WALL_STEP_MS;
        *wall
    }

    /// Reads the wall clock without advancing it.
    pub fn wall_clock(&self) -> f64 {
        *self.wall_ms.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn clock(&self) -> f64 {
        *self.clock_ms.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The PRNG seed for a given thread.
    ///
    /// Deliberately *not* one shared stream. A single stream is reproducible
    /// only if it is consumed in a reproducible order, and under threads it is
    /// not: two runs can interleave their `random_get` calls differently and
    /// hand the same caller different bytes. Per-thread streams, seeded from the
    /// thread id, make each thread's sequence depend only on how many bytes that
    /// thread has taken — which *is* reproducible.
    pub fn seed_for(thread: u64) -> u64 {
        RNG_SEED ^ thread.wrapping_mul(0x9E37_79B9_7F4A_7C15)
    }

    pub fn log(&self, thread: u64, mut text: String) {
        if text.len() > MAX_LOG_LINE {
            // On a char boundary, so this cannot split a multi-byte sequence.
            let cut = (0..=MAX_LOG_LINE)
                .rev()
                .find(|at| text.is_char_boundary(*at))
                .unwrap_or(0);
            text.truncate(cut);
            text.push_str(" […truncated]");
        }
        let seq = self.next_seq.fetch_add(1, Ordering::SeqCst);
        let mut trace = self.trace.lock().unwrap_or_else(|e| e.into_inner());
        if trace.logs.len() >= MAX_LOG_LINES {
            // Counted rather than dropped silently: a reader who sees a log
            // stop dead needs to know whether that is the end of the run or the
            // end of the buffer.
            trace.dropped_logs += 1;
            return;
        }
        trace.logs.push(LogLine { seq, thread, text });
    }

    /// How many log lines were discarded because the buffer was full.
    pub fn dropped_logs(&self) -> u64 {
        self.trace
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .dropped_logs
    }

    pub fn record(&self, module: &str, name: &str, args: Vec<i64>) {
        let symbol = format!("{module}::{name}");
        let mut trace = self.trace.lock().unwrap_or_else(|e| e.into_inner());
        *trace.counts.entry(symbol).or_default() += 1;
        if trace.calls.len() < MAX_TRACE {
            trace.calls.push(HostCall {
                module: module.to_owned(),
                name: name.to_owned(),
                args,
            });
        }
    }

    pub fn logs(&self) -> Vec<LogLine> {
        let mut logs = self
            .trace
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .logs
            .clone();
        // Sorting by sequence rather than by arrival makes the transcript
        // reproducible even though the interleaving is not.
        logs.sort_by_key(|line| line.seq);
        logs
    }

    pub fn log_texts(&self) -> Vec<String> {
        self.logs().into_iter().map(|line| line.text).collect()
    }

    /// Records that `thread` has mail waiting.
    pub fn notify_mailbox(&self, thread: u64) {
        self.mailboxes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(thread);
    }

    /// Takes the notification for `thread`, if there is one.
    pub fn take_mailbox(&self, thread: u64) -> bool {
        self.mailboxes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&thread)
    }

    pub fn calls(&self) -> Vec<HostCall> {
        self.trace
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .calls
            .clone()
    }

    /// Host symbols by call count, most-called first.
    pub fn hot_calls(&self) -> Vec<(String, u64)> {
        let trace = self.trace.lock().unwrap_or_else(|e| e.into_inner());
        let mut counts: Vec<(String, u64)> = trace
            .counts
            .iter()
            .map(|(symbol, count)| (symbol.clone(), *count))
            .collect();
        counts.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        counts
    }

    pub fn total_calls(&self) -> u64 {
        self.trace
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .counts
            .values()
            .sum()
    }

    pub fn clear_trace(&self) {
        let mut trace = self.trace.lock().unwrap_or_else(|e| e.into_inner());
        trace.calls.clear();
        trace.counts.clear();
        trace.logs.clear();
    }

    pub fn set_exit_code(&self, code: i32) {
        *self.exit_code.lock().unwrap_or_else(|e| e.into_inner()) = Some(code);
    }

    pub fn exit_code(&self) -> Option<i32> {
        *self.exit_code.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Asks every guest thread to stop at its next host call.
    ///
    /// A guest worker is usually an unbounded loop — it only ends when its fuel
    /// runs out, which with a realistic budget is a long time. Without this a
    /// finished test leaves threads burning CPU behind it, and the next test
    /// competes with every engine that ran before it.
    pub fn request_shutdown(&self) {
        self.shutting_down.store(true, Ordering::SeqCst);
    }

    pub fn is_shutting_down(&self) -> bool {
        self.shutting_down.load(Ordering::SeqCst)
    }

    pub fn allocate_thread_id(&self) -> u64 {
        self.next_thread_id.fetch_add(1, Ordering::SeqCst)
    }

    pub fn thread_started(&self) {
        self.live.fetch_add(1, Ordering::SeqCst);
    }

    /// Marks a thread finished and wakes anything waiting for quiescence.
    pub fn thread_finished(&self) {
        self.live.fetch_sub(1, Ordering::SeqCst);
        let _guard = self.idle_lock.lock().unwrap_or_else(|e| e.into_inner());
        self.idle.notify_all();
    }

    pub fn live_threads(&self) -> usize {
        self.live.load(Ordering::SeqCst)
    }

    /// Waits until every spawned thread has finished, or the deadline passes.
    ///
    /// This is the determinism mitigation that matters: the *interleaving* of
    /// threads is not reproducible, but the state observed after they have all
    /// finished usually is. Anything read before quiescing is a race with the
    /// module's own workers.
    ///
    /// Returns whether the wait completed rather than timed out.
    pub fn wait_until_idle(&self, timeout: std::time::Duration) -> bool {
        let deadline = std::time::Instant::now() + timeout;
        let mut guard = self.idle_lock.lock().unwrap_or_else(|e| e.into_inner());

        while self.live.load(Ordering::SeqCst) > 0 {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                return false;
            }
            let (next, timed_out) = self
                .idle
                .wait_timeout(guard, remaining)
                .unwrap_or_else(|e| e.into_inner());
            guard = next;
            if timed_out.timed_out() && self.live.load(Ordering::SeqCst) > 0 {
                return false;
            }
        }
        true
    }
}
