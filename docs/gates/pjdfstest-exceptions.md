# pjdfstest — adjudicated exceptions

The ledger `scripts/suites/common.sh`'s `suite_judge` reads (charter §11 "Gates": the release
gate passes pjdfstest with adjudicated by-design exceptions ONLY). A failing subtest whose id is
not a row here fails the gate; a row whose id did not fail in the current run is reported stale
and fails the gate too (exit 3) — the ledger never rots.

Ids are `<dir>/<NN>.t#<subtest>` as `prove -rv` folds them. `reason-class` ∈ {`noatime`,
`thin-provisioning`, `i64-timestamp-range`, `by-design-<name>`}; a `by-design-<name>` row cites
its decision 0002 row.

| test-id | reason-class | evidence |
|---|---|---|
