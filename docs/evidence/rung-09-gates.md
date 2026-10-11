# Rung 9 — the gates: skip ledger, fuzz campaign, literal rail, microbench baseline, release act

Charter §11 ("Engineering discipline") and §13 rung 9 ("Release gates, fuzz
campaign, baselines"). Commits on `dev` after `ce0edf4`: `9949e79` (the
fabric sampler's allocator lifetime), `9a45b0a` (skip ledger + fuzz census +
campaign), `9661053` (literal rail), `9934f02` (bench baseline), `36dc8f6`
(train identities), `0438272` (release act + gate docs), this note. The box
brackets (the charter's performance bars on the reference class of hardware)
are not in this rung's scope: every number here is the dev venue's, scoping
only.

## 1. The per-commit gate (`zig build gate`) — the full step list

| step | what | venue |
|---|---|---|
| `fmt` | `zig fmt --check build.zig src tests` | both |
| `test` | the unit suite over the library module (portable modules on every host; Linux-only modules on Linux) | both |
| `rails` | `tests/rails.zig`: knob census; inverse knob census; stats inverse census; **skip rail**; **fuzz census**; **literal rail**; the SDK header census; + the shim ELF census on Linux | both |
| `smoke` | `smoke:` tests: the binary against real mounts | Linux |
| `bench-smoke` | `bench gate:` — `squeezefs bench --smoke` (one iteration of every workload) | Linux |
| `shim-smoke` | the LD_PRELOAD battery | Linux |
| `nvmet-smoke` | the nvmeof fidelity tier | Linux |
| `sdk-smoke` | the C smoke against the static and the shared SDK | Linux |

New this rung: `zig build gate -Drelease-gate=true` sets
`SQUEEZEFS_TEST_RELEASE_GATE=1` on every test run step (skips fail, §3);
`zig build gate-release` = gate + `bench-compare` after it (§5); `zig build
fuzz` is the nightly campaign (§2); `bench-baseline` records (§5).

## 2. The fuzz campaign

**Census** (`src/gates/census.zig`; the rail "fuzz census" in
`tests/rails.zig`): the naming law — a `pub fn` named `decode*`, `parse*` or
`from*` taking a const byte slice (`[]const u8`, `[*]const u8`, `[:0]const
u8`, `[]align(N) const u8`, `*const [N]u8`; a slice of slices or a mutable
`[]u8` is not a decoder) — finds **139 decoders in 72 files**; every one is
named by a `test "fuzz: …"` in its own file (the fuzz closure: identifiers
and `A.b` pairs in the block plus the bodies of the file's functions it
reaches, to a fixpoint). **74 fuzz tests** (29 existed, of which 4 were PRNG
loops converted to the fuzzer; **45 new**), all through the ONE entry
`fuzzkit.fuzz` (`src/core/fuzzkit.zig`: `std.testing.fuzz` plus the replay
seam `SQUEEZEFS_TEST_FUZZ_REPLAY`), names unique tree-wide (the campaign
filters by name; one duplicate renamed). Checksummed records are also
RESTAMPED inside the fuzz body (the checksum over the fuzzed bytes) so the
checks behind the checksum are reached, and whatever decodes re-encodes
(byte-identical where the encoder is canonical, value-identical where the
wire tolerates padding).

**The campaign** (`src/gates/fuzz_campaign.zig`, `zig build fuzz`): per
target a fuzz-mode rebuild (`--fuzz=1`), an overhead launch, an adaptive
calibration (grown tenfold until ≥ 0.5 s past the overhead), the budgeted
run; `SQUEEZEFS_TEST_FUZZ_SECONDS` (default 60; total = targets × budget);
corpus under `SQUEEZEFS_TEST_FUZZ_CORPUS_DIR` (`/build/fuzz-corpus`) with
the build cache's `f/<hash>` symlinked onto `<root>/<slug>/` (re-created from
the slug's `.name` after a cache wipe); a crash is saved to
`<slug>/crashes/<unix s>.bin`, reproduced through the replay seam and
minimized by greedy chunk removal (`.min.bin`; each probe its own file, the
replay path in the run step's env so no probe is a cached pass); the report
to stdout and `<root>/report.md`. The build runner's iteration-limited mode
exits 0 on a crash, so the text `input saved to` is the verdict; its input
capture can race the crash and save an empty artefact (seen once on the dev
box with a deliberate probe) — the report says so. `vendor/zstd` is now its
own static library: the fuzz rebuild's clang coverage pass emitted
`__sanitizer_cov_trace_const_cmp*` into instrumented C that Zig's fuzzer
runtime lacks.

**The first campaign** (the venue, 10 s per target, 2026-10-10): 73 targets
(before the baseline JSON decoders' test landed), 730 s budget, ≈ 28 min
wall with the rebuilds, **84,424,445 execs, 0 crashes**, corpus 10 MB. The
full table is in `docs/gates/fuzz-campaign.md`; the extremes:

| target | execs | corpus | coverage |
|---|---:|---:|---:|
| record value decoders (`meta/record.zig`) | 5,402,852 | 209 | 806/22746 |
| every verb decoder (`wire/verbs.zig`) | 521,281 | 420 | 2018/25163 |
| the fsck report parser (`ops/fsck/finding.zig`) | 374,343 | 155 | 1058/25674 |
| the .stats term reader (`cli/bench.zig`) | 188,000 | 29 | 176/22462 |
| the enroll decoder (`wire/auth.zig`) | 1,169,223 | 2 | 39/22394 |

No crash ⇒ no red repro this campaign. The crash path itself was proven on
the dev box with a deliberate `@panic` probe in the kit's fixture: saved,
reproduced, minimized 18 B → 6 B (`len ‖ 2 bytes`), then removed.

## 3. The skip ledger

`src/core/testkit.zig` is the ONE skip: `skip(@src(), class, detail)`
prints `skip[<class>] file:line: detail`, appends a row to
`SQUEEZEFS_TEST_SKIP_LEDGER` when set, and returns `SkipZigTest` — or
`SkipRefusedByReleaseGate` under `SQUEEZEFS_TEST_RELEASE_GATE=1` for every
class but `needs_reference_hardware` and `other_step`. The mount rigs share
`testkit.venue.mount(io, staged)` (the staged inputs first → `other_step`,
then root, `/dev/fuse`, CAP_SYS_ADMIN). All **140 bare `SkipZigTest`
sites in 58 files** are migrated; the rail "skip rail" refuses a bare one
outside the kit.

Classes: `no_linux`, `no_root`, `no_cap`, `no_device`, `no_nic`,
`no_kernel_feature`, `no_concurrency` (the executor refused a unit),
`no_seam` (a harness marker unset), `debug_only` (the lock-order rail is
compiled out of ReleaseSafe), `external_interference` (a concurrent runner's
sweep killed a daemon mid-row), `no_baseline` (bench-compare on a venue
without its own), `needs_reference_hardware` (exempt), `other_step` (exempt:
a test another step stages inputs for).

Counts (the ledger file of one run; the kit's own two probe rows excluded):

| run | rows | by class |
|---|---:|---|
| macOS `zig build test` | 24 | no_linux 24 |
| Linux `zig build test rails` | 38 | other_step 36 (the smoke/shim/host/guard/fidelity rigs in the unit run), no_kernel_feature 1 (CONFIG_ETHTOOL_NETLINK), no_seam 1 (the looped smoke stage) |
| Linux `zig build gate` | see §7 | |

## 4. The literal rail

`src/gates/literals.zig` states the law; `tests/rails.zig` "literal rail"
runs it against `tests/rails/literal-allowlist.txt` (`path:line | literal |
reason`); a row matching no violation is stale and fails;
`scripts/rails-literal-rebase` moves rows across line shifts (same path, same
literal) and never admits a new literal.

Product scope: `src/**/*.zig`, outside `test` blocks, above the file-scope
`const testing = std.testing;` (the tests' preamble: the fixtures below it
serve the tests), not a harness file (`smoke`, `e2e`, `fidelity`, `_model`,
`testkit`, `fuzzkit`, `testing` stems). Allowed: 0/1/2; an enum field value;
a switch prong; inside an assertion / `comptime {}`; an index or width
(inside `[…]`, a `1 << n` flag bit); documented on the line (an EOL comment
or a comment heading the statement's same-indentation group); a row of a
documented enclosing declaration.

| measurement | literals |
|---|---:|
| first law (every number literal outside test blocks; group docs only directly above; harness files excluded) | **3145** |
| + index/width, flag-bit, group-heading rules | 1947 |
| + struct-literal braces are not statement bounds | 1753 |
| + the tests' preamble ends product scope | **768** (the law as landed) |
| after 3 modules fixed on the line | **708 allowlisted** |

Fixed on the line (commit `9661053`): `cli/bench.zig`'s splitmix64 /
xorshift64* constants named and cited; `meta/sym/appender_page.zig`'s put
block headed by its layout note; `codec.TAIL_ALIGN` (the 8 B wire grain)
replacing 32 bare `8`s across `wire/verbs.zig`, `wire/frame.zig`,
`ops/jobs/frames.zig`, `ops/jobs/admin_frames.zig`. The 708 rows carry class
reasons: layout offsets/widths 40, codec table entries 34, file modes 13,
ports 5, and **616 marked OWED** (372 "derive it or document it on the line",
103 sizes, 67 hex encoding constants, 53 percentages, 21 timing/retry
constants) — the backlog this rail now keeps honest and shrinking.

## 5. The microbench baseline

`src/cli/bench_baseline.zig` (pure; tests on every host) + the rig
`src/cli/bench_baseline_rig.zig` (`zig build bench-baseline` /
`bench-compare`; `gate-release` runs the compare after the gate). Groups:
the six workloads; a run folds `squeezefs bench --json` per group (ops/s
over the group's elapsed, median p50, max p99, the admission + reclaim
ledgers; a byte-moving row under 256 ops refuses the shape); a baseline is 3
runs with mean + sample σ, the measurement-law fields (instrument
train/commit/profile, substrate, venue kernel/cpu/disk class, label); the
thresholds are `max(3σ, 5 % of the mean)` per group for an ops/s drop and a
p99 rise; both files are keyed by the venue id `kernel__cpu__disk-class` so
a compare on another venue skips `no_baseline`.

Red-first bench fix found while recording: the bench sized every pass's
coverage and `expected_ops` by the SHAPE's block, so the suite's rand 4 KiB
passes over a 1 MiB-block shape touched 1/256 of the file and reported full
coverage (`expectedOps(pass)` and the order array now use the pass's block).

The committed baseline (`bench/baseline-7.2.6-sqz__arm-impl0x61-part0x000-9c__file-ext4.json`,
label **scoping-venue**, 2 × 1 × 256 MiB, 2 iterations, 5 s box, the O_DIRECT
suite, a file-backed set on ext4, load ≈ 7 from a concurrent agent's suites):

| group | ops/s mean ± σ | drop max | p50 µs | p99 µs mean ± σ | rise max µs | admission B | reclaim t/q/e/discard |
|---|---:|---:|---:|---:|---:|---:|---|
| write_seq_1m | 1055 ± 97 | 290 | 2048 | 27307 ± 9459 | 28378 | 1073741824 | 0/0/0/0 |
| read_seq_1m | 5337 ± 597 | 1792 | 512 | 1024 ± 0 | 51 | 0 | 0/0/0/0 |
| read_rand_4k | 9859 ± 700 | 2101 | 256 | 512 ± 0 | 26 | 0 | 256/256/0/0 |
| write_rand_4k | 3232 ± 143 | 430 | 1024 | 4096 ± 0 | 205 | 138211328 | 0/0/0/0 |
| stat | 3302 ± 457 | 1372 | 512 | 683 ± 296 | 887 | 0 | 0/0/0/0 |
| del | 2487 ± 90 | 270 | 1024 | 1024 ± 0 | 51 | 0 | 0/0/0/0 |

(p50/p99 are the bench's bucket upper bounds; a σ of 0 on a latency means
every run landed in the same bucket, and the 5 % floor then sets the rise
max.) A `bench-compare` minutes later, with the other agent's LTP run and a
bracket mounted beside it, judged every byte-moving group REGRESSED (write
seq 316 ops/s vs 1055) — the shared venue is not a judge, which is why the
compare lives in `gate-release` for a quiet venue and never in the
per-commit gate.

## 6. The release act

`scripts/release.sh <train> [--dry-run] [--wire-abi-bump] [--sdk-abi-bump]`
and `docs/gates/release.md` (the versioning law). New identities: the shim
carries `squeezefs-il train <train> commit <commit>` (its pairing lines
print it), the SDK's `sqz_version()` reads `squeezefs-sdk <train> wire-abi
W sdk-abi S`; `build.zig` refuses to configure when `train_version` and
`build.zig.zon`'s `.version` disagree. The dry run (0.1.0 → 0.2.0, the
venue): the two edits, `squeezefs 0.2.0 (… gate profile …)`, the shim and
the SDK strings naming 0.2.0, the pairing/window tests 42 pass, the dist
build's `.gnu_debuglink` → `squeezefs.debug` and `squeezefs 0.2.0 (… dist
profile …)`; nothing committed. The ABI-bump dry run moved the wire word to
2 with the window `{2, 1}`, the SDK word to 2, both header words and the
`.so` minor to 1, and the verification failed the SDK window test — the act
now refuses that, and the law says the window tests' new values land in the
same commit. **The train is not bumped**: that is the owner's act.

## 7. The gates at the end of the rung

The venue carried another agent's LTP run, then its nvmet-loop scoping
brackets, throughout (load 4–10, three live fabric controllers in sysfs,
page-cache churn); every failure below is attributed to that sharing and
re-ran green where it could be re-run.

**macOS `zig build gate`** (the portable half; HEAD `a4d61dc` minus the
rails' vanished-file tolerance, re-verified with `zig build fmt rails`):
25/25 steps, **1997/2021 tests passed, 24 skipped** (unit 1876 pass + 24
`no_linux`; rails 13/13; the smoke/bench/nvmet binaries' aggregator tests
pass vacuously).

**Linux `scripts/venue zig build gate`** (full run, `SQUEEZEFS_TEST_SKIP_LEDGER`
set): unit **2295 pass, 40 skip, 1 fail** (2336) — the one failure is
`stats.zig` "allocation law" with the concurrent agent's three fabric
controllers in sysfs (§9); rails **13/13**; shim census 1/1; smoke 59 pass /
2 skip / 2 fail in the gate run — daemon B's `kmbuf ring registration ENOMEM
(order-8 contiguous pages)` under the other agent's page-cache churn (the
known venue refusal) and a kill-row's `size >= claimed` under load — and
**61 pass / 2 skip** on the immediate re-run of `smoke bench-smoke
shim-smoke nvmet-smoke`; bench-smoke **40 pass**; shim-smoke pass;
nvmet-smoke **49 pass / 2 fail** — both legs read the SHARED kernel nvmet
configfs tree while the brackets agent's nvmet-loop ports were live (a
non-harness port id on our listener; their subsystem in `foreign_live`);
sdk-smoke ok (static + shared). Skip ledger of the Linux gate: other_step
40 (the rigs in the unit run: fidelity 12, shim smoke 3, host e2e 2, guard
smoke 1, the smoke rows 22), no_seam 3, no_kernel_feature 1
(CONFIG_ETHTOOL_NETLINK), plus the kit's two probe rows. An earlier gate
attempt was killed by its 65-minute `timeout` while the other agent's LTP
run held the venue; another lost its rails to a `.zig` file that vanished
mid-walk (the rails now skip a vanished file, `a4d61dc`).

The literal rail's allowlist was rebased twice during the rung (`19` rows
after the bench edit, `1` after the shim identity) with
`scripts/rails-literal-rebase` — the line-keyed row is the brittleness the
rail's law bought; the rebase never admits a literal.

Three gate hazards made deterministic afterwards (all test-side; no product
bug underneath):

- **`write_admit` "dead victim"** (`5db4540`, rung 9 F2): the pin spawned
  three writers at once and spun until three fallbacks and three capacity
  waiters showed — but a writer entering the gate while a sibling's cold
  flush is in flight DEFERS to that upload (the hot-set law), so the third
  first-round flush could land a whole tick late, on the first writer's
  tick: `expected 3, found 4|5` (16/100 in a loop on the venue), the wall's
  tripwire logged (7/100), and on that early return the World was torn down
  under three live, never-joined writers on a dead device — SIGSEGV in
  `waitUntilUncancelable` (9/100) or the panic-handler wedge seen once in
  four pristine `dev` runs (26 min). Fix (`0166ea7`): `ParkHook.refused`,
  the gate's second station (after a refused cold flush, registered on the
  capacity notify, the tick not started); the test's `RefusedHold` holds
  each writer there over `sync.Notify` — arrivals serialized, counters
  exact, the F2 trigger (a refused flush's settle) fired at the released set
  from the test thread, the quiet window a quarter-tick deadline wait on
  `upload_settled` itself; every test-side wait bounded at two ticks with
  the gate's counters on red, the writers fenced out and joined on every
  exit. The settle-riding gate re-introduced goes red in 3 s. Proof:
  **100/100** venue (2.01 s ×99, 2.02 s ×1), **30/30** under an 8-way `yes`
  load (2.01–2.03 s), **60/60** macOS (2.02–2.05 s).
- **`--daemon --supervise` smoke**: the row asserted the supervisor's
  `watching daemon` line on the log snapshot taken right after the invoker
  returned, but that line is written by the intermediate process AFTER the
  ready handoff that returns the invoker: 3/10 red under an 8-way `yes`
  load (load 6–9), green alone. Fix (`8e69fb9`): `awaitLog` from the ready
  line under the rig's bring-up bound (the log printed on a timeout).
  Proof: **15/15** under the same load (load 10–13).
- **`journal_lane` "a lost kick … exactly one tick rescue"** (F6,
  `9774031`): F6 moved the backstop tick's verdict to the next poll's
  re-check — the poll that finds the completed prefix also ACKS the
  successor inside `advanceDurability` and judges the tick only after that
  returns, so the ack reaches its waiter BEFORE the rescue is counted. The
  pin read `tick_rescues` the moment the ack returned: `expected 1, found
  0` once in the gate at load ~9, 4/200 in a loop under an 8-way `yes`
  load, 11/11 alone — the waker/wakee preemption of a loaded box, not a
  lost rescue (the count lands microseconds later). Fix (`1c47f85`): the
  pin records the count before the foreign completion and, after the ack,
  waits for the acking poll's verdict on the lane's own counter
  (`spinUntilBy`, two liveness ticks, the lane's counters on red); with the
  rescue path removed the row is red in 4 s (`tick rescues 0 (expected 1),
  backstop ticks 1, parks 3, prefix waits 1`). Proof: **500/500** on the
  venue under the 8-way load (0.11–0.17 s; load 6.5–8.5). Linux-only.
  Gates at its landing (on `0b6df4e`): macOS 25/25, 2029/2053 (24 skips;
  unit 1905, rails 16/16); Linux unit **2334 pass / 41 skip**, rails 16/16,
  shim census 1/1, smoke 60 / 2 / 1 fail beside the suites agent (daemon
  C's kmbuf order-8 ENOMEM, the known venue refusal) and **61 pass / 2 skip**
  with bench-smoke 40, shim-smoke 40, nvmet-smoke 51 on the immediate re-run.

Gates at the landing (`8e69fb9` rebased onto `c65935a`): macOS `zig build
gate` **25/25 steps, 2029/2053** (24 `no_linux` skips; unit 1905 pass, rails
16/16); Linux `scripts/venue zig build gate` on a quiet venue **43/43 steps,
2543/2586** (43 skipped): unit 2334 pass / 41 skip, rails 16/16, shim census
1/1, smoke 61 pass / 2 skip, bench-smoke 40, shim-smoke 40, nvmet-smoke 51.
The first Linux gate of the same commits ran beside the suites agent's
fstests (load ~9) and read unit 2331 / 41 / **1 fail** — `journal_lane` "a
lost kick … exactly one tick rescue" (`expected 1, found 0`; the third
hazard above, fixed in `1c47f85`) — and smoke 58 / 2 / 3 fail (the two-daemon
matrix, kill rows (a) on daemon B's `kmbuf ring registration ENOMEM
(order-8)` — the known venue refusal under page-cache churn — and the
`-o ro` reader), all green on the immediate re-run.

## 8. Found and fixed along the way

- `ops/nvmet/initiator.zig`: the `.stats` render's fabric sampler kept its
  retained identities in a process-global list allocated from the RENDER's
  allocator; under the test runner (a per-test allocator) the next render
  read a dangling list on any venue with live fabric controllers — the
  stats suite segfaulted while the other agent's nvmet suites ran. Fixed
  (the list lives on the process allocator; red-first repro counts the
  render allocator's live allocations).
- `cli/bench.zig`: pass coverage by the pass's block (§5).

## 9. Owed

- The literal allowlist's 616 OWED rows (§4): per-module passes deriving or
  documenting them, each a commit that shrinks the file.
- `stats.zig` "allocation law: a render is exactly ONE allocation" fails
  when fabric controllers are present in sysfs (the sampler's transient
  tables allocate per render): make the sampler's tables fixed-capacity or
  scope the law. Pre-existing; surfaced by the concurrent nvmet suites.
- The campaign at 60 s per target (the nightly default) on the venue, and a
  60 s campaign after every decoder change; a crash's red repro by the
  documented recipe.
- A `bench-compare` and a baseline on a quiet venue (every number in §5 was
  taken beside another agent's suites); the reference box's baseline when it
  exists.
- The "sdk e2e: the ABI window" fixture should derive its expected window
  text from `wire.sdk_abi_accepted` so an ABI bump does not need a hand edit.
