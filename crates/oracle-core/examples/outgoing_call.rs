//! Does the engine's outbound signaling channel work at all?
//!
//! An incoming offer is accepted and then dropped as `Missed`, and nothing
//! reaches `sendSignalingXMPP_js_sync`. That leaves two very different
//! explanations: the engine decided not to answer, or the channel itself never
//! carries anything. Originating a call separates them — an outgoing call
//! *must* put an offer on the wire, so if this produces one the channel is
//! fine and the incoming path is the problem.
//!
//! Arguments follow WhatsApp Web's own call, in `WAWeb/Voip/StackInterfaceWeb.js`:
//!
//! ```js
//! s.startVoipCall(e.toString({legacy: true}), u /* StringList */, n, r, a, i, c)
//! ```
//!
//! and `WAWeb/Voip/StartCall.js` supplies `r` as the video flag, `a` as a second
//! legacy JID, and the call id as `"00" + randomHex(16).substr(2)`. Note
//! `legacy: true` — the `@c.us` form, not `@s.whatsapp.net`.
//!
//! The participant list is the peer's *devices* — `pe(R, "callStart")` filters
//! companion devices out of a device list — and an empty one is why this used
//! to return `-1` with no log at all. Filled in, the engine starts the call:
//! `Call start, call_role 1, num_peers 1` and `ACTION start_precall`.
//!
//! Each variant gets a fresh instance: the engine keeps call state, and a
//! second start on a used one reports `init_local_state - Call context has
//! already been initialized` and fails with 70008, which answers a different
//! question.
use oracle_core::{Catalog, Runtime, ThreadPolicy, Value};

/// `WAWeb/Voip/Init.js` calls `voipInit` with three legacy-form JIDs: the
/// user, the user's device, and the user's **device LID**. The third was being
/// passed as `"{}"` — a settings blob it never was — which is what
/// `get_app_jids: wa_call_device_jid_from_string failed` and `Failed to fetch
/// typed self lid device jid` were reporting.
const SELF: &str = "15550002222@c.us";
const SELF_DEVICE: &str = "15550002222:0@c.us";
/// A LID is a separate identity namespace; the device form carries `:<device>`.
const SELF_LID: &str = "99887766554433:0@lid";
/// The peer's LID. The engine enforces LID for every call:
/// `start_precall peer_participant_jids must be LID`.
const PEER_LID: &str = "11223344556677@lid";
const PEER_LID_DEVICE: &str = "11223344556677:0@lid";
/// Sixteen hex characters, the shape WhatsApp Web generates.
const CALL_ID: &str = "0011223344556677";

fn engine(bytes: &[u8]) -> anyhow::Result<Runtime> {
    let mut runtime = Runtime::instantiate(bytes)?;
    runtime.set_thread_policy(ThreadPolicy::Spawn);
    runtime.run_ctors()?;
    runtime.attach_log_ring(4 << 20)?;

    // Unlock the engine's diagnostic logging.
    //
    // Function 8502 — the one every `file.cc:line` soft-assert goes through —
    // opens with `i32.const 1351084 / i32.load8_u / i32eqz / br_if`, so a zero
    // byte there makes it return without emitting anything. That gate is why
    // whole paths looked like they never ran: "no log line" proved nothing
    // about them. Setting it makes the engine explain itself.
    //
    // Read before writing: setting it and seeing no change proves nothing if it
    // was already set, and that mistake made the first run of this experiment
    // worthless.
    const ASSERT_LOG_ENABLE: u32 = 1_351_084;
    println!(
        "assert-log gate BEFORE any write: {:?}",
        runtime.read(ASSERT_LOG_ENABLE, 1)
    );
    runtime.write_bytes_at(ASSERT_LOG_ENABLE, &[1])?;

    // Same lesson one level up: that gate only governs 8502. Ordinary lines go
    // through a threshold instead, which admits level 3 while the lines a
    // subsystem writes on *success* are level 4. Leaving it alone makes a
    // function that completed look like a function that gave up.
    println!("engine log level was {:?}", runtime.set_engine_log_level(9));

    // Not done here, and worth knowing why: the soft-assert path needs a *second*
    // thing, an indirect call through a slot at 1351212 that nothing fills, so
    // every assert increments a counter and emits nothing. Whole exit paths in
    // `wa_call_group_create_participant` are invisible for that reason.
    //
    // Pointing it at slot 3770 — a three-argument logger, the same arity the
    // callback is invoked with — does not work: three runs came back at 94
    // engine-log lines each against a ~200-line baseline, deterministically
    // worse, with no assert text to show for it. Matching arity is not matching
    // meaning. Anything here has to be a function that treats its arguments as
    // `(file, function, line)`, and the callback also only fires for the *first*
    // assert of each severity, so it cannot enumerate exit paths anyway.

    let init_mark = runtime.engine_log().len();
    let init = runtime.call_embind(
        "initVoipStack",
        &[
            Value::Str(SELF.into()),
            Value::Str(SELF_DEVICE.into()),
            Value::Str(SELF_LID.into()),
        ],
    )?;
    runtime.refuel();
    println!("initVoipStack -> {init:?}");
    let lines = runtime.engine_log_from(init_mark);
    println!("--- init log ({} lines) ---", lines.len());
    for line in &lines {
        println!("   {}", line.trim());
    }
    println!("--- end init log ---");
    // Two calls WhatsApp Web makes at init that we never did.
    // `WAWeb/Voip/Init.js` runs `voipInit` and `setHideMyIp` together, then
    // starts network-medium monitoring, which reaches the engine through
    // `updateNetworkMedium(medium, 0)` (`StackInterfaceWeb.js:952`).
    //
    // The second one matches a complaint in our own log word for word:
    // `wa_tp.cc get network medium: unknown peer id`.
    let hide_ip = runtime.call_embind("setHideMyIp", &[Value::Bool(false)]);
    runtime.refuel();
    println!("setHideMyIp -> {hide_ip:?}");
    let medium = runtime.call_embind("updateNetworkMedium", &[Value::Int(1), Value::Int(0)]);
    runtime.refuel();
    println!("updateNetworkMedium(1, 0) -> {medium:?}");

    // `getVoipParam` reads engine config by dotted name; the only name visible
    // in the captured JS is `options.caller_timeout`
    // (`HandleNativeCallEvent.js:130`). Worth knowing whether the window works
    // at all, since it would expose configuration nothing else reaches.
    for name in ["options.caller_timeout", "options.callee_timeout"] {
        let value = runtime.call_embind("getVoipParam", &[Value::Str(name.into())]);
        runtime.refuel();
        println!("getVoipParam({name}) -> {value:?}");
    }

    println!("live threads right after init: {}", runtime.live_threads());
    // Again, after init: the first write happens before `initVoipStack`, which
    // brings the engine's own logging config up and can put the gate back.
    runtime.write_bytes_at(ASSERT_LOG_ENABLE, &[1])?;
    println!(
        "assert-log gate now: {:?}",
        runtime.read(ASSERT_LOG_ENABLE, 1)
    );
    println!();
    Ok(runtime)
}

fn main() -> anyhow::Result<()> {
    let catalog = Catalog::discover()?;
    let bytes = std::fs::read(&catalog.resolve("D5pLH9sfOOl")?.path)?;

    // "start_precall peer_participant_jids must be LID, enforce LID for all
    // calls" — the engine says it outright once `_localtime_js` is real enough
    // for it to get that far.
    // Every JID in LID form, not just the list: the engine enforces LID for
    // "all calls", and the peer arguments are JIDs too.
    // The shape that works: a bare LID for the peer argument and a *device*
    // LID in the participant list. Both LID — the engine enforces it:
    // `start_precall peer_participant_jids must be LID, enforce LID for all
    // calls`.
    // Two shapes, because `make_and_cache_offer` fails at `offer.cc:463` when
    // `wa_call_group_get_self_participant` returns null: the engine cannot find
    // *us* in the call's participant group. The list held only the peer, so the
    // second shape adds our own device LID to see whether that is what makes a
    // self participant exist.
    // The fifth argument is a *legacy-form* JID in WhatsApp Web, not a LID:
    // `StartCall.js` passes `(g ?? h).toString({legacy: true})`. We have been
    // passing the LID there, which is worth testing directly — 70008 is the
    // code the engine already uses for a JID in the form it did not want.
    const PEER_LEGACY: &str = "11223344556677@c.us";

    let shapes: [(&str, &str, &str, Vec<String>); 1] = [(
        "peer only",
        PEER_LID,
        PEER_LID,
        vec![PEER_LID_DEVICE.to_owned()],
    )];

    for (label, peer, alt_jid, devices) in shapes {
        for hold in [false] {
            let mut runtime = engine(&bytes)?;
            let mark = runtime.engine_log().len();

            // `hold`: build the vector ourselves and pass it as an already-made
            // object, which the call machinery does not release afterwards.
            // `startVoipCall` traps inside `std::vector`'s destructor (table
            // slot 375 → `free`), so the question is whether the engine has
            // taken the buffer and our release is a second free.
            let devices_arg = if hold {
                let registry = runtime.embind();
                let class_type = registry
                    .classes
                    .iter()
                    .find(|(_, class)| class.name == "StringList")
                    .map(|(type_id, _)| *type_id)
                    .expect("StringList should be registered");
                let handle = runtime
                    .build_vector(class_type, &[], &devices)
                    .expect("build StringList");
                Value::Object(handle)
            } else {
                Value::StringList(devices.clone())
            };

            // The engine exposes an SCTP ring buffer and a predicate for
            // whether it is initialised. Nothing here ever set it up, and
            // outbound data may well go through it.
            let initialised = runtime.call_embind("isSctpRingBufferInitialized", &[]);
            runtime.refuel();
            println!("   isSctpRingBufferInitialized (before) -> {initialised:?}");

            if let Ok(ptr) = runtime.malloc(1 << 20) {
                let set_up = runtime.call_embind(
                    "initSctpRingBuffer",
                    &[Value::Int(i64::from(ptr)), Value::Int(1 << 20)],
                );
                runtime.refuel();
                println!("   initSctpRingBuffer -> {set_up:?}");
                let now = runtime.call_embind("isSctpRingBufferInitialized", &[]);
                runtime.refuel();
                println!("   isSctpRingBufferInitialized (after) -> {now:?}");
            }

            // The sender is only ever reached through table slot 436, and the
            // dispatcher that invokes it sits in 437. If either is empty at
            // runtime the channel cannot carry anything, whatever the engine
            // decides.
            // Is the sender even instrumented? If something other than the
            // stub defines it, it would be called without being recorded — and
            // every "0 sent" in this investigation would be a blind spot
            // rather than a fact.
            let stubbed = runtime.stubbed_imports();
            for name in [
                "env::sendSignalingXMPP_js_sync",
                "env::on_call_event_js_sync",
                "env::call_sendto",
            ] {
                println!("   {name} stubbed(=recorded): {}", stubbed.contains(name));
            }

            for slot in [433, 434, 435, 436, 437] {
                println!(
                    "   table[{slot}] populated: {}",
                    runtime.table_entry_exists(slot)
                );
            }

            // `notifyWebP2PChannelReady(true, false)` used to be called here, on
            // the theory that the outbound channel needed to be told a transport
            // existed. It does not work that way, and the engine says so:
            // `[WebP2P] wa_call_notify_web_p2p_channel_ready failed: 70004`.
            //
            // In `StackInterfaceWeb.js` that call is made only from the
            // DataChannel state-change handler, once a *real* WebRTC channel has
            // opened, and after `initP2PVirtualAddresses`. Announcing a channel
            // that was never set up is a lie the engine is right to reject, so
            // the call is gone until there is a bridge behind it.

            // A setup step WhatsApp Web performs and we never did.
            // `WAWeb/Voip/JsWorkerThread.js` builds its wrapper out of
            // `startJsWorkerThread()` → `getJsWorkerPThreadId()` → a message
            // port, and `SctpDataChannelThread.js` is built on the same thing —
            // so this is the base of the P2P data path.
            let worker = runtime.call_embind("startJsWorkerThread", &[]);
            runtime.refuel();
            println!("   startJsWorkerThread -> {worker:?}");
            if let Ok(handle) = &worker
                && let Some(id) = handle.as_int()
            {
                let pthread = runtime.call_embind("getJsWorkerPThreadId", &[Value::Int(id)]);
                runtime.refuel();
                println!("   getJsWorkerPThreadId -> {pthread:?}");
            }

            // Before the call, for comparison: the post-failure dump shows a
            // participant with `is_self: true`, but it is read after the engine
            // has torn the call down, so on its own it cannot say whether that
            // participant existed *during* the attempt.
            let before_info = runtime.call_embind("getCallInfo", &[]);
            runtime.refuel();
            println!(
                "   getCallInfo BEFORE start -> {}",
                match &before_info {
                    Ok(value) => format!("{} chars", format!("{value:?}").len()),
                    Err(_) => "Err".to_owned(),
                }
            );

            let outcome = runtime.call_embind(
                "startVoipCall",
                &[
                    Value::Str(peer.into()),
                    devices_arg,
                    Value::Str(CALL_ID.into()),
                    Value::Bool(false),
                    Value::Str(alt_jid.into()),
                    Value::Bool(false),
                    // The last argument is the tcToken: `StartCall.js` fetches
                    // it (`getTcToken`) and passes it as `L`, which the stack
                    // interface turns into this Uint8List. We had been sending
                    // an empty one, and a WhatsApp offer carries the token.
                    Value::Bytes(vec![0xA5; 32]),
                ],
            );
            // The engine does the work on its own threads. One dying mid-call
            // would stop it just like a trap would.
            println!("   live threads before: {}", runtime.live_threads());
            // Is the trap simply the fuel running out? Traps at arbitrary
            // points are exactly what that looks like.
            println!(
                "   fuel left after the call: {:?}",
                runtime.fuel_remaining()
            );
            runtime.refuel();
            runtime.settle(std::time::Duration::from_secs(5));

            println!("   live threads after: {}", runtime.live_threads());
            for note in runtime.logs().iter().filter(|line| line.contains("thread")) {
                println!("   >>> {note}");
            }
            // `getCallInfo` takes no arguments and reads the call context
            // itself, through the same global the host cannot read: no global
            // is exported, so this is the only way to ask the engine what state
            // it believes the call is in.
            // Two more diagnostic windows, never used before. They take no
            // arguments and read the call context themselves, like getCallInfo.
            for probe in ["getShortStatisticString", "getDebugStatisticString"] {
                let out = runtime.call_embind(probe, &[]);
                runtime.refuel();
                let text = out
                    .as_ref()
                    .ok()
                    .and_then(|value| value.as_str())
                    .unwrap_or("<err>")
                    .to_owned();
                println!(
                    "   {probe} -> {} chars: {}",
                    text.len(),
                    &text[..text.len().min(400)]
                );
            }

            let info = runtime.call_embind("getCallInfo", &[]);
            runtime.refuel();
            println!("   getCallInfo -> {info:?}");

            let overflowed = runtime.engine_log_overflowed();
            let dropped = runtime
                .call_embind("getLogRingBufferOverflowCount", &[])
                .ok()
                .and_then(|value| value.as_int())
                .unwrap_or(-1);
            runtime.refuel();
            let lines = runtime.engine_log_from(mark);
            let sent = runtime.all_calls_to("env::sendSignalingXMPP_js_sync").len();
            let events = runtime.all_calls_to("env::on_call_event_js_sync").len();
            println!(
                "=== {label:22} hold={hold} -> {} | {} lines{}, {sent} sent, {events} events",
                match &outcome {
                    Ok(value) => format!("{value:?}"),
                    Err(error) => format!(
                        "trap: {}",
                        format!("{error:#}")
                            .lines()
                            .find(|line| line.contains("wasm function"))
                            .unwrap_or("?")
                            .trim()
                    ),
                },
                lines.len(),
                format!(" (overflowed={overflowed}, engine dropped {dropped})")
            );
            for line in lines.iter().take(140) {
                println!("      {}", line.trim());
            }
            // The tail is where the failure is. The head shows a call starting
            // normally every time, which is why watching only it kept this
            // looking like "the engine does nothing".
            println!("      ... last lines before it stops ...");
            for line in lines.iter().skip(lines.len().saturating_sub(40)) {
                println!("      | {}", line.trim());
            }
            // A call that is up should be endable, and ending it has to tell
            // the peer — so this is a second, independent chance for the
            // outbound channel to show it works.
            // Neither of these emits signaling either, and both *must* tell
            // the peer. The outbound channel is blocked for everything, not
            // just for the offer.
            for (name, args) in [
                ("endCall", vec![Value::Int(0), Value::Bool(false)]),
                ("rejectCall", vec![]),
            ] {
                let before = runtime.all_calls_to("env::sendSignalingXMPP_js_sync").len();
                let outcome = runtime.call_embind(name, &args);
                runtime.refuel();
                runtime.settle(std::time::Duration::from_secs(3));
                let after = runtime.all_calls_to("env::sendSignalingXMPP_js_sync").len();
                println!(
                    "   {name} -> {} | sent {} -> {after}",
                    match &outcome {
                        Ok(value) => format!("{value:?}"),
                        Err(_) => "trap".to_owned(),
                    },
                    before
                );
            }

            for call in runtime
                .all_calls_to("env::sendSignalingXMPP_js_sync")
                .iter()
                .take(2)
            {
                println!("   >>> SENT: args {:?}", call.args);
            }
            // The log ends at `Creating field_stats_manager` and the landing
            // pad calls `__resumeException`, so something throws there.
            // libc++abi writes the uncaught exception's message to stderr
            // before aborting: `libc++abi: <what()>`.
            let stderr = runtime.wasi().stderr_text();
            if !stderr.trim().is_empty() {
                println!("   GUEST STDERR: {stderr}");
            }
            println!("   --- stubs the guest actually called ---");
            for (symbol, count) in runtime.stubs_called() {
                println!("   ??? {symbol}: {count} call(s)");
            }
            for note in runtime
                .logs()
                .iter()
                .filter(|line| {
                    line.contains("C++ exception")
                        || line.contains("abort")
                        || line.contains("assertion")
                        || line.contains("unreachable")
                })
                .map(|line| {
                    line.replace('\n', " | ")
                        .chars()
                        .take(400)
                        .collect::<String>()
                })
                .rev()
                .take(4)
            {
                println!("   !!! {note}");
            }
        }
    }

    Ok(())
}
