# bench/brackets — the bracket records

Every file here is a measurement produced by `scripts/bracket/bracket.sh` (or
`scripts/bracket/io500.sh`) under the charter's measurement law (§11): the
instrument, the substrate, the venue, the build profile, the date, A-B-B-A
legs ≥ 60 s each on one aging store, the ledgers (`device bytes ÷ user bytes`,
`wareq-sz` vs the block, `queued + elided ≡ terminal frees`,
`Σ admit_write_* ≡ user bytes`, the read copy ledger, `invariant_tripwires`,
the tick rescues, the solo mount's wire census, the fsync exact sum, the
engagement law) and a verdict. Nothing is hand-edited.

```
bench/brackets/<venue-id>/<row>-<date>.json   the record (schema squeezefs-bracket/1)
bench/brackets/<venue-id>/<row>-<date>.md     the same as a markdown row
bench/brackets/<venue-id>/<row>-<date>.log    the harness log
bench/brackets/<venue-id>/io500-<date>/       one io500 run: result_summary.txt, io500.out,
                                              config.ini, ledgers.json, header.json, daemon.log
```

`<venue-id>` = `<kernel>_<arch>_<cpu>_<cores>c_<disk class>`, e.g.
`7.2.6-sqz_aarch64_apple-vm_9c_virtio-blk` (the dev venue: SCOPING only,
never acceptance) or the reference box's id (acceptance: `docs/gates/brackets.md`).

## Reading a record

- `verdict`: `VALID` / `INVALID` (a ledger did not close — `invalid_reasons`
  names the leg, the check and the numbers), prefixed `SCOPING-` on a dev-box
  row. An INVALID row is kept — it is evidence of a defect, never a number.
- `headline`: the row's number per arm (`A`, `B`: mean, spread, min, max) and
  over all legs; `noise_band_pct` when A = B (the same binary twice: the
  spread IS the venue's noise band for that row); `a_over_b` otherwise.
- `legs[]`: per leg the instrument's `metrics` (fio: ops, bytes, MiB/s, IOPS,
  clat p50/p99/p99.9; mdtest: creates/s; renames.py: entries/op; tar: wire
  verbs/entry), the `ledgers` (every term listed as a delta over the measured
  span; `reclaim_absolute` at the after-snapshot), `diskstats` per backing
  device (sectors → bytes, `wareq_sz`), the `verdicts`, `loadavg` before and
  after (a loaded venue is visible, not hidden).
- `header`: instrument string, substrate chain, venue facts (kernel, cpu,
  cores, RAM, disk class, `fuse.enable_uring`), `build_a` / `build_b`
  (`squeezefs --version`: train, commit, profile, dirty), the set geometry,
  `mem_budget_mb`, the mount options, the leg timing.

## Producing a record

```
# on the venue (root; fio, OpenMPI, IOR/mdtest and io500 under /build/tools — see scripts/bracket/)
scripts/bracket/bracket.sh --scoping --substrate nvmet-loop rand-4k-read zig-out-linux/bin/squeezefs zig-out-linux/bin/squeezefs
scripts/bracket/bracket.sh --scoping --substrate nvmet-tcp  seq-read-1m-direct ./A/squeezefs ./B/squeezefs
scripts/bracket/bracket.sh --scoping --lever posture creates-mdtest zig-out-linux/bin/squeezefs zig-out-linux/bin/squeezefs
scripts/bracket/io500.sh --scoping --substrate nvmet-tcp zig-out-linux/bin/squeezefs
```

Rows: `seq-{write,read}-1m-{direct,buffered}`, `rand-4k-{read,write}`,
`shim-seq-{write,read}-1m`, `shim-rand-4k-{read,write}`, `fsync-4k`,
`creates-mdtest`, `rename-unlink`, `tar-x`. Substrates: `file` (sparse files
on /build), `nvmet-loop` (latency-shaped rows), `nvmet-tcp` on 127.0.0.1
(fabric-sensitive rows). `--mount-opts direct_device_true` is the ruler row.

Instrument provenance on the dev venue: fio 3.36 (Ubuntu 24.04), OpenMPI 4.1,
IOR/mdtest `io500-sc19-596-g2587e96` (2587e96627ed786f942d84cea570969e89691177),
io500 `io500-sc26_v1` (34a596582715934d2f7695302a7c51e8f7e73d9a) with pfind
27427343bb66f525b93d225d1f124cbed440d0b5, built with gcc 13.3 into
`/build/tools/{bin,src}` from the sources on the external disk (`/build/ext/{ior,io500}`).
