# 2026-08-07 — perf-script output cleanup (`SQZ_DEBUG` convention)

**Chore, not a measurement.** User ruling: the perf rigs' DEFAULT output is
the clean summary format of the original scripts; the accumulated verbose
stream becomes an opt-in debug mode. One shared convention across
`tests/fio/{exa_client_perf,run_fio_row,perf_session,d12_session}.sh`
(documented in each header + `tests/fio/README.md` §Output convention):

* **default** — clean summary only. `exa_client_perf.sh`: one-line run
  header, `[i/N] row bs pass ...` progress lines, the side-by-side table,
  the report path. `run_fio_row.sh`: the ROW line (engagement inline on
  shim rows) + the amplification column. Session rigs: section headers +
  one result line per row.
* **`SQZ_DEBUG=1`** — the previous full verbose stream (banner header,
  per-pass runner output, stats-delta listings, cold-discipline verdicts,
  hints, verdict-law footer, fio chatter) on **stderr**; stdout is
  byte-compatible with default mode.
* **failures are ALWAYS verbose** — engagement gate trips dump the gate
  arithmetic + stats-delta + artifact paths, tripwire trips dump the full
  stats delta, fio errors dump the fio log, remount failures dump the
  mount/daemon log; the exa wrapper dumps a failing pass's captured
  runner output verbatim in default mode.

Gate SEMANTICS untouched (diff-audited): KD-7 screen, engagement math +
`SQZ_FIO_ENGAGE_MIN`, rc=4/`FAILED_HARD` exit-code law, the must-stay-flat
tripwire abort, cold-remount restore verification — presentation only. The
exa `$REPORT` artifact still persists the full banner + table + footer.

## Verification (local, D15 — loop devsub, dist-pair e0d0575c, fio-3.42; SMOKE-labeled rows, not quotable)

`bash -n` clean ×4. `exa_client_perf.sh` end-to-end both modes
(`--rows randwrite,randread --njobs 4 --size 256m`, `SQZ_EXA_RUNTIME=8`):
default = clean stdout, **0 bytes stderr**, exit 0; debug = identical
stdout, 58-line verbose stream on stderr; report artifact intact.
Clean-mode capture:

```
exa_client_perf: host=strixhalo mount=/tmp/sqz-perfout-mnt daemon=e0d0575ce50a... (KD-7 shim verified) mode="SMOKE override (8 s + 10 s ramp — plumbing proof, not a quotable row)" rows=randwrite,randread instrument="fio-3.42"
[1/4] randwrite 4k kernel (libaio qd=8 njobs=4 in-flight=32, 8s) ...
[2/4] randwrite 4k shim (libaio qd=8 njobs=4 in-flight=32, 8s) ...
[3/4] randread 4k kernel (libaio qd=8 njobs=4 in-flight=32, 8s) ...
[4/4] randread 4k shim (libaio qd=8 njobs=4 in-flight=32, 8s) ...

row        bs  kernel (engine)          shim (engine)              delta  shim engagement        verdict
--------------------------------------------------------------------------------------------------------
randwrite  4k     24.6 kIOPS (libaio)      39.5 kIOPS (libaio)    +60.6%  2.420 (>=0.90 OK)      shim OK
               kernel: in-flight 32 (njobs 4) | clat_mean 1.292 ms
               shim: in-flight 32 (njobs 4) | clat_mean 0.805 ms
randread   4k     46.6 kIOPS (libaio)     142.5 kIOPS (libaio)   +205.7%  2.098 (>=0.90 OK)      kernel WARM (label-only); shim WARM (label-only)
               kernel: in-flight 32 (njobs 4) | clat_mean 0.682 ms | WARM (label-only)
               shim: in-flight 32 (njobs 4) | clat_mean 0.223 ms | WARM (label-only)
report: /tmp/fio_rows/20260807_143837_exa_client_perf/exa_client_perf_20260807_143837.txt
```

Forced-failure smokes, DEFAULT mode: (a) wrong mountpoint → loud `FAIL`,
exit 1; (b) natural engagement trip (`--rows write_bw`, bs=1M shim libaio
at the derived 64 KiB slot slab → passthrough) → stdout stays the clean
table with `INVALID (passthrough)`, stderr carries the full diagnostic
block (gate arithmetic `0.000 (<0.90 INVALID)`, total_ios, stats-delta
summary + path, fio log path, runner ROW line, single-slot geometry hint);
exit 0 (rc=4 is not `FAILED_HARD` — unchanged). Standalone runner default
= one ROW line, 0 bytes stderr; runner fio-error path (bogus engine)
verbose + exit 1; `--emit-only` plan output unchanged. Markdown link
check PASS (212 files, 0 broken).

Environment note (not a script issue): a mount without `--allow-other`
refuses every shim HELLO as `Flags` — the daemon's own §5.2 `fstat` on
the received credential fd gets EACCES from the kernel FUSE user_id
restriction (strace-confirmed). Interception rigs need `--allow-other`
when client uid ≠ daemon uid (here: sudo-launched daemon, uid-1000
clients).
