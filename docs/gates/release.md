# The release act and the versioning law

Charter §11: *"Versioning: a release train (semantic) bumped only as a
release act across all first-party components; git commit is the
fine-grained identity; the daemon and shim refuse to pair across builds."*

## The law

- **Two identities.** The *train* (`X.Y.Z`, semantic) names a release; the
  *git commit* (12 hex, `-dirty` when the tree is not clean, `unknown`
  outside a repository) names a build. Every first-party artifact carries
  both: `squeezefs --version` prints `squeezefs <train> (<commit>, <profile>
  profile, zig <version>)`; `libsqueezefs_il.so` carries the static string
  `squeezefs-il train <train> commit <commit>` (printed on its pairing
  lines; `strings` reads it); `libsqueezefs_sdk.so`'s `sqz_version()`
  returns `squeezefs-sdk <train> wire-abi <W> sdk-abi <S>`.
- **Pairing is by commit, never by train.** The daemon and the shim refuse
  to pair across builds: the IL HELLO's same-commit law
  (`src/ipc/rendezvous.zig`; the tests `src/ipc/client.zig` "e2e: every
  HELLO refusal — … a wrong commit, a -dirty commit …" and
  `src/ipc/wire.zig` "degenerate identities: unknown, empty and -dirty
  refuse the same-commit law"). The SDK pairs on its *wire ABI word* inside
  the daemon's `{cur, cur−1}` window, never on the commit (il_sdk §4; the
  test `src/sdk/session.zig` "sdk e2e: the ABI window").
- **The train moves only as a release act**, one commit on `dev` tagged
  `v<train>`, written by `scripts/release.sh`. It touches every
  train-bearing word at once: `build.zig`'s `train_version` and
  `build.zig.zon`'s `.version` (build.zig refuses to configure when they
  disagree), and — only when the act says the ABI moved — the SDK's words:
  `--wire-abi-bump` advances `wire.SDK_ABI_CURRENT`, widens the accepted
  window to `{cur, cur−1}` and moves the header's `SQZ_ABI_WIRE`;
  `--sdk-abi-bump` advances `version.SDK_ABI`, the header's `SQZ_ABI_SDK`
  and `libsqueezefs_sdk.so`'s version minor (SONAME `libsqueezefs_sdk.so.1`
  stays: the C surface evolves additively). The shim's SONAME is
  `libsqueezefs_il.so` with no number: a build-paired library has nothing
  for a number to promise.
- **The two build profiles name themselves**: `gate` (ReleaseSafe + thin
  LTO; what the gate runs) and `dist` (ReleaseSafe + fat LTO, debug info
  split into `squeezefs.debug` with a `.gnu_debuglink`). A release ships
  `-Dprofile=dist dist`.

## The act

```
scripts/release.sh 0.2.0 --dry-run                      # the transcript below
scripts/release.sh 0.2.0                                # the act: ONE commit on a clean dev, tagged v0.2.0
scripts/release.sh 0.3.0 --wire-abi-bump --sdk-abi-bump # when the frozen subset / the C surface moved
```

The script (1) refuses a train that does not advance, a branch other than
`dev`, a dirty tree; (2) edits the words; (3) verifies on the Linux venue:
builds `install shim sdk`, checks the three identities name the train, runs
the pairing-refusal tests and the ABI-window test, builds `-Dprofile=dist
dist` and checks the debuglink, the debug file and the dist binary's
profile line; (4) commits and tags. `--dry-run` does (2)–(3) in a throwaway
worktree under `.release-dry-run/<train>` (inside the repo so the venue sees
it) and removes it: nothing is committed.

An ABI bump is a code change, not just a number: the dry run
`scripts/release.sh 0.2.0 --dry-run --wire-abi-bump --sdk-abi-bump`
(2026-10-10) moved `SDK_ABI_CURRENT` 1 → 2 with the window `{2, 1}`,
`SDK_ABI` 1 → 2, both header words and the `.so` minor 0 → 1, and the
verification then FAILED the SDK window test (`sdk e2e: the ABI window`
pinned the window's numbers) — the act refuses: the window tests' new values
land in the same commit as the bump, written by hand before the act. Since
`ecec435` the e2e fixture derives every expected value from
`wire.sdk_abi_accepted` and `wire.IPC_ABI`; what still pins today's set by
hand are `wire.zig`'s and `version.zig`'s "today's set is {1}" tests — the
act's deliberate edits.

What the act does NOT do: run the release gate. `zig build gate-release
-Drelease-gate=true` (the per-commit gate with environment skips turned into
failures, then `bench-compare` against the committed baseline) is the owner's
call on a quiet venue before the act; pjdfstest / LTP / fstests are the
external suites the charter names for the release gate and run outside this
tree.

## The dry run transcript (train 0.1.0 → 0.2.0, 2026-10-10)

```
== release act: train 0.1.0 -> 0.2.0 (dry-run=1 wire-abi-bump=0 sdk-abi-bump=0)
== dry run: editing the throwaway worktree …/.release-dry-run/0.2.0 (HEAD 36dc8f6765c8)
== edits
 build.zig     | 2 +-
 build.zig.zon | 2 +-
-const train_version = "0.1.0";
+const train_version = "0.2.0";
-    .version = "0.1.0",
+    .version = "0.2.0",
== verification (Linux venue)
-- squeezefs --version:
squeezefs 0.2.0 (36dc8f6765c8-dirty, gate profile, zig 0.17.0)
-- the shim's identity:
squeezefs-il train 0.2.0 commit 36dc8f6765c8-dirty
-- the SDK's sqz_version text:
squeezefs-sdk 0.2.0 wire-abi 1 sdk-abi 1
-- the daemon<->shim pairing refuses across builds (the existing tests):
+- run test squeezefs-unit 42 pass (42 total) 60ms MaxRSS:141M
-- the dist profile: split debug info + debuglink:
  [     0]  squeezefs.debug
squeezefs 0.2.0 (36dc8f6765c8-dirty, dist profile, zig 0.17.0)
== dry run complete: no commit, no tag; the worktree is removed on exit
```

(`-dirty`: the dry-run worktree's commit is the act's edits, uncommitted —
the real act commits before anyone builds from it.)
