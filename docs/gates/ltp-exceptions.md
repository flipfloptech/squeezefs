# LTP — adjudicated exceptions

The ledger `scripts/suites/common.sh`'s `suite_judge` reads for `scripts/suites/ltp.sh`
(charter §11 "Gates": the release gate runs the full LTP with adjudicated by-design exceptions
ONLY). A failing test whose id is not a row here fails the gate; a row whose id did not fail in
the current run of its runtest file is reported stale and fails the gate (exit 3).

Ids are `<runtest-file>/<tag>` (`fs/fs_fill`, `syscalls/utimensat01`). Only failures that PASS
with their TMPDIR on the venue's ext4 reach this ledger: a test that fails on ext4 too is the
venue's (container limits) and is listed in `scripts/suites/ltp-venue-skip.txt` with its control
run, never adjudicated here. `reason-class` ∈ {`noatime`, `thin-provisioning`,
`i64-timestamp-range`, `by-design-<name>`}; a `by-design-<name>` row cites its decision 0002 row.

| test-id | reason-class | evidence |
|---|---|---|
