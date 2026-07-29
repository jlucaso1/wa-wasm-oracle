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

### The one instrumentation site that is above suspicion

The body of the `offer.cc:485` assert inside `make_and_cache_offer` — sixteen
bytes, `41 00 41 c9da2e 41 e1df12 41 e503 10 b642`, **unique in the module**, and
reached only when the offer fails. `11198` has two call sites and only `10425`'s
runs, so a patch here reports the failing path and nothing else. Six more bytes
follow (`41 f8a204 21 0b`, the `70008` and its store) and can be absorbed for a
longer expression, at the cost of the return value.

The mould: `41 24` (address 36), `20 00` (arg0), the loads, `36 02 00`, padded
with `01`. Everything below was read that way, two or three runs each, stable:

| read on the failing path | value |
| --- | --- |
| the key passed to the lookup | `0x6b1018`, `+8` → `"11223344556677@lid"`, len 18 |
| `ctx->[659164]`, the group | `0x8d0018` — **not null** |
| `group->[552]`, participant count | **2** |
| `participant[0]`'s jid `+8` | `"99887766554433@lid"` — self |
| `participant[1]`'s jid `+8` | `"11223344556677@lid"` — **identical to the key** |
| `participant[0]->[8]`, its state | 7 |
| `participant[1]->[8]`, its state | 2 |
| `participant[i]->[0]`, the loop's guard | non-null for both |

And the semantics, read rather than inferred:

* `f10284(a, b)` returns **1 on a match**: `if a == b return 1; r = (a==0||b==0) ?
  1 : pj_strcmp(a+8, b+8); return r == 0`.
* `f10530(ctx, key)` is not a plain lookup — it finds the participant and then
  **filters by state**: `s = *(p+8); if s <= 12 return ((5233 >> s) & 1) ? 0 : p`.
  The mask is bits {0, 4, 5, 6, 10, 12}; neither 7 nor 2 is in it.
* `f10619(group, i)` is `*(f3719(group+44, i, 127, 4))`, which confirms
  participants live at `group + 44 + i*4` — previously an assumption.
* `10535`'s loop walks indices 0 and 1 and compares both.

### What the group has nothing to do with

Those group readings all say the lookup *should* succeed, and that is the point:
it never gets far enough to use them. Reading the locals at the same site, with
a control:

| read at the failing site | value |
| --- | --- |
| control, stores `-1` | `0xffffffff` — the site runs, the address works |
| `l1`, the array `11198` was handed | `0x24bed0` |
| `l11`, i.e. `array[0]` | **0** |

And `0x24bed0` is exactly what `wa_call_start_call` was handed at entry, where
`array[0]` was `0x6b1108` and the JID beyond it was the peer's. **The array
pointer never changes; the contents of slot 0 are gone by the time the offer is
built.**

That closes the chain, and it also redeems a reading discarded earlier:
`get_participant` logs line **1382**, its null-argument branch, which is exactly
what a null key produces.

```
array[0] cleared  ->  10297 returns null  ->  10530(ctx, null)
                  ->  get_participant takes its null-argument branch (1382)
                  ->  0  ->  offer.cc:485  ->  70008
```

**The array lives on the stack.** At the moment of failure the stack pointer is
`0x24b8c0` and the array is at `0x24bed0` — 1552 bytes above it, inside the live
region, so this is not a use-after-free of popped stack. Something writes over
that slot in between. (The `l27 = SP - 16` in `10425`'s LID-consistency loop is a
temporary copy of the `{ptr, count}` pair, not the array; ruled out.)

Reading the same slot at three points narrows the window to one function:

| where | `array[0]` |
| --- | --- |
| `wa_call_start_call` entry | `0x6b1108` |
| `wa_call_start_internal` entry | `0x6b1108` |
| `make_and_cache_offer`, at the failure | **0** |

**So it is cleared inside `wa_call_start_internal`.** Instrumenting that entry
uses the same shape: the `call_lifecycle.cc:695` assert has a dead 16-byte body
at file offset 4505540, preceded by `45 04 40`; turning that into `1a 02 40`
(`drop; block`) makes the body run unconditionally, and the body becomes the
store. Check the sleb encoding against the bytecode first — `775533` is
`ed aa 2f`, and assuming `ad aa 2f` finds nothing.

The frame arithmetic says where to look. `11198` allocates 384 bytes and `10425`
allocates 1168; the stack pointer at the failure is `0x24b8c0`, and
`0x24b8c0 + 1552` is exactly the array's address. The array sits immediately
above `10425`'s frame, at `frame + 1168` — so anything writing at or past that
offset lands on it. Direct stores do not: the largest offset `10425` writes is
1164. That leaves writes through computed pointers, or a callee writing past its
own frame.

A fourth reading closes the window further. `make_and_cache_offer`'s own entry
already sees the slot at zero — measured with a control at the same site that
stores `-1` and does fire, and with `l1` reading `0x24bed0` as expected. So the
clearing happens inside `wa_call_start_internal`, **before** it calls the offer.

That entry is instrumentable the same way: the `offer.cc:409` assert (file offset
5095354, unique) is preceded by a `0d 01` at `at-3`; turning it into `1a 01`
(`drop; nop`) makes the body run every time. The function then returns 70004,
which does not matter — the store has already happened.

Two candidates are already ruled out. `memory.fill(l5+208, 0, 400)` in `10425`
has `l5 = SP - 640`, a fresh allocation below the stack pointer, so it writes
entirely below the array. And no direct `store(frame, N)` goes past 1164 against
a 1168-byte frame.

### Probing by fixed address, which is what made bisection practical

The array sits at `0x24bed0` on every run, so it can be read from *any* site,
including inside callees where no local holds it — thirteen bytes:

```
41 24  41 d0 fd 92 01  28 02 00  36 02 00
i32.const 36 ; i32.const 0x24bed0 ; i32.load ; i32.store
```

Put it in a **logging call whose message shows up in a real run**, not in a dead
assert body. The sequence `41 <file> 41 00 41 <msg> <args> 10 <logger>` runs
14–19 bytes, which is room to spare, and unlike an assert it is known to
execute. Always run the control variant (`41 24 41 7f 36 02 00`) beside it.

| where | `array[0]` |
| --- | --- |
| `wa_call_start_call` entry | `0x6b1108` |
| `wa_call_start_internal` entry | `0x6b1108` |
| `start_precall begin` log (offset 4506149) | `0x6b1108` |
| `create_p2p_transport start` log (offset 5315086) | **0** |
| `make_and_cache_offer` entry | 0 |
| the `offer.cc:485` failure | 0 |

**The slot is cleared between `start_precall begin` and
`create_p2p_transport start`** — the stretch where the log shows participants
being created and SSRCs generated.

One confounder, recorded because it produced a silent null result: these are
measured under `self_participant_probe`, and **not every site is on its path**.
The `"updating peer jid to"` log inside `10532` (offset 4581479) did not execute
there — control and probe both came back with the pre-existing `0xeeade615`. A
message appearing in an `outgoing_call` log does not mean the probe reaches it.

**Store `value + 1`, not `value`.** Address 36 is not reliably zero — an
unpatched run already has `0xeeade615` sitting there — so a probe that reads a
garbage-looking word cannot be told apart from a probe that never ran. Adding
`41 01 6a` before the store costs three bytes and makes 0 mean "did not run".

It paid for itself immediately. Probing the SSRC log site
(`call_generate_ssrc_for_participant`, offset 4667237, 18 bytes):

| run | `*(36)` | reading |
| --- | --- | --- |
| 1 | `0x6b1109` | `array[0]` is `0x6b1108` — **still populated** |
| 2 | `0xeeade615` | did not store; the site is not reached every run |

Without the `+1`, run 2 reads as "already cleared" and the window closes on the
wrong side. **The window is now between SSRC generation and
`create_p2p_transport start`.**

### It is not one instruction — the clearing is non-deterministic

Probing `"updating peer jid to"` (offset 4581479, inside
`wa_call_group_create_participant`) three times, same binary, same point:

| run | `*(36)` | reading |
| --- | --- | --- |
| 1 | `0x6b1109` | `array[0]` populated |
| 2 | `0x6b1109` | populated |
| 3 | **`0x1`** | **already cleared** |

At a fixed point in the code the slot is sometimes alive and sometimes not. That
rules out "find the store that writes zero" and reframes the whole thing: the
array is a **stack temporary** — measured at `SP + 1552` when the offer fails —
and the engine runs guest threads. **Its lifetime does not cover its use.**

It also accounts for the run-to-run spread recorded above (24 versus ~200 log
lines from the same unpatched module), and for why the group's participants are
correct while the raw array is not: those were copied.

### The leading hypothesis: guest threads share one stack

Not how the list is passed — that was checked. `Runtime::build_vector` builds the
`StringList` with the engine's own constructor and `push_back`, exactly as the JS
glue does, so it is a guest-heap object. The array at `0x24bed0` is a stack copy
`startVoipCall` makes for itself.

The suspect is `crates/oracle-core/src/threads.rs`. Guest threads here are
**separate module instances over one SharedMemory**, and `__stack_pointer` is a
**per-instance** global. The code deliberately skips `establishStackSpace`, on
the grounds that `_emscripten_thread_init` already gives the thread its stack in
this build. **If that is not true**, every thread starts from the module's
initial stack pointer and they all write over the same region.

That would account for every symptom at once: a stack temporary zeroed
non-deterministically at a fixed point, the 24-versus-200-line spread between
runs of the same module, the traps inside `startVoipCall`, and workers dying on
wild addresses.

### The 347 KB fill is not it — measured

`wa_call_group_create_participant` does zero 347 KB, and again 287 KB past that:

```rust
memory.fill(l6, 0, 347664);
memory.fill(l6 + 59400, 0, 287864);
```

Against a 64 KiB main-thread stack that looked decisive. It is not. Patched to
store its own destination instead of filling, `l6` reads **`0x960018`** on two
runs of three — a heap address, and one that has turned up before as a
participant object. The fill covers `0x960018..0x9b4f28`; the array at
`0x24bed0` is nowhere near it. This is an object being initialised in the heap,
which is what it looks like.

The third run read back the scratch word's pre-existing garbage while a control
storing `-1` at the same site fired, so the site is reached and the store works —
the site simply is not reached on every run.

`unwasm`'s watchpoint reports the same function writing at `0x24bed0` via a Fill
from a worker thread, and the two do not agree. The file offset it quotes,
4667241, holds `53 22 00 04 40 20 01 10` rather than a fill. Both wanted checking
before anything was built on them, and this is that check.

### Confirmed: every guest thread starts on the same stack

```
thread 1 stack pointer 0x24cf60
thread 2 stack pointer 0x24cf60
thread 3 stack pointer 0x24cf60
thread 4 stack pointer 0x24cf60
thread 5 stack pointer 0x24cf60
```

`0x24cf60` is the module's initial value, which is also the main thread's.
`_emscripten_thread_init` does not relocate it in this build, whatever its
documentation says — and the comment in `threads.rs` asserting that it does is
load-bearing, since it is the reason nothing sets one.

The participant array sits at `0x24bed0`, 4240 bytes below that shared top.

Reading it needs the right export name: this module exports globals
positionally, `__global_0` upward, with no `__stack_pointer`. Global 0 is the
stack pointer.

### The obvious fix does not work

Allocating 512 KiB per thread with the guest's own `malloc` and writing global 0
does give each thread a distinct region, far from the main thread's:

```
thread 1 stack 0x849088..0x8c9080     thread 4 stack 0xfa46b0..0x10246b0
thread 2 stack 0x940030..0x9c0030     thread 5 stack 0x10246b8..0x10a46b0
thread 3 stack 0x9c0038..0xa40030
```

And it breaks the run: four attempts, all 24 log lines and 11-12 traps, the
dead-run signature. Tried both before and after `__emscripten_thread_init`, with
no difference, so it is not an ordering problem. Reverted.

Going through emscripten's own entry points does not help either. This module
exports `emscripten_stack_set_limits`, `stackRestore`, `stackSave`,
`emscripten_stack_get_base/end/current/free` and `emscripten_stack_init`, so the
bounds can be set without touching the pthread struct at all — and calling
`set_limits(top, base)` followed by `stackRestore(top)` fails exactly the same
way. Four variants were tried: writing global 0 or going through those exports,
each before and after `__emscripten_thread_init`. All four give 24 lines and
11-12 traps.

**What isolates the mistake:** running the allocation *without* moving the stack
gives 202 lines, 2 × 70008 and zero traps across three runs. So `malloc` on a
thread's instance is harmless; **the relocation is what breaks it**.

Which says the approach was wrong rather than the mechanics. In emscripten's
model the guest's own `pthread_create` has already allocated this thread's
stack and recorded it in the pthread struct — that is why `establishStackSpace`
*reads* it rather than allocating. A freshly malloc'd region is a second stack
the guest knows nothing about, while its TLS and canaries still refer to the
first.

### The `+52/+56` offsets do hold here, and it still does not help

Dumping each worker's `struct pthread`:

```
thread 1 pthread 0x820030: +52=0x832350 +56=0x10000
thread 2 pthread 0x880030: +52=0x892350 +56=0x10000
thread 3 pthread 0x892370: +52=0x8a4690 +56=0x10000
thread 4 pthread 0xe00030: +52=0xe12350 +56=0x10000
thread 5 pthread 0xe12370: +52=0xe24690 +56=0x10000
```

A distinct top per thread and a size of 64 KiB — so the guest's own
`pthread_create` did allocate a stack, and the note claiming those offsets do not
hold for this module was wrong. `_emscripten_thread_init` does not install it:
it calls a four-line function that sets the TLS globals and nothing else.
Establishing the stack is the host's job, and this host does not do it.

**But those words are not there when a worker starts.** Read at the top of the
thread, before `__emscripten_thread_init`, `+52` and `+56` are zero; the values
above are read after it. Nothing in that function writes them — it sets four TLS
globals and returns — so the guest's own `pthread_create` fills them, on the
creating thread, while the new one is already running.

Two things follow. The variants that installed a stack *before* thread init were
not testing ordering: they read zeros and skipped. And a worker here can evidently
begin against a half-initialised pthread, which the baseline never has cause to
notice.

**And installing it correctly still fails.** Five variants, all 24 lines and
9-12 traps:

1. write global 0 with a malloc'd stack, before thread init
2. the same, after
3. `emscripten_stack_set_limits` + `stackRestore` with a malloc'd stack, before
4. the same, after
5. `set_limits` + `stackRestore` with **the guest's own stack from +52/+56**
6. the same, plus passing the real size (`0x10000`, from `+56`) as thread init's
   fifth argument instead of zero
7. the mirror image — moving the **main thread** to a private 4 MiB region right
   after `run_ctors`, while its stack is still shallow. The move itself succeeds
   (`Ok((0x64ecf0, 0xa4ecf0))`) and the run dies the same way

So it is not *which* stack. Relocating either side breaks this harness, which
means something in how the module is driven is incompatible with moving the
stack pointer after instantiation. Do not spend more attempts on that direction
without a new hypothesis.

**The sharpest statement, and it is a strange one.** `stackRestore` is three
instructions — `local.get 0; global.set 0`, nothing else. Calling it with the
value already in the global is **healthy**: 202 lines, no traps, three runs.
Calling it with any other value is fatal. So it is not the call, not the region,
not the bounds, not the ordering, and not TLS at the top of the stack — a
worker's stack pointer simply cannot leave the module's initial value without
the thread dying. Ten variants say so.

Aliasing is ruled out too, by the cleanest region available: growing the shared
memory and using pages nothing has ever touched, which have no other claimant at
all. Same failure. So it is not what lives at the address — **the value itself
cannot change**.

That is the question to answer before anything else here: **why must a worker's
stack pointer keep its initial value?** Eleven variants say it must, something
the workers depend on is evidently tied to it, and nothing measured so far says
what. A reasonable next suspicion is that these workers are not really executing
against their own instance's globals the way this harness assumes.

An independent implementation disagrees with this one on a structural point.
`unwasm`'s threading model — instances over one shared memory, each with its own
globals — states that **the memory and the table are shared**. `threads.rs` here
states the opposite: each instance builds its own table from the element
segments, on the grounds that they all initialise identically. That holds for
static function pointers and stops holding the moment anything is registered at
runtime.

**In this module nothing ever is.** `wasm-tools print` finds zero `table.grow`,
`table.set`, `table.fill` and `table.copy`, and the table is declared
`(table 9291 9291 funcref)` — minimum equal to maximum, so it cannot grow, and
defined rather than imported. Independently, `unwasm` models none of those
opcodes and refuses by name any it cannot model, yet decompiles all 13347
functions here without complaint. Measurement agrees: 9291 entries at every
thread's start and at the main thread's end, 9290 slots filled after the
constructors and 9290 at the end.

**How the opposite claim got its evidence is the part worth keeping.** The counts
that suggested it — "16 `table.grow`, 4185 `table.set`" — came from searching the
binary for those *byte values*. A `0x26` inside an immediate, a data segment or a
function index matches just as well. Counting bytes is not counting instructions:
use `wasm-tools print`, or a decoder that walks the code section, never a search
over the whole file.

Testing it means giving the workers the main thread's table, which wasmtime does
not make easy: a `Func` belongs to its store, so entries cannot simply be copied
across.

**The earlier discriminator, still worth keeping.**
Running `emscripten_stack_init` on a worker's instance and then
`emscripten_stack_set_limits(top, end)` with the pthread's own `+52/+56`, but
*not* `stackRestore`, gives **202 lines and 0-2 traps** — healthy. Adding the
`stackRestore` gives 24 lines and 11 traps. So the bounds are accepted and the
region is right; **it is assigning the stack pointer that breaks**, and ordering
does not save it: installing the whole thing before `__emscripten_thread_init`
and `_emscripten_tls_init` fails identically.

Two more facts from the same round. A worker instance never runs
`emscripten_stack_init`, so its bounds globals read **0/0** — nothing is checked
there by default. And running it aims them at `0x14cf60..0x24cf60`, the module's
static 64 KiB stack, which is the main thread's; that 64 KiB also matches the
`0x10000` at `+56`, which confirms those offsets really are {top, size}.

One tempting hypothesis is already eliminated: that the stack pointer is an
*imported* global and therefore shared between instances, which would explain
both the identical readings and why writing it pulls the stack out from under a
running thread. It is not. The module's 228 imports are 227 functions and one
memory; all fifteen globals are defined in the module, so they are genuinely
per-instance.

Its sibling is eliminated too: that `emscripten_stack_set_limits` keeps the
bounds in linear memory, which *is* shared, so setting them from any thread
would clobber every other thread's. It does not — the whole function is
`g8 = base; g7 = end`, two more per-instance globals. So a worker setting its own
limits cannot be reaching the main thread that way.

With the allocation but no relocation: 202 lines, zero traps, three runs. So the
relocation is what breaks it, whichever stack it installs.

**Do not re-run those five.** Diffing a 24-line log against a healthy one says
where it goes: the run dies **inside `initVoipStack`**, not on the call path. Its
last lines are media init —

```
wa_media_api.  init_audio_codecs = 0
wa_media_api.  init_media_endpt_and_codecs Exit
```

— so the switch kills the workers started during initialisation, long before an
offer is built. That is where to look.

One concrete lead. `_emscripten_thread_init` stores its fifth argument at
1268876 when both that and its third are non-zero, which makes it the default
stack size. This host calls it with `(thread_ptr, 0, 0, 1, 0, 0)`: both are zero,
so the global is never written. Emscripten's signature is `(pthread_ptr,
isMainBrowserThread, isMainRuntimeThread, canBlock, defaultStackSize,
startProfiling)`, so passing the real size — `0x10000`, from `+56` — and possibly
a non-zero third argument is worth measuring. `emscripten_stack_init` is exported
too, and may need calling on the thread's instance.

Exports worth knowing: `emscripten_stack_set_limits`, `..._get_base`,
`..._get_end`, `..._get_current`, `..._get_free`, `emscripten_stack_init`,
`stackSave`, `stackRestore`, `stackAlloc`, `pthread_self`.

Two theories died getting here, both of them mine. The JID-shape mismatch is
gone — the strings are identical, and `pj_strcmp` reads its length as an i64 at
`+8` of the `pj_str_t`, which matches the dumps. And "the key is null" was first
measured by patching shared function bodies, which was unsound; it happens to be
true, and the sound measurement is the one above.

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

## What the watchpoint route would cost

`unwasm --instrument-stores` plus `memory.watch(addr, len)` answers "who wrote
this address" with a backtrace instead of a day of bisection, and it covers
`fill`/`copy`, which matters because `memset` is the usual answer to "who zeroed
it". That is the right tool for the cleared slot. The cost was measured rather
than guessed:

* `unwasm host` on this module emits **102 methods, 51 still to implement**:
  filesystem syscalls, the thread glue (`_emscripten_thread_mailbox_await`,
  `_emscripten_notify_mailbox_postmessage`, `emscripten_receive_on_main_thread_js`,
  `_emscripten_thread_set_strongref`), `emscripten_asm_const_int/double` — embedded
  JavaScript, deliberately left as `todo!()` — `_embind_register_class_constructor`,
  and this engine's own callbacks. This repository already implements all of
  them, so it is translation rather than invention.
* **The blocker is threads.** The decompiled Rust holds memory as a `Vec<u8>` in
  one instance and has no threading model, so `__pthread_create_js` has nowhere
  to put a second instance over shared memory — and this engine needs workers to
  initialise.

That first question is cheap to answer here, and the answer closes the route:
running this harness with `ThreadPolicy::PretendSuccess` — `pthread_create`
reporting success and starting nothing — gives **zero engine-log lines and a
trap, three times**. The engine does not initialise without real workers, so a
single-instance decompilation cannot reach `startVoipCall` either.

**The watchpoint needs a threading model to be usable on this module**: shared
memory with a second instance over it, which is what this repository's
`threads.rs` does and what the generated code has no shape for. That is the ask
if this route is worth opening.

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
