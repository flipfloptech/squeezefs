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

Six gate hazards made deterministic afterwards (all test-side; no product
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
- **The smoke's `kill -9 mid-tar -x` row judged at the FILE grain where the
  charter's promise is the WRITE(2) grain** (found by the definitive gate
  at `585152f`: `f199` read **512 bytes, not whole** → `error.TornFile`,
  once in weeks of gates). The tree is 1,500 entries of exactly 1,000 B
  (`file-NNNNN:` + 989 letters) archived with `tar -cf ktree.tar -C ktree .`
  — a 512 B header + 1,000 B of data padded to 1,024 B per entry, the `./`
  directory header first. GNU tar 1.35 reads the archive in records of
  `-b20` × 512 B = 10,240 B (`tar --show-defaults`) and `extract_file`
  issues one `write(2)` per record-contiguous span (`available_space_after`),
  so an entry whose data straddles a record boundary is extracted as TWO
  writes. The arithmetic: entry *i*'s header sits at block 1 + 3*i* (the
  `./` header is block 0), its data at blocks 2 + 3*i* and 3 + 3*i*; the
  data straddles iff 2 + 3*i* ≡ 19 (mod 20) ⇔ *i* ≡ **19 (mod 20)** — not
  the ≡ 6 a headerless archive would give. **strace of `tar -xf` on a
  scratch ext4 dir** (`-f -e trace=write,openat`, the same archive): 1,500
  files, **75 written in two calls — exactly f19, f39, …, f199, …, f1499
  (every archive index ≡ 19 mod 20), each `write(…, 512) = 512` then
  `write(…, 488) = 488`**; the other 1,425 one `write(…, 1000)`. `tar -R
  -tf` confirms the layout (`block 0: ./`, `block 1: ./f0`, `block 58:
  ./f19`, `block 61: ./f20`). The row kills at ≥ 200 files visible, so the
  entry in flight at the kill is usually **f199 — a two-write entry**: the
  kill lands between an entry's two writes far more often than 75 in 1,500.
  **The repro** (a driver mirroring the row: format, mount, `tar -xf` under
  strace, SIGKILL at 200 visible, remount, read every file; 40 rounds under
  strace, 40 without): **29/40** rounds under strace and **17/40** without
  (the row's 5 ms poll cadence makes it rarer still) ended with ONE
  non-whole file, **f199 at 512 B every time, byte-equal to the expected
  body's prefix**, `st_size` 512; tar's own ledger beside it in all 29:
  `openat(4, "f199", O_CREAT|O_EXCL) = 5`, then in 25 `write(5, …, 512) =
  512` followed by `write(5, …, 488) = -1 ECONNABORTED` (16) or `ECANCELED`
  (9) — the second write never acked — and in 4 `write(5, …, 512) = -1
  ECONNABORTED` — the first write landed, its reply lost. The recovery
  side: every remount read `0 torn dropped`, and a hit round replayed
  **1007 entries / 1615 content records against 1008 / 1617** for a round
  with f199 whole — exactly ONE transaction fewer (the 488 B write's layout
  Put + inode Δ), never a dropped or truncated one; the inline arm
  (`kv_fs.writeInline`) publishes the WHOLE image per `write(2)` as one
  journal entry, so no single write can leave a prefix. **Verdict: H1** —
  the 512 B prefix is tar's first `write(2)` exactly, a legitimate POSIX
  crash state (the 1,000 B write the row assumed never existed for that
  entry); no torn write, no charter violation; the oracle judged files
  where the charter (§1.10 "zero acked loss") promises syscalls. Fix (test
  side, this commit): `TarWriteGrain` derives each entry's first-write
  length from the ARCHIVE (`tar -R -tf` header blocks × `--show-defaults`'
  record) and tar's stderr is the ack ledger (`Cannot open/write` ⇒
  unacked, as the kill rows (a) already read it): an entry tar was ACKED on
  must be present and whole (`error.AckedWriteLost` / `AckedCreateLost`);
  an unacked one may be whole, empty or cut EXACTLY at its first write
  (counted, printed), anything else `error.TornFile`. The strict-cadence
  twin (`SQUEEZEFS_META_FLUSH_INTERVAL_MS=0`, the barrier before every ack;
  the mount line names the cadence) runs the same body; the oracle itself
  is a pure function with its own test (the gate's 512 B → accepted when
  unacked on a two-write entry, `AckedWriteLost` when acked, `TornFile` on
  a one-write entry or at 700/488 B), and the kill poll is tight (readdir
  back to back) so the state is exercised, not merely tolerated. Proof:
  **30/30 runs of both rows (60 rows, 0 failures)** via `zig build smoke
  -Dtest-filter="kill -9 mid-tar -x"` on the venue, the record-boundary
  state met and accepted **3 times** (all under the strict cadence, whose
  per-group barrier widens the gap; `f199: 512 bytes = tar's first
  write(2) of the entry, its second (488 B) never acked`), tar's unacked
  set 1,299–1,301 entries every time and every acked entry whole; a prior
  30-run loop at the old 5 ms poll cadence 60/60 with 0 hits. Gates at the
  landing: macOS `zig build gate` **28/28 steps** (the portable half; the
  rails re-run directly against the tree 16/16 — `smoke.zig` is not in the
  macOS compile, so its unit and rails steps were cache hits); Linux
  `scripts/venue zig build gate` with the skip ledger: unit **2375 pass /
  41 skip**, rails 16/16, shim census 1/1, smoke **64 pass / 2 skip** (both
  kill rows green), bench-smoke 40, shim-smoke 40, sdk-smoke ok, and
  nvmet-smoke **49 / 2 fail** beside the brackets agent's live nvmet-loop
  shares (`brk-1126008-set0-*` in the SHARED configfs tree: a non-harness
  port id on our listener, their subsystem first in `foreign_live` — the
  venue-sharing class above), which blocked suites-smoke; once the tree was
  quiet (25 min later) `nvmet-smoke suites-smoke` and their smoke chain
  re-ran **23/23 steps, 195/197 (2 skipped)**: nvmet-smoke 51/51,
  suites-smoke ok, smoke 64 / 2 skip, bench-smoke 40, shim-smoke 40. Skip
  ledger: other_step 40, no_seam 2, no_kernel_feature 1
  (CONFIG_ETHTOOL_NETLINK), the kit's two probe rows.
- **The hash microbench unit test's `chained ≥ throughput`** (the closing
  gate at `d390207`, once per host on a QUIET venue under the gate's own
  `-j`; 3/3 alone): the test asserted that a real chained cell's ps/op was
  ≥ its throughput sibling's — two durations measured 100 ms apart under
  whatever the scheduler was doing. **67/150** red in a loop under an
  8-way `yes` load. Fix (`22e764b`): the unit test keeps the pure rows
  (cell arithmetic, JSON names, the three columns' deterministic values,
  the measurement loop's own law — span ≥ `MIN_SPAN_NS`, ops ≥
  `FIRST_BATCH`, the mode's `access` name) and pins the relation on FIXED
  counts (2^20 × 16 B in 100 vs 250 ms: 95.367 / 238.418 ns/op, 167 / 67
  MB/s); the real relation is a new `bench gate:` row of `bench-smoke` —
  `squeezefs bench --hash --json` once, 24 cells with the measurement-law
  fields, the 16 B chained vs cpu ps/op per algorithm REPORTED (venue:
  xxh3-64 5115/2178, sqzhash128 5733/1349, sqzhash64 5582/1349), never
  asserted. Proof: **150/150** under the load (0.46–0.75 s); bench-smoke 41
  rows in 9 s.
- **The shim e2e "fio's shape" row's `served == ops`** (the same closing
  gate; reported as "3 of 4 completions inside the 5 s deadline" — it was
  not: all four ring completions were reaped every time, the read at `shim/
  e2e.zig:548` found the HOST's account at 3). `StubHost.serveOne` bumped
  `served` AFTER `completeAndWake` (the slot's DONE release + the doorbell
  wake), so the woken reaper read the account before the host thread
  reached its increment — the waker/wakee preemption again: **7/300** under
  the 8-way load, each red run 0.4 s (never the bound). Every e2e/SDK row
  asserting `served` on a completion's heels carried the window. Fix
  (`9042514`, the stub, which is test infrastructure in `ipc/client.zig`):
  the bump precedes `slot_core.completeAndWake`, so the DONE swap's Release
  half publishes it to any client that acquires the completion; the fio
  row's reap loop keeps its 5 s only as a wedge bound (the reap itself is
  the rendezvous: the doorbell park re-checks every 5 ms) and prints the
  reaped count, the host's account, the park eras and the abandoned slots
  when a completion is missing. Proof: **400/400** under the load
  (0.13–0.19 s; load 3.3–7.4). Gates at the landing of five and six (on
  `d390207`, a quiet venue): macOS **28/28, 2083/2108** (25 skips; unit
  1959, rails 16/16); Linux **46/46, 2611/2655** (44 skipped): unit 2398
  pass / 42 skip (2397 / 43 once the hash row's unit-run skip joined the
  ledger as `other_step`), rails 16/16, shim census 1/1, smoke 64 / 2 skip,
  bench-smoke **41** (the hash row), shim-smoke 40, nvmet-smoke 51.

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
- ~~`stats.zig` "allocation law: a render is exactly ONE allocation" fails
  when fabric controllers are present in sysfs (the sampler's transient
  tables allocate per render): make the sampler's tables fixed-capacity or
  scope the law. Pre-existing; surfaced by the concurrent nvmet suites.~~
  **Resolved 2026-10-12 (`8b0df62`; 0003 §I).** The cause, not the law: the
  sampler owns its tables for the process's life (the merged identities and
  the presence table join the retained identities on the process allocator,
  sized when the population grows and reused after), and the sysfs walk folds
  controllers straight off `getdents64` into a stack `Controller` — no name
  list, no dupe, no sort, no controller array; `sampleFabric()` takes no
  allocator. Measured on the venue with ONE real nvmet-loop controller
  connected (`nvmeof share --transport loop` + `nvmeof connect`, torn down
  after): the old render was **6 allocations** (the law's test on `84610ce`:
  `expected 6, found 11` — the payload plus five sampler allocations, each
  freed again) and the new render **1** (the same test on `8b0df62`, 42/42).
  Red first: `stats.zig` "allocation law under fabric controllers (linux) …"
  renders over a fixture sysfs of three fabric controllers (the process
  sampler's root is a test seam) and asserts the warm render is exactly one
  allocation with no resize, remap or free while the gauges read the
  controllers; `initiator.zig` "the sampler's tables are retained across
  samples …" pins the sampler's own zero calls from the second sample on,
  the reconnect law across samples, and the zero sample of an absent
  `class/nvme`.
- The campaign at 60 s per target (the nightly default) on the venue, and a
  60 s campaign after every decoder change; a crash's red repro by the
  documented recipe.
- A `bench-compare` and a baseline on a quiet venue (every number in §5 was
  taken beside another agent's suites); the reference box's baseline when it
  exists.
- ~~The "sdk e2e: the ABI window" fixture should derive its expected window
  text from `wire.sdk_abi_accepted` so an ABI bump does not need a hand edit.~~
  **Resolved 2026-10-12 (`ecec435`).** Every expected value of
  `src/sdk/session.zig` "sdk e2e: the ABI window …" is derived: the refused
  HELLO word is `windowBounds(&wire.sdk_abi_accepted).max + 1` (asserted
  outside the window and the build's own word inside it), the refusal's
  `sdk_abi_min/max` are the window's bounds, the ipc-mismatch line is printed
  from `wire.IPC_ABI` and `IPC_ABI + 1`, and the window line from the bounds
  and the refused word — a `--wire-abi-bump` release act edits the words
  alone (`docs/gates/release.md`'s "written by hand before the act" no longer
  applies to this fixture; `wire.zig`'s and `version.zig`'s "today's set is
  {1}" pins stay the act's deliberate edits).

## 10. Rung 10 — the gate's three intermittent rows (2026-10-13)

Each row was red in another agent's gate on a change that did not touch
it; the release gate's three-consecutive-runs rule (charter §11) cannot
hold with any of them. Each was reproduced, attributed (the test's
schedule or the product), pinned red where the window can be held, fixed,
and looped under an 8- or 16-way `yes` load on the venue (load 6–10, other
agents' suites alongside).

- **The fleet-jobs unit row** ("a fleet READ shard whose worker dies
  mid-walk"; the striping agent's 2 of 30): **7 of 1,000** on pristine `dev`
  under a 16-way load, as `expected .refused, found .abandoned` (the
  zombie's proposal) and `expected .done, found .abandoned` (the fresh
  pull, which runs its engine inline). PRODUCT: the read shard's heartbeat
  fiber parked with the recheck-before-enable shape `sync/notify.zig`
  retired — a `stopRequested` between its `stop` read and its registration
  inside `waitUntil` was lost, the fiber parked its whole 10 s period,
  `hb_task.cancel` waited it out and the worker's own lease clock (the
  row's 1 s TTL) ran out before the proposal. Red: `Worker.heartbeat_station`
  holds the fiber's first park in its window while the walk's end raises
  the stop; `wake.waiters` at the station read 0 (unregistered). Fix
  (`fix(jobs): the fleet worker registers on its wake before it re-reads
  the stop`): every park a worker's stop or release notifies registers
  first (the heartbeat, `parkUnlessStopped` for the task pacing, `serve` and
  the member's `fleetMain` retries, `holdForRelease` for the zombie seam,
  whose unbounded park could miss its release forever); the "lease expiry
  reassigns" row sweeps two TTLs ahead on the charter's TTL. (`dev`'s
  `752836b..fc80046` widened the dying row's TTL to a minute, which hid the
  abandonment but not the 10 s late proposal.) Loop: **0 of 1,000** under the
  same load (the four fleet/zombie rows), **0 of 300** on the landed tree.
- **The KVDA page-sink smoke row's daemon B "ended by a signal at umount"**
  (the budget agent's gate run 2; the jobs agent's gates met the same
  `expected .exited, found .signal` twice in the MutualTls rows): not the
  busy-umount contract racing a kill — two PRODUCT crashes after the
  daemon had detached its mount (so the verb read an orderly exit):
  (a) `SIGSEGV` in `std.mem.eql` under a `.stats` LOOKUP racing the
  teardown — the verb's own read on its signal's heels; `Session.close`
  joins the workers while handler fibers are mid-request, and each worker's
  exit unmapped its kmbuf payload region (the request's name lives there);
  1 of 60 mount/umount cycles, 1 of 40 KVDA loops. Red: an EchoFs LOOKUP
  parked across `close` judges its name with `msync` — unmapped. Fix
  (`fix(transport): the kmbuf region outlives every handler`): the teardown
  runs at `Group.deinit`, after the session awaited every handler.
  (b) `SIGSEGV` in `plane_arm.declareWindows` on the `sqz-member` beat — the
  joiner's `WirePlane.stop` destroyed the writer plane before joining the
  beat whose every renewal declares its windows; 1 of 40 KVDA loops (no
  in-process pin: the writer plane needs a joined data plane; the loop's
  trace is the red). Fix (`fix(sym): a joiner's teardown joins its
  membership beat before the writer plane the beat declares goes`). Rig:
  a daemon that dies by a signal now prints the signal and its log's tail
  (`test(smoke): a daemon that dies by a signal leaves its name and log
  tail`). Loops: **0 of 100** mount/umount cycles; **0 of 25** runs of the
  KVDA row and its fallback (50 row runs) with the matrix, the busy and lazy
  rows and the pin below, under an 8-way load (pre-fix: 3 of 40).
- **The two-daemon matrix's umount answered BUSY** (the budget agent's gate
  run 3: `is busy (0 holder(s))`): not a rig helper — the census named the
  VERB. `squeezefs umount` polls `.stats` right after its SIGTERM; when the
  daemon's census walked /proc while that fd was open it refused (`1
  holder(s): pid N (squeezefs) fd 3`), and the verb's census, which skips
  itself, found nobody; 1 of 40 mount/umount cycles. PRODUCT (the census).
  Red: a forked sender opens `.stats` and SIGTERMs the daemon with it open —
  refused. Fix (`fix(umount): the daemon's holder census exempts its stop's
  sender's control-document reads`): `holders.Exempt` (one pid's fds on
  inodes a predicate accepts, by fdinfo's `ino:`), the daemon latching its
  stop's sender from `si_pid` (`SA_SIGINFO`) and exempting its fds on the
  control documents' generation inos; a sender holding the mount any other
  way is still refused. The alert printer matched `warn(` — the sink writes
  `warning(` — which is why the daemon's own refusal never reached the gate
  log. Loops: **0 of 100** self-refusals in the mount/umount cycles; the
  matrix row **25 of 25**.
- Found on the way, not owned by anyone active: two transport e2e rows read
  a counter on a reply's heels (`EchoFs mounts over the ring`: `requests ≡
  replies` before the helper's background RELEASE was answered, `expected
  13, found 12`; `teardown with an in-flight request`: the dropped-reply
  baseline read after releasing the handler) — both read in this branch's
  loaded runs; fixed test-side (`test(transport): two e2e rows read a
  counter on a reply's heels`).
- Seen once and NOT fixed (its owner's call): the KVDA fallback row's
  must-stay-0 `transport_park_tick_cqe_rescues = 1` on daemon B (1 of 40
  pre-fix loops, 0 of 25 after, never in a gate log). By the kernel's
  `io_cqring_wait` contract a timed GETEVENTS that returns `-ETIME` ran the
  deferred completions after its timer and found the CQ empty (it returns 0
  otherwise), so a CQE the next reap surfaces arrived after the deadline:
  the CQE half of the rescue ledger can only count late arrivals (the
  worker preempted between the enter and the reap), never a lost wake.
  Owed: retire or redefine that half (it is in `brk.py`'s tick ledger and
  the smoke's must-stay-0 list).
- **Attributed, not fixed here — the slot handover's served ops under load**
  (the orchestrator's request: `smoke: ONLINE FSCK THROUGH THE ADMIN LANE`,
  red in the recovery agent's gates at `da080b7`/`dc7d0e7`; and `THE
  HANDOVER DRAINS PARKED STAMPS`, red in this branch's gate 2). Both rows
  pass on a quiet venue and fail under load on every tree tried, the
  recovery commits included or not — NOT a regression of `77c552a..dc7d0e7`:
  online fsck **9 of 12** on `39590f0` and **9 of 12** on `2e071ea` (4 + 4
  `yes` load, run concurrently), **5 of 10** on this branch (8-way, load
  ~12); the handover row **12 of 20** on `53b9a9b` (its landing, before
  every recovery commit), **34 of 40** on `da080b7`, **12 of 20** on this
  branch (8-way). The findings: B's `printf > xN` into A's directory —
  the slot A hands to B mid-loop (`slot 24 released by appender 0` in
  every run) — answers `cannot create xN: File exists` (open without
  `O_EXCL`; 5/12, 3/12, 3/10) or `Resource temporarily unavailable`; the
  closing `fsck --offline` finds `[C9] ino … live (nlink > 0), prior-era,
  named by no dentry` (3/12, 3/12, 2/10) — an inode a cross-owner create put
  in effect whose dentry never landed; `scrub` once answered `the live
  table could not be read (HolderUnreachable, AGAIN)` (1/12, 2/12); the
  handover row's B `chmod` of A's file answers EAGAIN (once ENOENT) between
  A's transfer and B's grant — the window `595a16b` narrowed. One class: a
  shipped verb or cross-owner step that meets the slot mid-handover
  (`record_ship.ship` follows one redirect and returns `Deferred` at once;
  a step abandoned live answers EAGAIN and the cadence's roll-forward
  later applies it) surfaces a retryable refusal to the client, and a
  create retried across the move meets its own first attempt. Owned by the
  handover/intent design (`src/meta/sym/xv`, `slots/`); not changed here.
- **The full gates of this branch** (`timeout 3900 zig build --cache-dir
  /build/zig-cache gate --summary all`, uncached by a `clock.zig` touch;
  other agents' smoke loops alongside): gate 1 (load 1.7) **46/46, 2746/2813**
  (unit 2511 / 65 skip, smoke 84 / 2 skip, bench-smoke 41, shim-smoke 40,
  nvmet-smoke 53, rails 16, shim census 1); gate 2 (load 7.9) smoke 83 / 2 /
  **1 fail — the handover row** (above); after the rebase onto `2118f55`
  three consecutive: gate A (load 9.4) shim-smoke **1 fail** — row 4b's
  `sessions1 > sessions0`: the staged `libsqueezefs_il.so` in that gate's
  directory carries identity `e1cdb245aca7` and `.wt-packtls` source paths
  (the concurrent agent's shim through the shared `/build/zig-cache`, so the
  pairing refused; the same staged pair fails 3 of 3 by hand, gate B's pair
  passes 3 of 3) — external; gate B (load 0.9) **46/46, 2753/2822** (unit
  2517 / 67 skip, smoke 85 / 2 skip); gate C (load 0.5) smoke 84 / 2 / **1
  fail — the handover row** (B's chmod EAGAIN, the attributed class). The
  online fsck row passed in all five. macOS `zig build gate` on the landed
  tree: **28/28, 2157/2184** (27 skipped; unit 2049).

## 11. Rung 10 — the gates' artifact identity, the tick CQE ledger, the fleet heartbeat's stop (2026-10-13)

The three items §10 left open. Each was pinned red first, fixed, and looped
on the venue under an 8-way `yes` load (loadavg 6–8, another agent's smoke
rows alongside).

- **Every gate step runs its own build's artifact** (§10 gate A's shim-smoke
  failure). ROOT CAUSE (build graph): the shim's `zig build-lib` was a
  `has_side_effects` Run step, and Zig 0.17's maker names a side-effect
  step's output directory `o/<hash of its argv strings>` — input files join
  the manifest but not that hash (`Maker/Step/Run.zig`: `man.hash.final()`
  when `has_side_effects`). Every worktree on `--cache-dir /build/zig-cache`
  computes the same argv (the zig path, the cache dirs, the target, the
  `-Mroot=`/`-Mbuild_options=` prefixes), so every worktree's shim landed at
  ONE path (`o/aabfdf97…/libsqueezefs_il.so`, the only build-lib directory in
  the cache), and the staging step copied it when the graph's binary was
  ready — minutes after its build-lib on an LTO rebuild: a neighbour's
  build-lib in between staged the neighbour's .so. The cache held the
  history: of 55 staged pairs, 4 named two builds (gate A's `e1cdb245aca7`
  beside `441d1be401ab-dirty` among them). Red: the census row
  `tests/shim_census.zig` "libsqueezefs_il.so and ./squeezefs are ONE build's
  pair" (the .so's `lifecycle.identity` against `./squeezefs --version`) and
  the repro — worktree A (`7e6fca1`, an exe rebuild in flight) built its
  shim, worktree B (`88723bf`) then ran `zig build shim` with the same cache
  dir, A's build-lib output named `88723bf2b86e` and A's census answered "the
  staged pair is two builds': libsqueezefs_il.so carries train 0.1.0 commit
  88723bf2b86e, ./squeezefs names train 0.1.0 commit 1ebf48d27f8d-dirty".
  Fix (`fix(build): the shim's build-lib is cached on its inputs …`): the
  step is CACHED, its inputs the compiler and every file under `src/` (a
  superset of the shim's imports, sorted; the configure re-walks the tree)
  beside `build_options`, so its output directory is the content digest of
  everything the shim can read. Green: the same repro twice — B on the old
  tree (it still wrote the shared `aabfdf97…`) and B on a second fixed
  commit (`eac2ce2f…`) — A's build-lib wrote its own directory (`6686fe…`,
  then `e0558349…` after the next touch), A's rails 16/16 steps, 19/19
  tests both times. The other steps, checked: `smoke`, `bench-smoke` and
  `nvmet-smoke` run in `exe.getEmittedBinDirectory()` (a Compile output the
  compiler's manifest names by content); `sdk-smoke` links Compile-step
  libraries (the shared one through an rpath to its own Compile output);
  `suites-smoke` takes the emitted binary by `--bin` — but its results
  directory (`/build/tmp/suites-<suite>-<date to the second>`, the sets and
  their mountpoints under it) was shared by two gates that started a suite
  in the same second: two concurrent `suite_prepare`s made ONE directory 5 of
  5 times; with `-<pid>` (and the cleanup's mount match anchored at
  `<results>/`) two, 5 of 5. Not an artifact, left as found:
  `nvmet-smoke`'s fixed TCP port base (54000) is shared by concurrent nvmet
  smokes.
- **The tick CQE ledger** (`transport_park_tick_cqe_rescues = 1` on the KVDA
  row's daemon B). The gate agent's reading holds: `io_cqring_wait` returns
  `head == tail ? -ETIME : 0` after running the deferred completions past its
  timer, so a CQE the reap surfaces after a backstop tick landed after the
  deadline — the counter could only count late arrivals. Red:
  `session.zig` "e2e: a completion that lands after a backstop tick's
  timeout and before the worker's re-check …" — a parked `/park` reader keeps
  its group owed (bounded parks), an in-process seam
  (`Shared.tick_await_cqe_remaining`) holds the next tick's worker in an
  unbounded park before its reap, the handler's release completes the wake
  READ in that window: the must-stay-0 counter read `expected 0, found 1`, 3
  of 3 (`bab8784`). Redefinition (`fix(transport): the tick's CQE ledger
  splits …`; decision 0002 row 511): the tick's RE-CHECK — the CQ read at
  the verdict, before any further enter — finding completions in a CQ that
  was empty at the park, on a `DEFER_TASKRUN` ring (its CQ moves only inside
  the worker's own enters, so the read is exact), fires the TRIPWIRE
  `transport_park_tick_cqe_lost_wakes` (the kernel's wait did not wake for
  them; on another posture nothing is classed lost); the reap's remainder is
  the plain `transport_park_tick_late_cqes`. `brk.py` `TICK_RESCUES` names
  the tripwire and a new `TICK_LATE` the late counter (in `ALL_COUNTERS`,
  recorded as `tick_late` beside `tick_rescues`); `bench.zig`
  `tick_rescue_keys`/`tick_late_keys` mirror them (the row carries
  `tick_rescues` and `tick_late`); the rails' bracket ledger pin equates
  both pairs; the smoke's must-stay-0 list and its teardown line name the
  tripwire alone. Green: the pin **0 of 50** under the load (late +1, the
  tripwire 0, the commit half 0); the 10 s storm: 0 ticks, 0 rescues, 0
  late. Still open: the commit half (`transport_park_tick_commit_rescues`)
  keeps a narrower late window of its own — a reply published between the
  post-tick reap and the pump, its wake READ still deferred — not redefined
  here.
- **A stop reaches the fleet read shard's heartbeat at once.** ROOT CAUSE:
  the heartbeat fiber parked on its own `Heartbeat.wake`, which only the
  walk's end notifies; `Worker.requestStop` notified the worker's `wake` and
  `release_hold`, and the fiber read the worker's stop only after its park —
  a stop mid-scan raised the walk's cancel at the next beat (`HEARTBEAT_MS`,
  10 s). Red: `wire.zig` "a requestStop during a read shard's scan WAKES the
  heartbeat fiber …" — "the stop took 10007 ms (bound 1000 ms; the heartbeat
  period is 10000 ms)". Fix (`fix(jobs): a worker's stop wakes the read
  shard's heartbeat fiber …`): the fiber parks on the WORKER's wake and,
  registered, re-reads both stops (`Heartbeat.ended`: the worker's stop
  raises the cancel); the walk's end notifies the same wake. Loops: **0 of
  500** (200 + 300) under the load, the slowest run 75 ms whole (the rig
  included); the read-shard rows 0 of 100, the fleet rows 0 of 100.
- **The gates of these items** (`timeout 3900 zig build --cache-dir
  /build/zig-cache gate --summary all`, uncached by a `clock.zig` touch):
  on item 1 alone (`9d3b27d`, load 1.3) smoke 84 / 2 skip / **1 fail — the
  handover row** (B's chmod EAGAIN, §10's attributed class), which held
  bench-, shim-, nvmet- and suites-smoke back (35/46 steps); on the landed
  tree (`8bf8742`; `e950c7b` adds a two-line comment; load 4.5 → 9.5 with
  another agent's smoke rows alongside) **46/46, 2759/2828** (unit 2521 /
  67 skip, smoke 85 / 2 skip, bench-smoke 41, shim-smoke 40, nvmet-smoke 53,
  rails 16, shim census 3 — the identity row among them; suites-smoke under
  `/build/tmp/suites-pjdfstest-20261013-103548-854379`, 1 clean run). macOS
  `zig build gate` on the same tree: **28/28, 2159/2186** (27 skipped).
