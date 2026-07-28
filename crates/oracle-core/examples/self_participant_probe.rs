//! Why does `make_and_cache_offer` say the group has no self participant?
//!
//! It fails at `offer.cc:463`, where it requires
//! `wa_call_group_get_self_participant` — which returns `group->[592]` — to be
//! non-null. `getCallInfo` says a participant *is* marked `is_self`, so the two
//! disagree, and separating them means reading those words rather than
//! inferring them.
//!
//! That was impossible while the call context lived in an unexported global.
//! `scripts/export_globals.py` fixes that:
//!
//! ```sh
//! python3 scripts/export_globals.py caps/D5pLH9sfOOl.wasm /tmp/patched/D5pLH9sfOOl.wasm 15
//! WA_WASM_DIR=/tmp/patched cargo run --release --example self_participant_probe
//! ```
//!
//! Reads, all from the same context pointer:
//!   * `call + 48`      — the byte `reset_group_and_self_participant` returns on
//!   * `call + 659164`  — the group
//!   * `group + 592`    — the self participant the offer path demands
use oracle_core::{Catalog, Runtime, ThreadPolicy, Value};

/// Fictitious, like every identity in this repository.
const SELF: &str = "15550002222@c.us";
const SELF_DEVICE: &str = "15550002222:0@c.us";
const SELF_LID: &str = "99887766554433:0@lid";
const PEER_LID: &str = "11223344556677@lid";
const PEER_LID_DEVICE: &str = "11223344556677:0@lid";

/// Offsets established from the disassembly; see `VOIP_STATUS.md`.
const GROUP_IN_CALL: u32 = 659_164;
const SELF_IN_GROUP: u32 = 592;

/// Dumps every exported global, so the context can be found by which one moves.
///
/// `global 10` was the obvious guess — `startVoipCall` opens with
/// `global.get 10` — and it is wrong: it reads 0x18 and never changes, so it is
/// a constant, not a pointer. Finding the real one is a measurement, not a
/// reading.
fn dump_globals(runtime: &mut Runtime, when: &str) {
    let mut seen = Vec::new();
    for index in 0..15 {
        if let Ok(value) = runtime.global_i32(&format!("__global_{index}")) {
            seen.push(format!("{index}={value:#x}"));
        }
    }
    println!("{when}: {}", seen.join(" "));
}

/// Finds the call context by scanning, since it is in neither a global nor an
/// obvious static address.
///
/// The call id is a 16-character string the engine stores inside the call
/// object, so every hit is a candidate `base + k`. A candidate is accepted when
/// `base + 659164` holds something that looks like a heap pointer — that word is
/// the group, and the offer path reads the self participant out of it.
fn find_context(runtime: &mut Runtime, call_id: &str) -> Vec<(u32, u32, u32, u32, u32)> {
    const CHUNK: u32 = 1 << 20;
    const LIMIT: u32 = 64 << 20;

    let needle = call_id.as_bytes();
    let mut hits = Vec::new();
    let mut at = 0u32;
    let mut scanned = 0u64;
    while at < LIMIT {
        let Ok(block) = runtime.read(at, CHUNK) else {
            break;
        };
        scanned += block.len() as u64;
        for (offset, window) in block.windows(needle.len()).enumerate() {
            if window == needle {
                hits.push(at + offset as u32);
            }
        }
        at += CHUNK - needle.len() as u32;
    }
    println!(
        "scanned {scanned} bytes; call id found at {} address(es)",
        hits.len()
    );

    // The call id sits at some unknown offset inside the object, so walk back a
    // little from each hit and test which base yields a usable group pointer.
    let mut found = Vec::new();
    for hit in hits {
        for back in 0..4096u32 {
            let Some(base) = hit.checked_sub(back) else {
                break;
            };
            let Ok(bytes) = runtime.read(base + GROUP_IN_CALL, 4) else {
                continue;
            };
            let group = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
            // A heap pointer, not text and not a small constant.
            // Reject a group that points at unallocated memory: an all-zero
            // or ASCII-looking word there means the base is wrong, and reading
            // zeros from arbitrary memory is what makes a bogus candidate look
            // like a confirmed null.
            let structured = runtime
                .read(group, 16)
                .map(|b| b.iter().any(|&x| x != 0) && b.iter().any(|&x| !(0x20..0x7f).contains(&x)))
                .unwrap_or(false);
            if group > 0x10000 && group < 0x4000000 && group % 4 == 0 && structured {
                let Ok(bytes) = runtime.read(group + SELF_IN_GROUP, 4) else {
                    continue;
                };
                let selfp = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
                // `group[12]` decides the branch in
                // `reset_group_and_self_participant`: zero means it creates the
                // self participant, non-zero means it expects one to exist
                // already and only reads `[592]`. Measuring it says which of
                // "never created" and "created then lost" actually happened.
                let count = runtime
                    .read(group + 12, 4)
                    .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
                    .unwrap_or(u32::MAX);
                // The offset of the call id inside the object is fixed, so a
                // recurring `back` across candidates is the sign of a real
                // object rather than of a lucky read.
                found.push((base, group, selfp, count, back));
                break;
            }
        }
    }
    found
}

/// Reads the self participant out of a call context, given its base.
///
/// No validated base exists yet, which is the whole difficulty. `0x6d03b4` came
/// out of the scan below and reproduced across two runs, and it is *wrong*:
/// read directly it yields a group pointer of `0xbb4342f4`, three billion, far
/// outside an 18 MiB memory. Two runs agreeing on an address only shows the
/// allocator is deterministic enough to put the call id in the same place — it
/// does not show the address is the object.
///
/// So this takes the base as an argument, and the caller has to have earned it.
/// Ways to earn it, none done yet: derive the call id's offset from the object
/// layout rather than by walking backwards; require several fields to agree
/// with what `getCallInfo` reports at the same moment; or find where
/// `wa_call_start_internal` (10425) gets its first argument.
fn sample(runtime: &mut Runtime, base: u32, when: &str) {
    let Ok(group) = runtime
        .read(base + GROUP_IN_CALL, 4)
        .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    else {
        println!("  [{when}] group unreadable");
        return;
    };
    // Anything outside linear memory means the base is not a call context.
    if group > 0x4000000 {
        println!("  [{when}] group={group:#x} — not a pointer; base is wrong");
        return;
    }
    let selfp = runtime
        .read(group + SELF_IN_GROUP, 4)
        .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .unwrap_or(u32::MAX);
    println!("  [{when}] group={group:#x} self={selfp:#x}");
}

fn main() -> anyhow::Result<()> {
    let catalog = Catalog::discover()?;
    let bytes = std::fs::read(&catalog.resolve("D5pLH9sfOOl")?.path)?;

    let mut runtime = Runtime::instantiate(&bytes)?;
    runtime.set_thread_policy(ThreadPolicy::Spawn);
    runtime.run_ctors()?;
    runtime.attach_log_ring(4 << 20)?;

    dump_globals(&mut runtime, "before init");

    runtime.call_embind(
        "initVoipStack",
        &[
            Value::Str(SELF.into()),
            Value::Str(SELF_DEVICE.into()),
            Value::Str(SELF_LID.into()),
        ],
    )?;
    runtime.refuel();
    dump_globals(&mut runtime, "after init");

    let mark = runtime.engine_log().len();
    let outcome = runtime.call_embind(
        "startVoipCall",
        &[
            Value::Str(PEER_LID.into()),
            Value::StringList(vec![PEER_LID_DEVICE.to_owned()]),
            Value::Str("0011223344556677".into()),
            Value::Bool(false),
            Value::Str(PEER_LID.into()),
            Value::Bool(false),
            Value::Bytes(Vec::new()),
        ],
    );
    runtime.refuel();
    runtime.settle(std::time::Duration::from_secs(5));
    println!("startVoipCall -> {outcome:?}");
    dump_globals(&mut runtime, "after startVoipCall");

    println!("--- scanning for the call context ---");
    for (base, group, selfp, count, back) in find_context(&mut runtime, "0011223344556677") {
        println!(
            "  candidate call={base:#x} id@+{back} group={group:#x} group[12]={count:#x} group[592]={selfp:#x} {}",
            match (count, selfp) {
                (0, 0) => "<-- would CREATE self, yet null",
                (_, 0) => "<-- expected an existing self, found null",
                _ => "<-- self set",
            }
        );
    }
    // Same reads, but through the guard: a base whose group is outside linear
    // memory is reported as wrong rather than silently producing numbers.
    for (base, ..) in find_context(&mut runtime, "0011223344556677") {
        sample(&mut runtime, base, &format!("re-read {base:#x}"));
    }

    for line in runtime
        .engine_log_from(mark)
        .iter()
        .filter(|line| line.contains("make_and_cache_offer") || line.contains("Calling"))
    {
        println!("  | {}", line.trim());
    }

    Ok(())
}
