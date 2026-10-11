# fstests (xfstests) — adjudicated exceptions

The ledger `scripts/suites/common.sh`'s `suite_judge` reads for `scripts/suites/fstests.sh`
(charter §11 "Gates": the release gate runs the full fstests `-g auto` with adjudicated by-design
exceptions ONLY; `_notrun` rows are not failures). A failing test whose id is not a row here
fails the gate; a row whose id did not fail in the current run is reported stale and fails the
gate (exit 3).

Ids are xfstests' own (`generic/003`). Tests excluded in the container venue for reasons that
are not a filesystem semantic (no loop devices, no dm, …) live in
`scripts/suites/fstests-venue-exclude.txt` with their reasons, never here. `reason-class` ∈
{`noatime`, `thin-provisioning`, `i64-timestamp-range`, `by-design-<name>`}; a `by-design-<name>`
row cites its decision 0002 row.

| test-id | reason-class | evidence |
|---|---|---|
| generic/192 | noatime | atime is never stamped by a READ: the data plane's read ladder (charter §4 try-only warm ladder, zero-copy serves) performs no inode write per read, and the kernel sends no SETATTR for atime on a FUSE mount — the charter's adjudicated `noatime` class; the test measures `delta1 == 0` where it expects 5–7 s |
