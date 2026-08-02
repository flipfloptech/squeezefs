# 2026-08-04 — SDK campaign, design phase + Tier-1 direct-link enablement

Branch `feat/sdk-design` (off dev tip `68e8474`, **unmerged — the
orchestrator merges**). User intent (2026-08-02, verbatim): *"build an
application with 100% squeezefs support for the absolute fastest/safest
access"* — link-time integration instead of LD_PRELOAD, up to a
first-class SDK. Two deliverables: **Tier 1** (direct-link support for
the existing shim — shipped here) and **the SDK design doc**
(`docs/design-sdk.md` — the program charter; implementation is the
follow-on `feat/sdk-…` ladder, NOT this campaign).

Box constraint honored: a kernel build owned cores 0–7 for most of the
session (two campaigns' compiles frozen); all cargo work ran
`taskset -c 8-15 nice -n 10 CARGO_BUILD_JOBS=8`, minimal (the Tier-1
crate suite + the preload gate), queued behind the freezer where it was
stopped. Docs were the bulk by design.

## 1. Tier 1 — direct-link (`-lsqueezefs_il`) support: what shipped

### 1.1 The constructor-ordering audit (the charter's first question)

Verdict: **already safe, by design — with two real gaps fixed.**

* The shim has **no constructors at all**: the §5.4 posture ("no ctor
  ordering games; first call initializes", `interpose.rs` module doc)
  means every global is a lazily-initialized `OnceLock`/atomic behind
  the first interposer call; `dlsym(RTLD_NEXT)` chaining is
  position-relative and libc is always behind the shim in the search
  order (it is both the app's implicit `-lc` tail and the shim's own
  DT_NEEDED); the TLS reentrancy guard uses `try_with` (TLS-unavailable
  phases ⇒ real call); `pthread_atfork` registration is lazy
  (`atfork_init` from the open-family tail). A DT_NEEDED load therefore
  differs from LD_PRELOAD in exactly one relevant way — OTHER objects'
  constructors / C++ static initializers may call interposed symbols
  before `main` — and the lazy-init design already serves that window.
  Pinned, not assumed: the gate's linked harness does its I/O round
  trip **inside** `__attribute__((constructor))` (leg 1e).
* **Gap 1 — no SONAME** (fixed, `build.rs`
  `cargo:rustc-cdylib-link-arg=-Wl,-soname,libsqueezefs_il.so`): rustc
  sets no cdylib SONAME, so linking the shim *by path* embedded the
  build-tree path as the consumer's DT_NEEDED entry. Asserted by the
  gate (`readelf -d` SONAME row).
* **Gap 2 — unload safety** (fixed, `-Wl,-z,nodelete`): atfork
  handlers can never be unregistered, so any load of this object must
  be permanent. LD_PRELOAD/DT_NEEDED never unload; `-z nodelete` makes
  the `dlopen` edge structural (dlclose = no-op). Asserted by the gate
  (`readelf -d` NODELETE flag row).
* Pathological link orders degrade safe: `-lc` listed *before* the shim
  puts libc ahead in the global scope — the interposers simply never
  engage (fully inert shim, kernel FUSE serves; no crash class). The
  real operational trap is `--as-needed` (distro default) dropping the
  DT_NEEDED entry when the app references no interposed symbol at
  static-link time — silent no-interception. Documented
  (operations.md: wrap in `-Wl,--no-as-needed … -Wl,--as-needed`);
  the detection line is the run-time tell.

### 1.2 Linked-mode detection line (the bootstrap path)

`session::load_mode_from` (pure, unit-pinned) classifies the
`LD_PRELOAD` value: basename-prefix match on `libsqueezefs_il`,
colon/space list splitting, near-miss names excluded. On the first
bootstrap-blob decode (= first contact with an interception-armed
mount) a linked shim prints once per process:

```
squeezefs-il: active via direct link (DT_NEEDED), not LD_PRELOAD — same KD-7 build pairing applies
```

Exactly-once + engagement pinned by gate leg 2b-linked. KD-7 unchanged
in linked mode (same-commit pairing; refusal ⇒ passthrough + the
reason line). Red-first: contracts committed failing
(`crates/squeezefs-preload/tests/linked_mode_tests.rs`, E0432 red
proof), implementation followed.

### 1.3 Gate battery rows added (`tests/run_preload_gate.sh`)

* **Leg 1e (unprivileged)**: SONAME/NODELETE readelf asserts; a
  cc-compiled harness linked `-Wl,--no-as-needed <shim>` run **without
  LD_PRELOAD**: ctor-context I/O round trip (the ordering pin),
  `dladdr` scope-occupancy proof (`pread64` must resolve into the shim
  — the root-free engagement tell, since passthrough is invisible by
  design), DT_NEEDED presence (`ldd`), 8 MiB pwrite/pread byte parity.
* **Leg 2b-linked (root)**: the same harness on the armed mount —
  `ipc_ops_{read,write}` deltas ≥ 128 ops each (charter §3 rule 4) +
  the detection line exactly once.

### 1.4 Verification (this session; instrument: the crate suite + gate leg 1, devbox, cores 8-15 @ nice 10)

| Check | Result |
|---|---|
| Red proof (contracts vs unimplemented tree) | E0432 `no load_mode_from in session` — committed red (`d38cd23`) |
| `cargo test -p squeezefs-preload --profile preload-release --features interposers` | PASS (all suites; the 8 new linked-mode contracts 8/8) |
| `cargo clippy -p squeezefs-preload --profile preload-release --features interposers --all-targets -- -D warnings` | clean |
| `cargo fmt -p squeezefs-preload --check` | clean |
| `readelf -d libsqueezefs_il.so` | `SONAME [libsqueezefs_il.so]`; `FLAGS_1: NOW NODELETE`; NEEDED order libc after the shim |
| Preload gate **leg 1** (build + Issue-4 guard proof + passthrough battery + libaio lifecycle ×3 + **new leg 1e**) | **PASS** — `OK: direct-link battery (SONAME/NODELETE, ctor-order, scope occupancy, parity)` |
| Preload gate **leg 2** (root; incl. the new 2b-linked engagement + exactly-once line) | RUN RESULT RECORDED BELOW |

Leg-2 run history (multi-run discipline — counts restart post-fix):

* **Run 1: ABORTED at 2b, attributed — instrument pairing, not a
  product bug.** The shim was built mid-campaign (`39fb4f9-dirty`),
  the daemon after the docs commits (`2da86e0`): KD-7 refused the pair
  (`session refused: build mismatch (shim 39fb4f9…-dirty, daemon
  2da86e0…)` — the reason-bearing line naming both commits, exactly as
  designed; commit-equality refusal is dev-override-proof by design)
  and the engagement assert correctly failed the run. The gate did its
  job twice over: silent passthrough could not masquerade as
  interception, and the refusal line named the cause.
* **Run 2 (from zero, same-commit pair at `2da86e0`, clean tree — no
  dev override needed): PASS, both legs.** The new row:
  `OK: direct-link engagement (reads +128, writes +128) + detection line`
  — engagement EXACT (the harness issues exactly 128 pwrites + 128
  preads; every op rode the ring) and the linked-mode line printed
  exactly once. Every pre-existing row green (notify delivery, mount
  parity/engagement, dup, close_range-reuse, lseek pin 13/10k,
  offsetful parity, fio/elbencho verify, fio libaio Δ8192/Δ8192,
  foreign-netns rendezvous, kill-9 soak ×5 zero residue,
  fork-then-kill-parent, aio lifecycle ×3 on armed +
  establish-refused shapes, direct-drive kill-9 soak +15360 serves) —
  no regression from the Tier-1 diff. Instrument: devbox, cores 8-15
  @ nice 10, 2.2 GHz thermal-governor cap, /dev/shm-backed gate
  volumes (the gate's own substrate).

Root .rs untouched (change surface: `crates/squeezefs-preload/{build.rs,
src/session.rs,src/interpose.rs,tests/}`, `tests/run_preload_gate.sh`,
docs) — the change-class gate is the preload suites + the gate script;
root clippy/fmt not triggered by this diff.

## 2. The design doc (`docs/design-sdk.md`) — keyed-decision summary

| # | Decision | One-line resolution |
|---|---|---|
| SKD-1 | API surface | Rust crate + `libsqueezefs` C ABI (`sqz_*`); session client factored into `squeezefs-ipc::client` (one implementation, shim + SDK frontends); explicit handles, positional-only ops, io_uring-shaped `sqz_submit`/`sqz_reap` mapping 1:1 onto slots + the IPC_ABI-2 completion doorbell; sync wrappers = submit(1)+inline-reap |
| SKD-2 | **The buffer-lease law** (the hard part) | Arena-native `sqz_buf_*`: APP_OWNED → SUBMITTED → APP_OWNED; free-while-inflight structurally refused (-EBUSY), poisoned custody never recycled (the §5.4.1 never-reuse law verbatim); write-while-submitted priced at exactly the `write(2)` torn-content bar — **safe because §5.3.1 already hardens the daemon against hostile mid-serve arena mutation** (snapshot-then-validate; derived values from severed copies); §5.2 boundary untouched: sealed per-session memfd, fd screen + SO_PEERCRED still THE boundary, an SDK app endangers only its own session. Rust surface lifts the law to compile time (by-value submit). Daemon sever/merge NOT deleted (census-declared load-bearing) |
| SKD-3 | Fallback ladder | Every rung lands on plain POSIX on the real mount fd, semantics identical (daemon absent / unarmed / version window / budget / mid-flight death / per-op ineligibility); explicit `sqz_fs_mode[_reason]`, never silent; engagement truth stays daemon-side `ipc_ops_*` |
| SKD-4 | Versioning | HELLO gains `client_kind` + `sdk_abi` (IPC_ABI 3); SDK window = {current, previous} release train over a minimal frozen subset (header/slot/ring/doorbell layouts + 5 ctl verbs); shim keeps same-commit KD-7 verbatim; refusal-is-safe makes the window a performance promise, not a correctness one |
| SKD-5 | Scope fence | No metadata bypass, no client device access/keys/tokens, no multi-daemon, no cross-host, no mmap/`FILE*`, C-only bindings v1, no new durability semantics before Idea 8 |
| SKD-6 | Durability classes | Per-open strengthen-only (`SQZ_D_MOUNT`/`SQZ_D_SYNC`) composing with rewrite-program §9.3 Idea-8 mount classes ("toward durability, never away"); SDK-4 blocks on Idea 8 |

## 3. The priced win table (the reviewer's numbers — field ledgers, not new runs)

Basis: `.benchmarks/2026-08-02-read-copy-count.md` (§3.2, §4, §7, §9)
+ `.benchmarks/2026-07-31-near-zero-copy.md`. No new measurements this
campaign (design phase); G-S2 is the counted adjudication.

| Row (field client, nvme-tcp nullblk) | Today (il shim) | SDK arena-native target | Deleted term |
|---|---|---|---|
| Write 1 MiB stream (wri il 25.69 GB/s) | ≈ 6.2 adj DRAM B/B = **S1 app→arena ≈ 3** + S2 NT sever ≈ 2 + DMA ≈ 1.2 | **≈ 3.2 adj** (−~48 %/byte) | **S1 — the prize**; one full client CPU pass/byte |
| Read cold 1 MiB (rd-il 34.41 GB/s med, 6.59 raw B/B) | RX + 1 daemon pass + **consume `slab_read` ≈ 3** | **≈ 3.6 raw class** | the consume copy ("structural POSIX — the app hands us ITS buffer": the SDK removes the premise) |
| Read warm (§5.5.1 fast path) | 2 passes (tier→arena + consume) | 1 pass (tier→app buffer) | the consume copy |
| Throughput conversion floor | — | the E-IL1 class: **+5.9 %** per deleted pass on the copy-governed cold row (A-B-B-A, order-independent); larger expected on client-CPU-walled fleets (the ingest-economy wall class) | — |
| Interposition tax | fd-table probe + TLS guard + dlsym chains + libaio emulation | direct calls, native batch | G-S3 pins ≥ the 633 k libaio-shim row |
| Honesty rows | sync RTT (2–3 µs) unchanged; rand-4k warm machinery-bound (no claim); daemon side byte-identical | | |

## 4. PR ladder (implementation = follow-on campaign)

SDK-1 skeleton + sync ops + fallback parity (G-S1) → **SDK-2
arena-native buffers + the lease law + the G-S2 counted A-B-B-A (the
go/no-go; miss ⇒ the surface self-deletes)** → SDK-3 batch submit/reap
(G-S3) → SDK-4 durability classes (blocks on rewrite Idea 8) → SDK-5
scoreboard `sdk` mode + closing report. Full ladder + gates:
`docs/design-sdk.md` §13.

## 5. Change manifest (this branch)

* `crates/squeezefs-preload/tests/linked_mode_tests.rs` — red-first contracts.
* `crates/squeezefs-preload/src/session.rs` — `LoadMode` + `load_mode_from`/`load_mode`.
* `crates/squeezefs-preload/src/interpose.rs` — `announce_load_mode()` on first blob decode.
* `crates/squeezefs-preload/build.rs` — cdylib SONAME + `-z nodelete`.
* `tests/run_preload_gate.sh` — legs 1e + 2b-linked (+ header, cleanup).
* `docs/operations.md` — the Direct link block (link order, `--as-needed` trap, setuid/env-scrub wins, fatal-missing-DT_NEEDED, KD-7 unchanged).
* `docs/design-preload-interception.md` — Rev 17.
* `docs/design-sdk.md` — the program design (new).
* `AGENTS.md` — reference-list rows (Rev-17 clause + design-sdk.md).
* `.benchmarks/2026-08-04-sdk-design.md` — this note.
