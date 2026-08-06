# 2026-08-06 — The read ceiling verdict: whole-box CPU wall (passes-per-byte at box scope)

**Venue:** squeeze-test, pair `a17f625d` (fan-out live, lanes=6). Cold 128 GB
16-stream bs=1M row at 21.8 GB/s: **idle 8.0 %, %sys 50.4 (≈16 cores),
%soft 6.3, daemon 1,194 %CPU (≈12 cores)** — the 32-core client is ~92 %
busy. The four-round ladder (touch-cadence → hold truth → completion
refill → submission fan-out) was each mechanically verified and none moved
the number: **every concurrency fix was correct and irrelevant, because
reads are aggregate-CPU-bound**, not queue-bound (no single thread
saturates — round 1's "hottest thread 34 %" misled; the box does).

Closing arithmetic: ~2.7 CPU passes/byte (the standing read census) ×
22 GB/s ≈ 60 GB/s of memory traffic + protocol work ≈ this box. Writes
reach 34 GB/s because nvme-tcp TX is zero-copy spliced (~1 fewer pass);
the raw row reaches 41.8 because libaio pays ~1 pass and no FUSE.

**The road to the 85 % bar (35.5 GB/s) is the copy-elimination program**
(user ruling 2026-08-06: BOTH, in sequence):
1. **The serve dest-copy kill first** (1.00 passes/byte — every served
   byte memcpy'd from the tier/fill buffer into the reply ring buffer;
   the A1 tier-buffer-lease class, now priceable via the warm/cold ledger
   split). Target: `read_copy_dest_bytes` → ~0 on lease-served rows.
2. **zcrx engagement second** (0.69 passes/byte — the kernel RX copy;
   the lane is correct-and-safe after 8 rounds, parked only on
   engagement economics, which a CPU-bound box inverts: every zero-copy
   RX byte is direct read capacity).

Fan-out verdict for the record: field-PAR (FAN-D16 22.51 vs ONE-D16
22.85, lanes=6 live) — the RX-core spread thesis was falsified in the
field by this CPU wall sitting in front of it; the fan-out stays (correct,
derived, +44 % raw headroom on non-CPU-bound hosts).
