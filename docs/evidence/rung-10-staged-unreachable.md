# Rung 10 — a staged read served zeros for a promoted file (the unreachable-payload window)

**The symptom.** xfstests generic/127 with `SQUEEZEFS_FSYNC_PROMOTE_STAGED=on`, 1 of 8 runs on
`082761b`: fsx_std_nommap (seed 191110531) `READ BAD DATA: offset = 0x22161, size = 0xc690`, zeros
for the whole grain from 0x27000; the TEST set's daemon log holds exactly one line, `staged
payload unreachable for ino 8: serving 4096 zero bytes at offset 159744 (another client's live
staged custody, or a crash discarded it)` — a single-writer local mount, no crash. The 4 KiB
request is the kernel's read of a page a partial write (op 22152's copy_file_range) had left
not-uptodate; the bytes were acked.

**The window.** `ReadPath.readStaged` resolved the layout (staged, file id F), asked the ring for
`(F, ino)` and, on `.absent` with no durable copy, served size-consistent zeros — the "STABLE lost
identity" arm — without re-resolving. The reader takes no lock. A promotion (`promoteStaged` →
`pack.commitLocked`) commits the packed layout and then releases the ring entry under (3.5)+(3.7),
which a reader never takes; a read that resolved before the commit and looked up after the
release met "layout staged F, ring without F" and answered zeros. The reference's §1.3 has the
line the port dropped — "fresh identity moved ⇒ re-resolve" before the in-flight and lost
verdicts (`docs/porting/data_read_path.md`); the port's `in_flight` arm (ring gone, ledger row
still there) re-resolved, the fully released state fell through. The try-only warm ladder is not
exposed: its ring miss is a Demote to the lane. Who promoted fsx's file: fsx_std_nommap never
fsyncs, but a host-wide `sync(2)` reaches every FUSE mount as FUSE_SYNCFS and `kv_fs.syncfs` under
the lever promotes every resident ring entry; its times drain takes the ino's exclusive guard
first, which lines the kernel's one-page READ up right behind the promotion. Nothing in the suite
syncs — another actor on the shared workstation did; the window is the daemon's whatever triggers
the promotion (the file's fsync, pressure, syncfs).

**The fix** (`src/data/read_path.zig`). An ABSENT ring copy is judged only against a resolve
taken AFTER the lookup: a moved kind re-dispatches (the packed tenant, the own block, inline), a
moved file id or durable copy re-resolves and retries (`staged_identity_retries`), and the lost
arm needs the SAME staged identity on both sides of the miss. The other-client law stands
(`staged_payload_lost_reads`, stated and counted); the warning stays a warning, not a tripwire —
the read path cannot tell a single-writer set from a multi-client one, where the arm is legitimate.

**The seam.** `SQUEEZEFS_TEST_STAGED_READ_HOLD=<fifo>`: a staged read parks between its identity
resolve and its ring lookup until a writer opens the FIFO. The hand rig (`~/tmp/sqz-staged-hold.sh`:
the lever on, `SQUEEZEFS_DIRECT_DEVICE_TRUE=1` so an O_DIRECT page read is demoted to the lane, a
256 KiB file, `dd iflag=direct` of page 39 = 0x27000 parked, `sync -f` from another CPU's queue,
the FIFO opened, the page compared): unfixed 5 of 5 rounds BAD DATA, each the suite's exact
warning; fixed 5 of 5 ok, 0 warnings, every file promoted by the sync.

**The red tests.** `data.read_path` "a staged read whose identity moved between its resolve and
its ring lookup re-resolves instead of serving zeros": the write-side double applies the move at
the lookup — (1) the promotion: red `byte 1 (dev 1048577): got 0 want 1` with the warning, green
the tenant's bytes and `staged_payload_lost_reads` unchanged; (2) a re-stage under a fresh id: the
fresh copy; (3) the same identity still absent after the fresh resolve: zeros, counted, the
verdict's resolve after the lookup (`file_calls` + 2). The smoke row "THE STAGED READ'S IDENTITY
WINDOW" is the hand rig as a gate row (the read and the sync pinned to different CPUs' queues):
the page equal, `fsync_promoted_files` 1, `layout_promoted_packed` 1, `staged_payload_lost_reads` 0.

**Runs.** data.read_path 61/61, data.staging 57/57, core.knobs 52/52, rails green, the smoke row
green. Hand loops on the fixed binary (`~/tmp/sqz-staged-loop.sh`: generic/127's six fsx instances
in parallel on one set, a host `sync` every 100 ms): lever on 20 of 20 clean, lever off 10 of 10
clean, 0 `unreachable` lines. `082761b`'s binary went 30 of 30 clean without the sync loop and 10
of 10 with it — the race needs the read lined up behind the drain's guard; the seam holds it.

**Open.** `-o direct_device_true` reaches the stats row only; the read path's ruler is the env
knob (out of scope). The ring's placement judges only indexed and pending extents live: a retired
cell a reader still leases could be overlaid and its reclaim punch the newcomer's bytes —
unreached here (the cursor moves forward through a near-empty ring), noted for the ring's owner.
