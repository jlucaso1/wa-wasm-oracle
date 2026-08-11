//! What actually happens when the engine's log ring "fills with garbage".
//!
//! About one run in four, an incoming offer followed by an outgoing call leaves
//! the ring holding high-entropy bytes instead of messages — `"E'8da(R#"`,
//! `"8+bb=BX+"` — with `getLogRingBufferOverflowCount` still zero. That was
//! recorded as a write over a live allocation. It is not one.
//!
//! # What this establishes
//!
//! Each round brings an engine up, hands it an offer, snapshots *all* of linear
//! memory, starts a call, and snapshots again. Six facts, each reproducible:
//!
//! 1. **The whole memory changes, not the ring.** A healthy round differs in
//!    ~372 KB across ~539 spans — heap and stack churn. A bad one differs in a
//!    single span covering every byte from `0xd` to the end.
//! 2. **What the host reads is not linear memory.** Zero bytes go from 83% of
//!    the image to 3%. A 64 KiB guard block of `0xA5`, allocated either side of
//!    the ring, is *absent* — not moved, absent. A string literal in static
//!    data reads as noise, and static data is written once at instantiation.
//! 3. **The guest is fine.** `emscripten_stack_get_base` still returns
//!    `0x24cf60` at that exact moment. It reads a global and needs none of the
//!    host's marshalling, which is why it is the question worth asking.
//! 4. **No host call wrote it.** Instrumenting every host write of 64 KiB or
//!    more finds only this example's own guard fills, and the host's only
//!    entropy source — `getentropy` / `get_random_bytes_js` — is never called
//!    outside the heap or in chunks over 4 KiB.
//! 5. **The mapping did not move**, as far as wasmtime will say:
//!    `SharedMemory::data()` reports the same base pointer before and after.
//!    Note what that is worth — wasmtime freezes a shared memory's `base` when
//!    the memory is created and `grow` updates only `current_length`, so this
//!    check cannot see a move even in principle.
//! 6. **Growth is not the trigger.** Pre-growing the memory to a fixed 64 MiB
//!    at creation, so the guest never calls `grow`, does not stop it — it
//!    happened in two rounds of three.
//!
//! Together those say the host and the guest are looking at different memory,
//! while every mechanism that could explain how is ruled out above. That is
//! where this stops, and it is a long way from "something wrote key material
//! over the ring", which is what the evidence looked like before any of it was
//! measured.
//!
//! ```sh
//! cargo run --release --example ring_corruption -- [rounds]
//! ```
use base64::Engine as _;
use oracle_core::{Catalog, Runtime, ThreadPolicy, Value};
use wacore_binary::builder::NodeBuilder;
use wacore_binary::jid::Server;
use wacore_binary::{Jid, Node, marshal};

const VOIP: &str = "D5pLH9sfOOl";
const CALL_ID: &str = "0102030405060708";
const CALLER: &str = "15550001111";
const CALLEE: &str = "15550002222";
const PEER_LID: &str = "11223344556677@lid";
const PEER_LID_DEVICE: &str = "11223344556677:0@lid";
const LOG_RING: u32 = 4 << 20;
const VOIP_SETTINGS: &[u8] =
    br#"{"encode":{"use_mlow_codec_v1":"false"},"options":{"enable_48khz_rtp_clock":"false"}}"#;

/// Bytes the engine keeps at the front of the ring before the text starts.
const RING_HEADER: u32 = 24;

/// Size of each guard block, and the byte they are filled with.
const GUARD: u32 = 1 << 16;
const GUARD_BYTE: u8 = 0xA5;

fn jid(user: &str) -> Jid {
    Jid::new(user, Server::Pn)
}

fn offer_stanza(now: u64) -> Node {
    let caller = jid(CALLER);
    NodeBuilder::new("call")
        .attr("from", caller.clone())
        .attr("call-id", CALL_ID)
        .attr("call-creator", caller)
        .attr("t", now.to_string())
        .children([
            NodeBuilder::new("offer")
                .children([
                    NodeBuilder::new("audio")
                        .attr("enc", "opus")
                        .attr("rate", "16000")
                        .build(),
                    NodeBuilder::new("net").attr("medium", "3").build(),
                    NodeBuilder::new("encopt").attr("keygen", "2").build(),
                ])
                .build(),
            NodeBuilder::new("voip_settings")
                .attr("uncompressed", "1")
                .bytes(VOIP_SETTINGS.to_vec())
                .build(),
        ])
        .build()
}

/// What the ring held at one moment, and the whole of memory around it.
struct Snapshot {
    /// Where the host thinks the guest's memory starts, at the moment of the
    /// snapshot.
    base_ptr: Option<usize>,
    header: Vec<u8>,
    text: Vec<u8>,
    /// Every byte of linear memory. The ring is only where the damage was
    /// *noticed*; the write that did it starts wherever it starts, and a
    /// window the size of the ring cannot see that.
    memory: Vec<u8>,
}

impl Snapshot {
    /// How much of the text looks like engine messages rather than noise.
    ///
    /// A real entry carries a source tag; noise is printable by accident. This
    /// counts printable runs and how many of them are shaped like a message.
    fn structured(&self) -> (usize, usize) {
        let lines = runs(&self.text);
        let structured = lines
            .iter()
            .filter(|line| line.contains(".c") || line.contains("EVENT") || line.contains("call"))
            .count();
        (lines.len(), structured)
    }
}

fn runs(raw: &[u8]) -> Vec<String> {
    const MIN: usize = 8;
    let mut out = Vec::new();
    let mut current = String::new();
    for &byte in raw {
        if byte.is_ascii_graphic() || byte == b' ' {
            current.push(byte as char);
        } else if current.len() >= MIN {
            out.push(std::mem::take(&mut current));
        } else {
            current.clear();
        }
    }
    if current.len() >= MIN {
        out.push(current);
    }
    out
}

fn snapshot(runtime: &Runtime, base: u32, bytes: u32) -> Snapshot {
    let header = runtime.read(base, RING_HEADER).unwrap_or_default();
    let text = runtime
        .read(base + RING_HEADER, bytes - RING_HEADER)
        .unwrap_or_default();
    let size = u32::try_from(runtime.memory_size()).unwrap_or(u32::MAX);
    let memory = runtime.read(0, size).unwrap_or_default();
    Snapshot {
        base_ptr: runtime.memory_base(),
        header,
        text,
        memory,
    }
}

/// Every contiguous span of memory that changed between two snapshots.
///
/// Spans separated by fewer than `JOIN` identical bytes are reported as one:
/// a single write leaves gaps wherever it happened to store a byte that was
/// already there, and splitting on those turns one write into hundreds.
fn changed_spans(before: &[u8], after: &[u8]) -> Vec<(usize, usize)> {
    const JOIN: usize = 64;

    let mut spans: Vec<(usize, usize)> = Vec::new();
    for (at, (a, b)) in before.iter().zip(after).enumerate() {
        if a == b {
            continue;
        }
        match spans.last_mut() {
            Some(last) if at - last.1 <= JOIN => last.1 = at,
            _ => spans.push((at, at)),
        }
    }
    spans
}

/// The span of `after` that differs from `before`, and whether it destroyed
/// text that had already been written.
fn damage(before: &Snapshot, after: &Snapshot) -> Option<(usize, usize, usize)> {
    let first = before
        .text
        .iter()
        .zip(&after.text)
        .position(|(a, b)| a != b)?;
    let last = before
        .text
        .iter()
        .zip(&after.text)
        .rposition(|(a, b)| a != b)?;
    // How much of the changed span used to be *written* text rather than the
    // untouched zeroes past the cursor. Only that part is a wild write; the
    // rest is the engine appending, which is its job.
    let clobbered = before.text[first..=last]
        .iter()
        .filter(|byte| **byte != 0)
        .count();
    Some((first, last, clobbered))
}

fn engine(bytes: &[u8]) -> Option<(Runtime, u32, u32)> {
    for _ in 0..6 {
        let mut runtime = Runtime::instantiate(bytes).expect("instantiate");
        runtime.set_thread_policy(ThreadPolicy::Spawn);
        runtime.run_ctors().expect("ctors");

        // A guard block on each side of the ring, filled with a byte nothing
        // else writes. If the ring is destroyed and both guards survive, the
        // write was exactly ring-sized and ring-aligned — which is not what a
        // stray overflow looks like, it is what a second owner filling an
        // allocation it was handed looks like. If the guards go too, it is a
        // wild write and its extent is measurable.
        let low = runtime.malloc(GUARD).expect("low guard");
        runtime.attach_log_ring(LOG_RING).expect("log ring");
        let high = runtime.malloc(GUARD).expect("high guard");
        for guard in [low, high] {
            runtime
                .write_bytes_at(guard, &vec![GUARD_BYTE; GUARD as usize])
                .expect("fill guard");
        }

        // The identity `engine_with_identity` in `tests/signaling.rs` uses, and
        // it matters: with the placeholder arguments the outgoing call gives up
        // before logging anything, and a call that never runs cannot corrupt
        // anything either.
        let init = runtime.call_embind(
            "initVoipStack",
            &[
                Value::Str(format!("{CALLEE}@c.us")),
                Value::Str(format!("{CALLEE}:0@c.us")),
                Value::Str("99887766554433:0@lid".to_owned()),
            ],
        );
        runtime.refuel();
        if init.as_ref().ok().and_then(|value| value.as_int()) == Some(0) {
            runtime.settle(std::time::Duration::from_secs(2));
            return Some((runtime, low, high));
        }
    }
    None
}

fn round(bytes: &[u8], index: usize) -> bool {
    let Some((mut runtime, low, high)) = engine(bytes) else {
        println!("{index:>3}: engine did not come up");
        return true;
    };
    let (base, size) = runtime.log_ring().expect("ring attached");

    let stanza = {
        let node = offer_stanza(runtime.virtual_unix_time());
        let encoded = marshal::marshal(&node).expect("marshal");
        base64::engine::general_purpose::STANDARD.encode(&encoded[..])
    };
    let _ = runtime.call_embind(
        "handleIncomingSignalingOffer",
        &[
            Value::Str(stanza),
            Value::Str("web".to_owned()),
            Value::Str("2.3000.0".to_owned()),
            Value::Str(runtime.virtual_unix_time().to_string()),
            Value::Str(runtime.virtual_unix_time().to_string()),
            Value::Bool(false),
            Value::Bool(false),
            Value::Str(jid(CALLER).to_string()),
            Value::Bytes(Vec::new()),
        ],
    );
    runtime.refuel();
    runtime.settle(std::time::Duration::from_secs(3));

    // The state to protect: everything the ring holds with the offer handled
    // and before a call is started.
    let before = snapshot(&runtime, base, size);
    let (before_lines, before_structured) = before.structured();

    let outcome = runtime.call_embind(
        "startVoipCall",
        &[
            Value::Str(PEER_LID.to_owned()),
            Value::StringList(vec![PEER_LID_DEVICE.to_owned()]),
            Value::Str("0011223344556677".to_owned()),
            Value::Bool(false),
            Value::Str(PEER_LID.to_owned()),
            Value::Bool(false),
            Value::Bytes(Vec::new()),
        ],
    );
    println!(
        "     startVoipCall -> {}",
        match &outcome {
            Ok(value) => format!("{value:?}"),
            Err(error) => format!("{error:#}")
                .lines()
                .next()
                .unwrap_or("?")
                .to_owned(),
        }
    );
    runtime.refuel();
    // Before settling: whether the damage is already there when the call
    // returns says whether it was this thread or a worker that did it.
    let on_return = snapshot(&runtime, base, size).structured();
    runtime.settle(std::time::Duration::from_secs(5));

    let after = snapshot(&runtime, base, size);
    let (after_lines, after_structured) = after.structured();
    let healthy = after_structured > 0;

    println!(
        "{index:>3}: ring {base:#x}+{size:#x} | memory {:#x} -> {:#x} | before {before_lines} \
         lines/{before_structured} structured | after {after_lines}/{after_structured} | {}",
        before.memory.len(),
        after.memory.len(),
        if healthy { "ok" } else { "CORRUPT" }
    );

    // The control this instrument needs: a healthy round's diff. Without it,
    // "the whole memory changed" says nothing — it could be what every round
    // looks like.
    let spans = changed_spans(&before.memory, &after.memory);
    let changed: usize = spans.iter().map(|(a, b)| b - a + 1).sum();
    println!(
        "     {} changed spans, {changed} bytes, first {}",
        spans.len(),
        spans
            .first()
            .map(|(a, _)| format!("{a:#x}"))
            .unwrap_or_else(|| "none".to_owned())
    );

    println!(
        "     host memory base: {:x?} -> {:x?}{}",
        before.base_ptr,
        after.base_ptr,
        if before.base_ptr == after.base_ptr {
            ""
        } else {
            "   <- THE MAPPING MOVED"
        }
    );
    // Is the content *gone*, or just somewhere else? The low guard is 64 KiB
    // of a byte nothing else writes, so finding it at a different offset would
    // mean the mapping shifted rather than that anything was overwritten.
    let needle = vec![GUARD_BYTE; 4096];
    let find = |image: &[u8]| {
        image
            .windows(needle.len())
            .position(|window| window == needle.as_slice())
    };
    println!(
        "     64 KiB guard pattern: before at {:?}, after at {:?} (guard was allocated at {low:#x})",
        find(&before.memory).map(|at| format!("{at:#x}")),
        find(&after.memory).map(|at| format!("{at:#x}")),
    );

    if !healthy {
        println!(
            "     when the call returned: {} lines/{} structured",
            on_return.0, on_return.1
        );
        // Is the allocator still holding the block for us? If a fresh request
        // of the same size comes back at the same address, the block was freed
        // — and everything above is a second owner filling what it was handed,
        // not a wild write at all.
        match runtime.malloc(size) {
            Ok(again) if again == base => {
                println!("     malloc({size:#x}) returned {again:#x} AGAIN — the block was freed")
            }
            Ok(again) => {
                println!("     malloc({size:#x}) -> {again:#x}, so the block is still ours")
            }
            Err(error) => println!("     malloc({size:#x}) failed: {error}"),
        }
        // Where the write really begins. The ring is at the very bottom of the
        // heap, so a write that starts below it starts in the stack or in
        // static data — and that names the buffer far better than "the ring
        // is full of noise" does.
        for (start, end) in spans.iter().take(8) {
            println!(
                "       {:#x}..={:#x} ({} bytes){}",
                start,
                end,
                end - start + 1,
                if (*start as u32) < base {
                    "  <- BELOW the ring"
                } else {
                    ""
                }
            );
        }
        // The cleanest question available: an export that reads a *global* and
        // touches no memory, so it needs none of the host's marshalling. If it
        // still answers `0x24cf60` the guest is alive and executing, and the
        // damage is in how the host sees its memory rather than in the memory.
        println!(
            "     emscripten_stack_get_base -> {:x?}",
            runtime
                .call("emscripten_stack_get_base", &[])
                .map(|values| format!("{:x?}", values.first()))
        );
        runtime.refuel();
        // How much of the image is zero. A real 10 MiB linear memory is mostly
        // zeroes; an image with almost none is not linear memory at all.
        let zeroes = |image: &[u8]| {
            let count = image.iter().filter(|byte| **byte == 0).count();
            format!("{}%", count * 100 / image.len().max(1))
        };
        println!(
            "     zero bytes: before {}, after {}",
            zeroes(&before.memory),
            zeroes(&after.memory)
        );

        // Is the *guest* damaged, or only this view of it?
        //
        // Static string data cannot change: the module's data segments are
        // written once at instantiation and nothing writes over a string
        // literal. If `332628` no longer reads as its constant, either the
        // whole memory is gone or the host is looking at the wrong bytes — and
        // an embind call that still answers correctly settles which.
        println!(
            "     static string at 332628: {:?}",
            runtime
                .read_cstr(332_628)
                .map(|text| text.chars().take(48).collect::<String>())
        );
        runtime.refuel();
        println!(
            "     getWebP2PVirtualIpv4 -> {:?}",
            runtime
                .call_embind("getWebP2PVirtualIpv4", &[])
                .map(|value| value.as_str().map(str::to_owned))
        );
        runtime.refuel();

        for (name, guard) in [("low", low), ("high", high)] {
            let intact = runtime
                .read(guard, GUARD)
                .map(|bytes| bytes.iter().all(|byte| *byte == GUARD_BYTE))
                .unwrap_or(false);
            println!(
                "     {name} guard at {guard:#x}: {}",
                if intact { "intact" } else { "DESTROYED" }
            );
        }
        println!("     header before {:02x?}", &before.header);
        println!("     header after  {:02x?}", &after.header);
        match damage(&before, &after) {
            Some((first, last, clobbered)) => {
                println!(
                    "     changed {:#x}..={:#x} ({} bytes), {clobbered} of which held text",
                    base + RING_HEADER + first as u32,
                    base + RING_HEADER + last as u32,
                    last - first + 1,
                );
                let window = &after.text[first..(first + 64).min(after.text.len())];
                println!("     at the start: {window:02x?}");
                println!(
                    "     it replaced:  {:02x?}",
                    &before.text[first..(first + 64).min(before.text.len())]
                );
            }
            None => println!("     the text is byte-identical, so the damage is elsewhere"),
        }
    }
    healthy
}

fn main() -> anyhow::Result<()> {
    let rounds: usize = std::env::args()
        .nth(1)
        .and_then(|arg| arg.parse().ok())
        .unwrap_or(6);

    let catalog = Catalog::discover()?;
    let entry = catalog.resolve(VOIP)?;
    let bytes = std::fs::read(&entry.path)?;

    let mut healthy = 0usize;
    for index in 1..=rounds {
        if round(&bytes, index) {
            healthy += 1;
        }
    }
    println!("{healthy}/{rounds} rounds left the ring intact");
    Ok(())
}
