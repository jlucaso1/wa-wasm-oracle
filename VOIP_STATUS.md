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

### Finding the site that actually fires

`i32.const 70008` occurs **481 times** in the module — it is a data constant as
well as an error code, so any account built on a hand-counted subset is
guessing. (An earlier draft of this file said "nine sites at lines 296, 430,
463…". That was wrong, and chasing line 430 in particular cost several rounds.)

Every site pushes the same four bytes, and neighbouring values encode to the
same 3-byte sleb — so a copy of the module can give **all 481** a distinct code
without moving anything:

```sh
# site i -> i32.const (200000 + i), then read back which one the engine reports in
#   "wa_call_start_internal, make_and_cache_offer failed: %d"
```

Exactly one comes back: **200391**, which is function **11198**, and the
bytecode there carries the line number as a literal:

```
call 10297 ; local.tee 15
call 10530 ; local.tee 21
i32.eqz ; if
  i32.const 0 ; i32.const <file> ; i32.const <func> ; i32.const 485   <- offer.cc:485
  call 8502                                                          <- log
  i32.const 70008
```

### The failure, measured end to end

The lookup is not the problem. **The key handed to it is null**, so nothing is
ever compared. And the array that key comes from is not built inside the engine
at all — it arrives as an argument:

```
wa_call_start_call
  t5 = *(params + 0)          <- the participant array
  t6 = *(params + 4)          <- the count
  |
  +- 10425  wa_call_start_internal   [25 params; l3 = p3 = t5, l4 = p4 = t6, never reassigned]
       |
       +- 11198(ctx, l3, l4, ...)   [10 params]
            if it returns non-zero:
              "wa_call_start_internal, make_and_cache_offer failed: %d"
            |
            +- 10297(*(l3)) -> NULL          <- measured
                 10297(x) = (x == NULL ? log(line 158), NULL : x->[0])
                 |
                 +- 10530(ctx, NULL) -> 10535 returns before its loop
                                     -> 0 -> offer.cc:485 -> 70008
```

**So the null is `params->[0]->[0]`: the first entry of the array the host
supplies.** The engine does not assemble it, which puts the defect close to the
host boundary — quite possibly on our side of it, in how `startVoipCall` is
called.

**The "null key" step is the weak link, and it is weak for a specific reason.**
It was measured by patching the *body* of 10297 and of 10535 — and a body patch
reports whichever call happened to run, not the one on the path being traced:

| function | call sites |
| --- | --- |
| `10284` | 58 |
| `10530` | 32 |
| `10297` | 10 |
| `10535` | 3 |
| `11198` | 2 |

Measured against a single-caller body instead — `wa_call_start_call`'s own —
`*(params+0)` is `0x24bed0`, and dumping it gives `+0 = 0x24bf3a`,
`+4 = 0x24bf2e`: **the first slot is not null.** The engine's own log agrees the
list arrived: `num_peers 1` and `ACTION start_precall with 1 peers`.

So the chain above is right about *where* it fails and wrong, or at least
unproven, about *why*. **Never instrument the body of a shared function here.**
Patch the specific call site, which the decompiled source makes findable, or a
function with one caller — and always pair it with a control variant.

**Next experiment:** instrument `11198`'s call site inside `10425` (there are
only two sites, and this is the one that runs) to capture `l3` and `l4` as that
call actually receives them.

### The caller this file used to name, and why it is the wrong one

`grep "self.f11198_"` over the decompiled module returns **two** call sites: one
in `10534`, one in `10425`. An earlier draft of this section traced the first,
and everything it concluded — that `10534` builds a 64-pointer array at
`local2+80`, that a conditional write leaves element 0 null, that `10532`
returning null is the cause — describes **a path that does not run**.

Two things settle which one does. In a healthy baseline no
`call_create_participants_*` or `wa_call_invite_*` message appears at all; break
the run and `call_create_participants_for_1_to_1_call` shows up, and that string
lives inside `10425` (which carries both the `1_to_1` and `n_way_group`
variants and picks by branch). And the `if != 0` guarding `10425`'s call emits
the exact line the log shows. `10425` is `wa_call_start_internal`.

Two smaller corrections from the same detour, worth keeping because both cost
real time:

* In the `10534` path the array write is **not** conditional. `if (l4 == 0)
  break` skips only the `*(l4+80) = 1` that follows; the write to
  `*(l2+80+l6*4)` happens either way. That was a misread `br_if` depth.
* `5233` is not a validity mask. `arg3` is 2, measured, and the guard reads
  `(both non-null) & ((1 << arg3) & 5233) == 0 || arg3 > 12)` — being *outside*
  the mask is what lets execution continue.

`oracle abi <module> --index <n>` names functions by resolving the constants
they hand their logger. Trust the `__func__` argument of an assert over a
derived name: the decompiler's own naming called `10532`
`wa_vid_quality_manager_get_vid_rate_control`, from a string it references once
in a message about a *callee* failing, while its asserts say
`wa_call_group_create_participant` repeatedly.

| index | name | file |
| --- | --- | --- |
| 11198 | `make_and_cache_offer` | `messages/senders/offer.cc` |
| 10425 | `wa_call_start_internal` | `core/call_lifecycle.cc` |
| 10532 | `wa_call_group_create_participant` | `core/call_membership.cc` |

### What this replaces

The device-vs-user JID story that used to fill this section is **dead**. The
comparison it blamed never executes. Two things kept it alive longer than they
should have:

* `10284` is a *generic* JID comparator with **57 call sites**. Instrumenting it
  measures whichever call happened last, not the one on the offer path.
* The claim "patching 10284's offsets makes the 70008 disappear" cannot be
  reconciled with the loop never running. Unhealthy runs also produce *no*
  70008, and that failure mode had already burned us once. Re-verify anything
  resting on it against the health marker before reusing it.

### Instrumentation that works

Reading a guest value at a chosen point, length-preserving:

* Replace the call/compare with `i32.const <ADDR> ; local.get N ; i32.store ;
  i32.const 1 ; nop...`.
* **Address choice is the whole trick.** `0x900000` lands in the live heap and
  is overwritten; `0xF00000` and above make the store *trap* because memory has
  not grown that far, which kills the run early and looks like "never
  executed". **36** works — low static area, writable, survives.
* Encode `value + 1` when zero is a meaningful answer, so "stored 0" and "never
  stored" stay distinguishable. Otherwise pair every measurement with a control
  variant that stores a constant.
* To free bytes for a store, swap `if` (`04 40`) for `block` (`02 40`) — same
  two bytes, and the condition's bytes become yours.

Applied to 10535, with control:

| variant | `*(36)` | reading |
| --- | --- | --- |
| control, stores -1 | `0xffffffff` | the site does execute |
| `arg0` | `0x6d0018` | the context, non-null — matches `*(u32*)1352840` |
| `arg1` | 0 | the key is null |

Two decoding traps worth remembering: the guard in 10535 is `eqz ; eqz ; or ;
eqz ; if` — there is an **extra `45`**, so it reads "both non-null", and reading
the polarity backwards sends you to the wrong branch. And engine log lines
**do not print the line number** they are given, so the absence of a particular
line in the log proves nothing.

Signatures on the path: `11198` 10 params / 1 result · `10534` 1/1 · `10530` 2/1
· `10535` 2/1 · `10297` 1/1.

### Reading the call context

`*(u32*)1352840`. The engine reaches it the same way: `getCallInfo` registers
table slot 746 (function 1108), which calls function 10386 — seven instructions
that open `i32.const 1352840 / i32.load`.

Three things worth knowing before trusting a run:

* **A run is not repeatable, so never conclude from one.** Four runs of the same
  unpatched module through `outgoing_call`:

  | run | engine-log lines | `70008` | traps |
  | --- | --- | --- | --- |
  | 1 | 24 | **0** | 11 |
  | 2 | 202 | 2 | 0 |
  | 3 | 202 | 2 | 2 |
  | 4 | 201 | 2 | 3 |

  A healthy baseline reaches ~200 lines and reports the failure **twice**. The
  short run reports *no* `70008` — not because an offer was built, but because
  nothing got that far. **The health signal is how far the log got**; "the error
  disappeared" on its own reads a dead run as a fix, which is how a wrong
  conclusion survived several rounds here. Compare only runs of comparable
  length, and run each variant three or four times.
* A healthy run has `0x6d0018` there and structured data around `1352680`. A run
  showing `0xe0c70adc` and high-entropy data died before reaching the offer, and
  the 70008 never appears in it. Note the probe (`self_participant_probe`) traps
  inside `startVoipCall` on *every* run, baseline included, so it is a memory
  reader, not a progress meter.
* Whether the engine's log is real. After an incoming offer followed by an
  outgoing call it sometimes fills with random printable bytes instead of
  messages, which reads as "the call went quiet" when it means the opposite.

## Read the module as source before disassembling anything

`unwasm` (`~/projects/unwasm`) decompiles this module into Rust:

```sh
unwasm decompile D5pLH9sfOOl.wasm -o generated.rs   # 2.4M lines, 13347 functions, ~1 min
grep -n "fn f10532" generated.rs                    # then awk to the next `pub(crate) fn f`
```

It annotates every `i32.const` with the string it addresses, which is what makes
the output readable — a bare `call 8502` says nothing, but
`f8502_voip_assert(0, "…/call_membership.cc", "wa_call_group_create_participant",
1665)` says everything. Both corrections in the section above came from reading
it; neither was visible in hours of hand-decoding, and one of them (a misread
`br_if` depth) had sent the whole investigation down a path that does not run.

Reach for it first. Disassembly is for confirming a specific byte you are about
to patch.

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

## A sibling project that does place calls, and why its recipe does not transfer

`~/projects/meowmeow-node-wasm` drives a WhatsApp Web VoIP module from Node and
successfully places calls. Its module is not this one:

|  | that build | this build |
| --- | --- | --- |
| embind surface | 82 functions | 206 functions |
| `initVoipStack` | `(str, str, str, bool, u32, u32, u32, u32)` | `(str, str, str)` |
| `startVoipCall` | 8 arguments | 7 arguments |
| peer JID in its example | `…@s.whatsapp.net` | rejected — `peer_participant_jids must be LID` |

The extra `initVoipStack` arguments are configuration: `voipParamsVersion`,
`maxParticipants`, `maxGroupSize`. This build takes none of them, and rejects
phone-number peers outright. It is the newer of the two, and configuration has
moved out of initialisation — which lines up with what it says at runtime,
where `getVoipParam("options.*")` answers empty and the incoming path reports
`Application settings not loaded`.

So the shape of the fix is not "pass more arguments". Something has to supply
settings the way a server would. `the_call_entry_points_take_the_arguments_this_build_declares`
in `tests/voip_oracle.rs` pins the signatures so a capture update cannot quietly
invalidate this comparison.

Three things borrowed from that project and measured here, none of which
changes the outcome: its JID shapes (bare LID for self, PN for the user JID, no
device suffix), the peer's PN as the fifth `startVoipCall` argument, and its
readiness wait before placing a call. Its SCTP relay bridge remains the best
reference for emulating the data path.

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
