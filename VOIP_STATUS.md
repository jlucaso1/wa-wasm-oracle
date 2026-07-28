# Driving the VoIP engine: where it stops, and what has been ruled out

The engine (`D5pLH9sfOOl`) comes up, negotiates media, and reaches the state a
ringing outbound call is in — and then stops without putting an offer on the
wire. This is what is known about why, so the next attempt starts from evidence
rather than from the reading that occupied most of the last one.

## Where it stops

`startVoipCall` reaches `[None -> Calling]` with SSRCs generated for audio,
video and screen-share on both participants, and then:

```
wa_call_tr  create_p2p_transport start
core/call_  wa_call_start_internal, make_and_cache_offer failed: 70008
events/ev   EVENT: Call offer send failed
```

**The offer is never built.** Nothing reaches `sendSignalingXMPP_js_sync`
because nothing was ever constructed to send. Any account of this that starts
from "the outbound channel is blocked" is wrong — that reading survived a long
time and cost accordingly.

`make_and_cache_offer` is function 11198 (`offer.cc`). At line 463 it requires
`wa_call_group_get_self_participant` to be non-null; that returns
`group->[592]`, and the pointer is null.

The participant itself is *not* missing. `getCallInfo` reports:

```json
"participant_count": 2,
"participants": [
  { "is_self": false, "jid": "…@lid", "state": 2, "order_id": 2 },
  { "is_self": true,  "jid": "…@lid", "state": 7, "order_id": 0 }
],
"self_participant_uuid": ""
```

So the open question is narrow: **what fills the group's cached self pointer,
given that the self participant already exists in the list.**

## The one tool to reach for first

```rust
runtime.call_embind("getCallInfo", &[])
```

It takes no arguments, reads the call context itself, and returns the engine's
entire view of the call as JSON — state, result, both participants, flags. It
is empty before a call and populated after, so it can also distinguish state
built by a call from state that was already there.

This matters because the module **exports no globals**, so the host cannot read
the call context directly; the context arrives in guest code through
`global.get 10`. `getCallInfo` is the whole of the available observability, and
reaching for it earlier would have replaced days of disassembly.

## Ruled out by measurement — do not retry these

Each was implemented or configured, measured, and reverted.

| Hypothesis | What happened |
| --- | --- |
| Per-instance function table diverging between threads | Table is `9291..9291`, non-growable, identical in every instance |
| `establishStackSpace` on the host (pthread `+52`/`+56`) | Much worse: workers went from 1 returning to all five stopping on wild addresses |
| Registering the main thread (`thread_id == 0`) | Drains stop failing, but `initVoipStack` starts trapping |
| Draining the proxy queue from inside `host_func` | Worse still — startup fails a round earlier |
| `can_block = 0` on workers | 7 of 12 threading tests fail; only the main thread cannot block |
| Turning the guest scheduler off | Reaching `Calling` went from 5/6 to 0/6 |
| Adding our own device LID to the participant list | Identical failure |
| Legacy-form JID as the fifth `startVoipCall` argument | Identical failure |
| Registering a video renderer | No such embind API exists; the video imports are never called |
| AB props for logging | No change in verbosity |

## The architectural gap

WhatsApp Web does not let the wasm engine gather ICE candidates. The browser's
WebRTC stack does that, and JavaScript hands the result back. From
`StackInterfaceWeb.js` and `HandleNativeCallEvent.js`:

1. the server sends the relay list, the **engine** raises it as an event, and JS
   caches it — with no cached list, `initP2PConnectionIfEnabled` gives up;
2. JS builds `stun:` URLs from it and calls
   `initP2PConnection(isCaller, iceServers, callback)`;
3. JS reads `getWebP2PVirtualIpv4` / `…Ipv6` / `…Port` from the engine and sets
   up the virtual addresses on its own side;
4. inbound P2P data enters the engine through
   `handleOnMessageFromHeap(ptr, len, callId, …)`;
5. **only once a real DataChannel opens** does JS call
   `notifyWebP2PChannelReady(true, false)`;
6. the resulting transport goes back via `sendWebP2PTransport(callId, …)`.

Calling `notifyWebP2PChannelReady` without a bridge behind it is a claim the
engine is right to reject — it answers `70004`. A bidirectional call here needs
that bridge emulated: a loopback data channel, `handleOnMessageFromHeap` fed,
and a relay list supplied.

## Reading the engine's state directly

Most of what is above was inferred from disassembly, and inference has a poor
record here. Two pieces of instrumentation now make parts of it measurable.

**Exported globals.** `scripts/export_globals.py` adds an export for every
global to a *copy* of a module, so `Runtime::global_i32` can read them:

```sh
python3 scripts/export_globals.py caps/D5pLH9sfOOl.wasm /tmp/patched/D5pLH9sfOOl.wasm 15
WA_WASM_DIR=/tmp/patched cargo run --release --example self_participant_probe
```

Only the export section is rewritten; everything else is copied byte for byte.

What that measured, and it is worth knowing before reading `global.get N` as
anything: `global 0` is the stack pointer (the only one that moves), `7` and `8`
are the stack bounds, `10`–`14` are small constants — `global 10` is `0x18`, not
a pointer, despite `startVoipCall` opening with `global.get 10`. **No global
holds the call context.**

**Scanning for the context.** `examples/self_participant_probe.rs` searches
memory for the call id and walks back to a base whose `+659164` looks like a
heap pointer. Within a single run this is self-consistent and reports
`group[592] = 0` — the null the offer path rejects.

It is *not* validated, and the failure mode is instructive: a base that
reproduced across two runs still turned out to hold garbage on a third, because
two runs agreeing on an address only shows the allocator is deterministic enough
to put the call id in the same place. Scan per run; never hardcode a base. To
make it trustworthy, one of: derive the call id's offset from the object layout,
require several fields to agree with `getCallInfo` at the same moment, or find
where `wa_call_start_internal` gets its first argument.

## Working notes

- The engine's `file.cc:line` soft-asserts all pass through function 8502, which
  is gated on a byte at `1351084`. Setting it to 1 is *not* sufficient to make
  them appear, so their absence proves nothing about whether a path ran.
- `wa_call_group_create_participant`'s fourth argument is the participant state:
  **7 is self, 2 is peer** — matching the `state` field in `getCallInfo`.
- Static reading of this module has a poor record: eight hypotheses drawn from
  disassembly were refuted by measurement in a single session, twice because an
  unverified "and therefore this code runs" slipped into the chain. `callers`
  answers *who calls*, never *what executed*.
- When checking whether a fix worked, do not grep only for the old failure
  message. A change that merely renamed the failure once read as a success.
