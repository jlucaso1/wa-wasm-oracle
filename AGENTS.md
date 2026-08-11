# wa-wasm-oracle

Runs WhatsApp Web's shipped wasm modules and calls into them. Read `README.md`
first — it holds the module map, the recovered VoIP API, and the known limits.

## Build & verify

```sh
cargo fmt --all
cargo clippy --all --tests --examples --release -- -D warnings
cargo test --release
cargo machete                 # no unused dependencies
```

`--examples` is in that line deliberately. They were outside it, and what
accumulated behind the gap was a dead helper carrying `flate2` — a whole
dependency kept alive by a function nothing called.

CI runs `stable`, which is ahead of the toolchain in this container. A clean
local clippy is therefore not a clean CI clippy: `useless_borrows_in_formatting`
failed the lint job on lines 1.94 accepts. When CI reports a lint you cannot
reproduce, the version gap is the first thing to check, not the last.

Always `--release` for anything that executes a module. In a debug build
Cranelift compiles the 9.3 MiB VoIP module so slowly that runs look hung.

Tests exercise the real captures and skip when the capture directory is missing.
A skipped run is not a passing run: check for `skipping:` in
`cargo test --release -- --nocapture` before trusting green.

## Where the modules come from

`python3 scripts/fetch-wasm.py` puts them in `wasm/`, pulled from the whatspec
`bundle-store` release and verified against the SHA-256s in `wasm.lock.json`.
Do not commit them — a copy in the repository drifts from the capture the
protocol notes refer to. `WA_WASM_DIR` overrides the lookup, and a sibling
whatsapp-rust checkout with `docs/captured-js/wasm/` still works.

**The lock pins hashes, not the latest capture, and that is the point.** Every
function index and absolute address in `README.md`, `VOIP_STATUS.md` and the
tests — `infer_index(&bytes, 10_347)`, `read(1_352_840, 4)` — was read out of
these exact bytes. Repointing the lock at a newer WhatsApp module invalidates
all of them at once, silently: the reads still succeed, they just answer about
different code. Treat a capture bump as a re-derivation, never as an update.

## Ground rules

- **A stub that returns zero is a hypothesis, not an implementation.** The table
  in `README.md` lists what each zeroed stub actually cost: busy-waits in the
  millions, skipped static initialisers, threads that never ran. When a module
  misbehaves, read `hot_calls()` before suspecting the module.
- **Never look up an export without reporting a miss.** Go through
  `exports.rs`. A `let Some(..) = get_export(..) else { return; }` cost a day:
  the module exports `_emscripten_thread_init`, the host asked for
  `__emscripten_thread_init`, and the silent miss left the main thread
  unregistered — so every call a worker queued for it was dropped, and the
  symptom surfaced thousands of instructions away. A miss now names the near
  misses, normalising leading underscores and case.
- **Determinism is the product.** Anything that would vary between runs — clocks,
  randomness, filesystem — must be replaced by something reproducible. A
  comparison against whatsapp-rust is worthless if the oracle's own output
  drifts.
- **Unsupported is an error, never a guess.** `call.rs` refuses types it cannot
  marshal, and `wasi.rs` returns `ENOSYS` rather than success for calls it does
  not implement. A wrong answer from an oracle is worse than no answer.
- **Derive host functions from the module's declared signature.** Emscripten
  changes these between releases — `_embind_register_bigint` takes five arguments
  in one capture and seven in another — so a fixed `func_wrap` breaks on the next
  module. `embind.rs`, `cxa.rs` and `wasi.rs` all build from `import.ty()`.
- **The main thread must not be able to block in wasm.** Pass
  `can_block = 0` to `__emscripten_thread_init` — the value emscripten itself
  uses on the web (`canBlock: !ENVIRONMENT_IS_WEB`). With 1, a waiting main
  thread takes `memory.atomic.wait32`, which blocks *inside* wasm with no host
  call: it holds its scheduler turn while blocked and the thread that would
  notify it never gets one. That was the "startup race", and it was the
  harness's, not the engine's — `initVoipStack` trapped about one attempt in
  six, and no amount of retrying would have fixed it. With 0 the main thread
  takes emscripten's busy-wait, which calls `_emscripten_yield` each time round:
  a host call, so the turn is yielded and the proxying queue drains.
  `startup_is_reliable_and_never_forces_a_turn` is the guard; `forced_turns()`
  must stay zero.
- **Guest threads are not serialised, whatever `schedule.rs` says.** Measured:
  `Runtime::max_threads_in_wasm()` peaks at **five or six**, in every round,
  healthy and corrupt alike. A thread acquires the turn once around its whole
  routine and `yield_point` hands it on only while somebody is blocked in their
  own first `acquire`, so once every worker has forced past `TURN_TIMEOUT`
  nothing waits and nothing yields. Making the turn cover exactly the
  guest-execution window does serialise and is unusable — a two-minute round
  had not finished in ten. Do not write code whose safety argument is "the
  scheduler holds all but one thread outside guest code"; that is not true
  today. `HostState::read`'s SAFETY note is the one place still saying it.
- **Every test that starts an engine takes both locks.** `threaded_guard()`
  serialises within a test binary; `common::engine_lock()` serialises *across*
  them, because cargo runs the binaries in parallel and `threading`,
  `signaling` and `host_environment` all bring up PJSIP worker pools. Two pools
  competing for cores miss their own deadlines, and that surfaces as an
  unrelated-looking failure — `initVoipStack` trapping inside a test that is not
  about startup. The cross-binary lock is a TCP bind rather than a lock file:
  the OS releases a port when the process dies, so a killed test cannot wedge
  every later run.
- **Sweep, don't spot-check.** The `convertFixed32BitToFloat` model was wrong in
  a way that only showed up at `n >= 25`; a two-point test would have shipped it.
- **Inspection must not compile.** `inspect.rs` is `wasmparser` only, and
  resolves signatures by hand. Reaching for a runtime there trades 8 ms for
  seconds to learn something already present in the bytes.
- **Keep the dependency surface honest.** wasmtime is `default-features = false`
  with an explicit list; adding a feature means something needs it. Run
  `cargo machete` before calling work done.
- **Identify by data segments, not `strings(1)`.** Dense wasm opcodes decode as
  printable ASCII by accident; the `strings` subcommand scans data segments only.
- **No real PII.** Test JIDs use fictitious `1555...` numbers.
- `unsafe` is denied workspace-wide. The shared-memory accessors in `runtime.rs`
  carry `#[allow]` plus a SAFETY note; do not add more without one.

## Where things live

Host environment, in the order a module exercises it:

- `emscripten.rs` — clock, PRNG, `invoke_*` trampolines, thread refusal
- `cxa.rs` — C++ throw/catch, exception messages via `__get_exception_message`
- `wasi.rs` — preview-1 subset over an in-memory filesystem
- `embind.rs` — recovers the registered API from the `_embind_register_*` calls
- `call.rs` — marshals C++ types and calls through the invoker table

## Two host bugs worth not repeating

- **The guest memory is not always exported as `memory`.** mozjpeg exports it as
  `x`; a host looking only for the conventional name cannot read that module at
  all, and every read fails in a way that reads as "the module is broken". The
  name now comes from the module's export list.
- **A dropped `Runtime` used to leave its workers running.** A guest worker loop
  only ends when its fuel runs out, so finished tests kept burning CPU. `Drop`
  signals a shutdown that every host call checks.

## The signaling tests are slow, and were flakier than this file claimed

They bring up PJSIP's worker pool and take about fifteen minutes, so they are
`#[ignore]`d and run on their own:

```sh
cargo test --release --test signaling -- --ignored --test-threads 1
```

This heading used to end at "not flaky", while `a_well_formed_offer_is_accepted`
failed about one run in four. Two more causes are now found and handled, both in
the module docs of `signaling.rs`: startup returning before `call_event_proc`
exists, and the engine's own lock watchdog firing on the offer path — the second
is `schedule.rs`'s doing, and the retry for it is conditioned on that complaint
and nothing else, so a real refusal still fails the test.

What was already known, and still holds:

- **Startup raced about one time in nine.** Measured with
  `examples/init_stress.rs`: `initVoipStack` finishes in ~5 ms, and the trap
  landed with 99.6% of the fuel untouched and the media init already logged as
  complete — two real threads reaching the same state. `schedule.rs` now runs
  one guest thread at a time, which took it to roughly 1 in 40; six retries
  cover the rest.
- **Offer handling is asynchronous.** The call returning says nothing about the
  event thread. Wait for the log to grow and go quiet, never for a fixed time.

Also worth remembering: **substring matches on log lines lie.**
`handleIncomingSignalingOffer from platform ...` contains `Offer from`, so a
test looking for the call-stack banner passed for a stanza that never parsed.

## Meeting a module you have never seen

`oracle abi` is the way in, and it is deliberately general — it reads bytecode,
so it works on captures that do not exist yet. The order that has paid off:

1. `oracle inspect <id>` — what it imports says which host environment it wants
   (`env::_embind_*` → emscripten/embind; `wasi_snapshot_preview1` → a WASI
   command; a shared memory → it expects threads).
2. `oracle strings <id>` — data segments identify the module. Never `strings(1)`
   on the whole file.
3. `oracle embind <id>` for an emscripten module; `oracle abi <id>` for anything
   stripped.
4. If `abi` reports a trampoline, read the vtable slot out of a live object and
   follow it with `--slot`.

## Open work

1. **Reconcile `participants[0]` with the bytecode.** `offer.cc:485` reads it as
   null, and three static facts say it cannot be — see "What `l1` is" in
   `VOIP_STATUS.md`. One of the two is measuring something else, and the probe
   is the newer and less certain of them.
2. **What writes over the guest heap.** Two symptoms, one bug, and it is
   upstream of most of the rest of this list.

   `f1139` → `f763` → `f13513` → `f13089` is a container destructor inside
   `startVoipCall`'s embind wrapper handing `free` a pointer it refuses. It
   shows up whichever stack the workers use — see "Giving each thread its own
   stack works, and is still not the fix" — so it predates every explanation
   offered for it so far.

   The other symptom is readable rather than fatal, and it is not what it
   looked like. About one run in four,
   `settings_from_an_incoming_offer_do_not_unblock_an_outgoing_call` finds the
   engine's log ring full of high-entropy bytes with the ring not overflowed.
   `examples/ring_corruption.rs` measured it: it is not the ring, it is *all*
   of linear memory — one changed span from `0xd` to the end, 83% zeroes down
   to 3%, a 64 KiB guard block gone rather than moved, static string data
   unreadable — while `emscripten_stack_get_base` still answers `0x24cf60`, so
   the guest is executing correctly throughout. Host writes, the host's entropy
   source, a moved mapping and memory growth are each excluded by measurement;
   see "Nothing writes key-shaped bytes over a live allocation" in
   `VOIP_STATUS.md`. Note while you are there that wasmtime freezes a shared
   memory's base at creation, so the host cannot detect a move even if one
   happened.

   The fault is open; the oracle answering from it is not.
   `Runtime::memory_view_is_coherent` re-reads a slice of the module's own
   static data and `engine_log` returns nothing when it no longer matches,
   because a wrong answer from an oracle is worse than no answer. Take that
   witness **after `run_ctors`**: a shared-memory build's data segments are
   passive, so at instantiation there is nothing placed to watch and the check
   answers `None` on every run — absent rather than wrong, which is the hard
   kind of broken to notice. `an_incoherent_memory_view_withholds_the_log`
   induces the fault through `coherence_witness()` instead of waiting for it,
   because a guard against a 1-in-4 event that is only ever exercised by that
   event is a guard nobody has seen work.
3. **Drive a full call flow**: `initVoipStack` then
   `handleIncomingSignalingOffer`, and compare the recorded
   `sendSignalingXMPP_js_sync` payloads against what whatsapp-rust emits. The
   marshalling this needs is done. What is in the way is not the payload but the
   main-thread proxy queue — see `state.rs`: the engine queues its outbound
   stanzas there and every drain fails while `register_main_thread` is off.
   `init_stress --register-main-thread` measures what turning it on costs.
4. **What corrupts memory in `examples/outgoing_call.rs`.** It ends with traps
   whichever stack the workers use, while `examples/profiler_flag.rs` — same
   engine, same log level, same assert gate — has none. `startJsWorkerThread`
   and `initSctpRingBuffer` are what remain untested between them.
5. **Non-vector embind classes**, if a module ever registers one that matters.

`_start` exiting 71 on the media modules used to head this list. It was already
fixed by the WASI memory-window bug in the table above and nothing noticed,
because the MP4 core was the one module in the lock that no test exercised.
`the_mp4_core_reads_its_arguments` now does.
