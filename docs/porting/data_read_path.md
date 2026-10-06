# The data read path — porting spec (rung 4: tiers, admission, single-flight, prefetch, ranged + dest lease, zc serve)

Source of truth (reference, read-only): `/Users/joberdorf/Source/squeezefs`.
Primary files and the line ranges this spec was extracted from:

| File | Lines | What lives there |
|---|---|---|
| `src/routing.rs` | 19499–21140 | `read_file_range_zero_copy_with_meta` — the ONE read decision (layouts, overlays, the striped single-block arm, every tier probe, the zc direct leg, ranged dispatch, raw dest leg, the fetch loop) |
| `src/routing.rs` | 12235–12871 | `get_cached_or_fetch_block_traced` — the result-carrying single-flight fill + the R1b admission site |
| `src/routing.rs` | 14379–14680 | `get_block_for_index_class` — the binding-validated walk, the rebind ladder, the serialized settle |
| `src/routing.rs` | 14858–15210 | `ranged_eligible`, `ranged_escalation_candidate`, `get_block_range_for_index`, `slice_whole_for_ranged` |
| `src/routing.rs` | 5965–6658, 7235–7603 | `TierAdmission`, `GhostTable`, `EscalationCooldown`, `AdmissionGovernor`, `StreamLane{s}`, `StreamActivityGauge`, the pure window functions, `FillClass`, `ReadClassHint`, `SyncServeArm` |
| `src/routing.rs` | 12922–14156 | `pipeline_touch`, `ring_read_lane_touch`, `spawn_prefetch_task`, `read_lane_top_up`, `spawn_read_lane_task`, `lane_fetch_block`, `hold_serve_admission` |
| `src/routing.rs` | 18192–18445 | `try_read_range_sync` — the SYNC warm ladder the transport probe calls |
| `src/fuse_client.rs` | 33724–34396, 26345–26477, 16195–16288, 9070–9202 | `read_impl`, `read_fast_probe`, `ipc_read_probe_locked`, `IpcReadProbe`/`FastReadSink` |
| `src/tiering/memory.rs` | 1–588 | the sharded clock (`MemoryCache`, `EntryState`, `EvictClass`) |
| `src/cache/lru.rs` | 1–453 | `LruCache` (eviction channel, `PutClass`, Yellow freeze, Red shed, governor report) |
| `src/cache/mod.rs` | 17–391 | `TieredCache`, the budgets, the dehydration workers, `purge_block_key` |
| `src/tiering/nvme.rs`, `src/cache/nvme.rs` | 1–716; 1055–1235, 2260–2473 | the NVMe read cache: mmap segment format, `put_impl`, the shard-lock invariant, `shard_plan`, the validated publish façade |
| `src/read_lane.rs` | 1–834 | the hold + the ahead lane's engage governor |
| `src/write_pipeline_core.rs` | 118–310 | `ProbeCore` (reused verbatim by the ahead lane) |
| `src/cache/pool.rs` | 300–838 | `ReadBlockValue`, the slab pools, the memfd `ZC_FILL_POOL`, `read_bounce_pool` |
| `src/nvme_dev.rs` | 154–210, 2682–2810 | `DestToken`/`DestResolver` (MEM-1), `read_block_with_dest_inner` |
| `src/incarnation_core.rs` | 1–90 | the seqlock word protocol |
| `crates/fuse3/src/raw/connection/fast_dispatch.rs` | 1–80 | the transport's arm (a) |
| `docs/design-read-path.md` | whole (622) | the R1–R5 program design + every amendment |
| `.benchmarks/2026-07-11-elbencho-odirect-read-smallblock-attribution.md`, `2026-07-15-hybrid-io.md`, `2026-08-01-read-lane.md`, `2026-08-06-read-dest-lease.md`, `2026-09-05-r4-read-zc-serve.md` | — | the measured numbers |

Line refs below are `routing.rs:N` etc. unless prefixed with a path.

Port seams this spec binds to:
* `src/fuse/transport/filesystem_vtable.zig:122–127` — `probe_read(ctx, req, window) ?Reply` (arm (a), §10).
* `src/fuse/transport/filesystem_vtable.zig:77–85` — `Handle.replyFdBody(fd, off)` (the zc fd-source bridge, §8) and `Handle.replyPrefilled(n)` (the direct-leg header-only commit, §1.2 step 11).
* `src/fuse/transport/filesystem_vtable.zig:70` — `Handle.window: []u8` (the registered reply window = the dest, §3).
* `src/fuse/handler/op_guard.zig:366` — `Table.tryAcquire` (rung 1 of the warm ladder).
* `src/io/nvme_dev.zig:1343–1433` — `readBlock` / `readBlockOutcome` into a pooled `Buf`; `:121–159` the `RangedSlab` registered as fixed buffer 0; `:166–191` the `BlockPool`; `:68–79` `Buf.Kind{none, ranged, block, adhoc}`. Rung 4 adds the caller-dest form (§3.5).
* `src/mem/budget.zig:389–431` — `Component{name, source, floor, weight, shed, kernel_reclaimable}`; `:484` `register`.

Charter bindings: §6.3 (the whole section), §4 READ dispatch law arm (a), §6.2 "Memory budget authority", §1.3, §1.5, §1.8, §10, §12.
Decision 0002 governs every row of §13.

---

## 0. Vocabulary (one definition each)

* **block key** — the device-offset string `[be_id://]offset` naming a 4 MiB striped block (`routing.rs:14266–14275`). Freed + reallocated offsets REUSE the key string — the whole validation lattice exists because of this.
* **incarnation word** — one `AtomicU64` per offset, `gen << 1 | stable` (`incarnation_core.rs:3–10`). `snapshot` returns `Some(w)` iff stable (`:60–67`); `still(word, before)` = unchanged, with an `Acquire` FENCE ordering the payload read before the validation load (`:69–80`; Boehm's seqlock-reader rule — x86/TSO masks it, ARM/POWER do not).
* **binding** — the CURRENT map's `block_index → key` (`current_block_binding`, `routing.rs:14181`; `block_binding_is`, `:14215`).
* **fill** — a device fetch of whole-block bytes under a key. **Validated fill** = incarnation stable before the DMA, unchanged after, deposited only through an insert-time `validate` closure (finding 17, `:12564–12577`).
* **serve** — handing bytes to a reader for block `b`. **Binding-validated serve** = fill valid ∧ the current map still binds `b → K` AFTER the bytes are in hand (`:14277–14287`).
* **tier** — a block-key-addressed byte store. Five exist in the reference: hot tier (RAM clock), read LRU (≤ 256 KiB RAM clock), read-lane hold (RAM FIFO), NVMe read cache (mmap ring), GDS file cache. ALL are arms of the ONE purge (`cache/mod.rs:374–391`).
* **R1a / R1b / R2 / R3 / R4 / R5** — the design's mechanism names (`docs/design-read-path.md:35`): result-carrying single-flight / scan-resistant admission / prefetch pipeline / ranged reads / hot tier / memory budget.
* **dest** — the request's own registered reply window as `ReadDest{addr, cap}` (`routing.rs:6739–6801`). **Dest lease** = a cold sub-block window DMA'd straight into it (`:6666–6675`).
* **cohort** — every reader of one block key whose fill is in flight; served from the primary's `FillResult` (§4).
* **lane** (two meanings, kept apart): a `StreamLane` = one of K = 4 per-file sequential cursors (§5.1); the **read lane** = the hold + the ahead issue path (§2.3, §5.5).

---

## 1. The read decision — ONE ordered procedure

Entry: the FUSE READ handler `read_impl(ino, fh, offset, size, flags)` (`fuse_client.rs:33724`), reached after the transport's arm (a) probe declined (§10) or on a classical delivery. Every step names its counters.

### 1.1 Prelude (the handler; lock-order level (1) held for a SHORT section only)

```
read_impl(ino, fh, offset, size, flags):
  fuse_ops += 1                                                 // :33733
  odirect = flags & O_DIRECT                                    // :33748
  odirect ? read_odirect_requests += 1
          : (unique != 0) ? note_page_instantiation(ino)        // page-coherence latch BEFORE the reply, :33752–33759
  hint = {odirect, dest_arena: arena_override?, lane_pre_fed: unique == 0, dest_lease: false}   // :33772–33777
  if virtual_ino(ino): serve the frozen payload / regenerate; EOF ⇒ empty      // :33779–33820
  if device_overlay::any_open_fast():                           // ONE relaxed gauge load, :33827
      match try_serve_overlay_read(ino, offset, size):          // covered → dest, gaps → base/zeros
        Some(Ok(buf)) ⇒ reply buf
        Some(Err)     ⇒ errno
        None          ⇒ drain_device_overlays_range(ino, offset, offset+size)   // :33839–33846
  test_stall_park(read_window_stall)                            // SQUEEZEFS_TEST seam, :33852–33856
  serve_t0 = now                                                // read_serve_phase_ns anchor, :33863
  {  guard = inode_lock(ino).read()                             // level (1), SHORT, :33875
     size = attr_cache[ino].size ?? backend.getattr(ino)?.size  // FAIL LOUD — the old unwrap_or(0) read a dead ino as full-length zeros (generic/795), :33876–33899
     if meta = router.metadata_cache.get(ino): size = meta.size // the write path updates it synchronously; durable caches lag, :33900–33919
     if offset ≥ size ⇒ reply empty
     read_len = min(size, file_size − offset)
     if single block:
        if buf = active_block_buffers[ino, b]:                  // the accumulation buffer — write-side authority, :33941
           extent repr ⇒ compose (runs over base; deferred & !covered ⇒ base read)      // :33953–33984
           !contained ∧ !deferred ⇒ zeros + covered runs in a FRESH buffer (never mutate the shared one)   // :33999–34019
           contained ⇒ CoW snapshot slice (zero copy)            // :34020–34031
           deferred ⇒ under BLOCK_FLUSH_LOCKS(ino, b, ReadSeed): fetch_seed_image, fill_complement_from, serve  // the reader pays the deferred read, :34033–34100
     else active_hit = true
  }  // guard dropped — NEVER held across flush or backend I/O (:33870–33873)
  if active_hit: flush_active_blocks_with_retry(ino, token); meta = metadata_cache.get(ino)   // :34110–34122
  dest = arena_override ?? conn.get_payload_buffer(slot) → ReadDest{ptr, cap}   // FUSE-4e: the LENGTH rides along, :34136–34149
  hint.dest_lease = dest ∧ !hint.dest_arena ∧ router.dest_lease_enabled()        // :34156–34157
  zc_serve = conn.zc_armed ∧ slot.is_ring ∧ !arena ? ZcReadServe::for_connection(conn, slot) : None   // no alloc, :34169–34173
  bindings_before = read_custody_fingerprint(meta, …)           // :34213–34215
  loop:                                                         // the OVERLAY-NEVER-INVISIBLE loop, :34226–34360
     pre_runs = capture_parked_runs(ino, offset, read_len)
     zc_pass = zc_serve iff zc_enabled ∧ pre_runs.is_empty()
     data = router.read_file_range_zero_copy_with_meta(path, offset, read_len, dest, hint, meta.take(), zc_pass)   // §1.2
     if data.is_empty() ∧ zc_pass.served():                     // the direct leg landed bytes in the caller's pages
        post_runs; overlay_live = device_overlay_blocks_in_window; bindings_after      // registry BEFORE fingerprint, :34271–34277
        all clean ⇒ break
        zc_enabled = false; bindings_before = bindings_after; continue   // NOT counted against attempts, :34284–34289
     post_runs = capture_parked_runs; data = apply(pre_runs); data = apply(post_runs)
     overlay_blocks = device_overlay_blocks_in_window(ino, offset, read_len)        // SOURCE registry first
     test_stall_park(read_validate_stall)
     bindings_after = read_custody_fingerprint(None, …)                              // DESTINATION fingerprint second — the ORDER LAW, :34304–34316
     overlay_blocks.is_empty() ∧ fp_matches ⇒ break
     overlay_blocks.is_empty() ∧ attempts < 2 ⇒ attempts += 1; bindings_before = bindings_after; continue   // benign churn, :34332–34343
     overlay_window_escalations += 1                            // :34352–34354
     data = read_window_settled(ino, path, offset, read_len)    // under (3) + (3.5), :34355–34358
     break
  read_serve_phase_record(PostValidate, router_done_at) → done; record(Total, serve_t0, done)   // ONE clock read ends both, :34361–34364
  if data.is_empty() ∧ zc_serve.served() = n ⇒ reply zc_prefilled(n)                 // :34368–34377
  zc_fd_body = zc_serve ∧ !data.is_empty() ∧ zc_fill_fd_offset(data.ptr, data.len)   // re-probed at reply time, :34386–34389
  reply {data, zc_fd_body}
```

Phase family `read_serve_phase_ns` (`fuse_client.rs:4102–4124`): `prelude, meta_resolve, key_resolve, classify_probe, sf_wait, block_fetch, binding_check, slice_out, post_validate, total` — span form: each `record` returns its clock read and the abutting phase starts from it (a warm serve reads the clock 9 times, not 13).

The ORDER LAW (`:34304–34316`): every settle publishes its destination strictly BEFORE retiring its source (map move `M` before record retire `R`), so the reader probes the SOURCE registry first and the DESTINATION fingerprint second — a miss then requires probe-post-`R` ∧ fingerprint-pre-`M`, i.e. `R < M`, a contradiction. Pinned by `settle_inside_the_validate_gap_never_hides_acked_bytes`.

### 1.2 The router: `read_file_range_zero_copy_with_meta` (`routing.rs:19520`)

```
read_file_range_zero_copy_with_meta(path, offset, size, dest, hint, meta_hint, zc):
  READ_BUDGET (thread-local): every 128th op yields                      // OQ-5 lost-wakeup, :19545–19555 (§13 Q4)
  dest_addr = dest.checked_ptr(size)                                     // FUSE-4e; refusal ⇒ read_dest_overruns, loud, every leg copies, :19568–19571
  meta = meta_hint iff fresh_or_dirty else fetch_metadata(path)          // phase meta_resolve, :19585–19589
  for attempts in 1..=64:                                                // staged identity may move mid-read, :19612–19620
    match meta.file_type:
      "inline"  ⇒ data_key[offset..] zero-padded to want = min(size, meta.size − offset)      // :19622–19637
      "staged"  ⇒ §1.3
      "striped" ⇒ §1.4
  Err(InvalidOperation "identity kept moving")
```

### 1.3 Staged layout (`:19638–19847`)

```
  file_id = meta.file_id?                                                // :19639
  if guard = nvme.read_staged_zero_copy(file_id):                        // ring entries always hold the identity's CURRENT payload
     cache_hits += 1; want = min(size, meta.size − offset)
     rider_runs = staged_extent_runs_in(path, 0, offset, offset+want)    // W2 rider: NEWER than the ring image, :19664–19669
     rider ⇒ compose into `out` (dest copy or Bytes)
     else dest ⇒ memcpy + zero tail; phys == want ⇒ Bytes::copy; else zero-padded out
  else if bk = staged_block_mapping(path, meta):                         // promoted/spilled durable image, :19733
     window = read_mapping_window(bk, offset..offset+want)               // ONE ranged routed window (LBA-aligned on passthrough), :19747–19749
     still_bound = freshest_layout_identity: staged ∧ same file_id ∧ block_map[0] == bk    // :19762–19770
     Ok ∧ still_bound ⇒ out[..got] + W2 rider runs
     Err ∧ still_bound ⇒ classify_bound_mapping_error (real I/O error; a stale-lifetime refusal on a bound mapping = contradiction ⇒ EIO + invariant_tripwires)
     else staged_identity_retries += 1; meta = fresh; continue
  else if fresh identity moved ⇒ staged_identity_retries += 1; meta = fresh; continue          // :19806–19818
  else if nvme.staged_generation(file_id).is_some() ⇒ retries += 1; attempts ≤ 4 ? yield : sleep 2 ms; continue   // in-flight stage, :19820–19834
  else STABLE lost identity ⇒ note_lost_staged_payload; zeros of lost_staged_range_len   // D0 degrade, loud, :19835–19847
```

### 1.4 Striped, single block (`start_block == end_block`, `:19848–21140`)

```
  b = start_block; slice_start = offset − b × bs; slice_len = end_offset − offset       // :19866–19869
  cache_key = active_block(ino, b)                                                       // stack key, :19875–19879
  // (1) W2 extent-record runs — NEWER than every base
  ext_runs = staged_extent_runs_in(path, b, rel_s, rel_e)                                // :19889–19890
  if !ext_runs.is_empty():
     fully_covered ⇒ runs only
     else base = nvme.read_staged(cache_key) ?? get_block_for_index(path, b, key, false, true)     // :19906–19929
     overlay runs; dest copy or Bytes; return
  // (2) the staging mmap ring (the write-side custody station)
  if guard = nvme.read_staged_zero_copy(cache_key): dest copy / Bytes copy; return       // :19955–19982
  // (3) key resolve straight out of the live map — phase key_resolve
  key = match block_key_in(meta, b): Key(k) ⇒ k | Hole ⇒ None | NeedsAuthority ⇒ load_striped_block_keys(...).pop()   // :19996–20012
  probe_t0 = record(KeyResolve)
  device_true = direct_device_true() ∧ hint.odirect                                      // :20023
  device_true ⇒ read_device_true_reads += 1
  dest_leaseable = hint.dest_lease ∧ !device_true ∧ slice_start % 4096 == 0 ∧ slice_len % 4096 == 0
                   ∧ slice_len < bs ∧ dest_addr % 4096 == 0 ∧ crypto.is_passthrough()  // :20044–20050
  zc_geometry = zc ∧ !device_true ∧ slice_len > 0 ∧ slice_start % 4096 == 0 ∧ slice_len % 4096 == 0
                ∧ slice_start + slice_len ≤ bs ∧ passthrough                             // sub-block OR whole-block, :20066–20072
  // (4) the pipeline driver — once, BEFORE the serve probes
  if !device_true ∧ !hint.lane_pre_fed:
     will_wait = key ∈ inflight_block_reads                                              // the reader caught the pipeline
     pipeline_touch(path, meta, bs, offset, len, b, b, will_wait, key, claim = true, dest_leaseable ∨ zc_geometry)   // §5.3, :20083–20100
  // (5) HOT tier — refcount hit, binding recheck
  if key ∧ !device_true ∧ hot = hot_block.get_serving(key, slice_len):                   // credits payback, :20113–20115
     slice_t0 = record(ClassifyProbe)
     fd_source = zc_fd_source(zc, dest_addr, &hot[start..end])                           // §8.1
     data = fd_source ? hot.slice : dest ? {serve_copy_to_dest (NT-policied); read_copy_dest_bytes += len; read_copy_warm_serve_bytes += len; read_copy_hot_serve_bytes += len; nt_read_serve_bytes if NT ran; dest_bytes(dest, len)} : hot.slice
     bind_t0 = record(SliceOut); still = block_binding_is(path, b, key); record(BindingCheck)
     still ⇒ cache_hits += 1; hot_block_hits += 1; fd_source ⇒ note_zc_pool_serve(len, warm = true);
             hint.odirect ⇒ read_odirect_tier_serves += 1; read_lane.enabled ⇒ hold.credit(key, len); return data   // :20185–20209
     else stale_binding_rebinds += 1 (fall through)                                      // :20211–20218
  // (6) the read-lane HOLD — the deep-qd cohort-stability serve, strictly after hot
  if read_lane.enabled ∧ key ∧ !device_true ∧ (held, ledger_visible) = hold.serve_with_provenance(key, 0):   // :20232–20236
     same copy / fd / recheck shape; ledger: read_copy_hold_serve_bytes
     still ⇒ cache_hits; fd ⇒ note_zc_pool_serve(warm); read_lane_serves += 1; read_lane_serve_bytes += len; odirect ⇒ read_odirect_tier_serves;
             hold.credit(key, len); ledger_visible ⇒ hold_serve_admission(key, held, class).await (§6.5); return      // :20301–20345
     else stale_binding_rebinds += 1
  // (7) NVMe read cache — the mmap guard is COPIED OUT before any await
  if key ∧ !device_true ∧ guard = nvme.get_cached_read_block_range_zero_copy(key, slice_start, slice_len):     // :20366–20373
     cache_hits += 1; slice_t0 = record(ClassifyProbe)
     dest ? {serve_copy_to_dest; read_copy_dest_bytes; read_copy_warm_serve_bytes; read_copy_cache_serve_bytes} : {read_copy_bounce_bytes; Bytes::copy}
     drop(guard); recheck; still ⇒ odirect ⇒ read_odirect_tier_serves; hold.credit(key, len); return             // :20424–20442
     else stale_binding_rebinds += 1
  // (8) zc DIRECT leg — cold by construction, device → the caller's pages through the sparse slot
  if zc_geometry ∧ key ∧ key ∉ inflight ∧ (fd, dev_base) = backend_router.zc_device_resolve(key):             // :20471–20480
     tracked = key_incarnation_tracked(key); before = fill_incarnation(key)
     fetch_t0 = record(ClassifyProbe)
     match zcs.fetch(fd, dev_base + slice_start, slice_len):                                                   // :20495–20497
        Ok(n == slice_len) ⇒ record(BlockFetch); incarnation_ok = !tracked ∨ still(key, before)
                             incarnation_ok ∧ block_binding_is ⇒ read_zc_serve_bytes += n; zcs.mark_served(n); return EMPTY   // :20523–20528
                             else stale_binding_rebinds += 1
        Ok(short) | Err ⇒ debug; fall through (the ladder's bounce OVERWRITES the pages)                         // :20539–20553
  // (9) RANGED dispatch — strictly after the probes missed (§3)
  lease_admits = dest_leaseable ∧ key ∉ inflight                                                               // :20579–20582
  if key ∧ ((device_true ∧ passthrough) ∨ (!device_true ∧ ranged_eligible(path, slice_len)) ∨ lease_admits):  // :20583–20588
     aligned = slice_start % 4096 == 0 ∧ slice_len % 4096 == 0 ∧ end ≤ bs
     rdest = (dest_addr ∧ aligned ∧ dest % 4096 == 0) ? dest.ranged(slice_len).map(lease_admits ? with_lease : id) : None   // :20610–20623
     record(ClassifyProbe)
     match get_block_range_for_index(path, b, rel, key, rdest, hint):
        Some(val) ⇒ aligned-dest ? val (already backs the dest) : dest ? {memcpy val; zero tail; read_copy_dest_bytes} : Bytes / bounce   // :20637–20701
        None      ⇒ hole: zeros into the dest (or a zero Bytes)                                                // :20703–20719
     return
  // (10) RAW FULL-BLOCK dest leg — small-block configs (slice == block ≤ max_read)
  probe_end = record(ClassifyProbe)
  if key ∧ dest ∧ slice_start == 0 ∧ slice_len == bs ∧ passthrough ∧ dest % 4096 == 0:     // §5.6 sibling-leg hygiene: transform volumes NEVER (ciphertext), :20752–20764
     tracked; before; read_block_with_dest(key, bs, dest); read_dest_dma_bytes += bs       // :20778–20790
     incarnation_ok ∧ block_binding_is ⇒ hold.credit(key, bs) (not device_true); resolved = dest_bytes(dest, bs)   // :20791–20829
     else stale_binding_rebinds += 1 (the validated loop overwrites the dest)
  // (11) the binding-validated WHOLE-BLOCK walk (§7)
  val = get_block_for_index(path, b, key, device_true, escalate_contended = true)          // :20855–20863 (stale-ABSENT key ⇒ None in, fresh resolve, :20994–21003)
  record(BlockFetch)
  (Some(val), Some(dest)) ⇒ hold.credit(key, len) BEFORE the slice (the coverage-credit truth);
                            zc_fd_source ⇒ fd_slice = val.slice (read_zc_pool_serve_bytes, cold)
                            else serve_copy_to_dest; read_copy_dest_bytes; read_copy_fill_slice_bytes; ZERO the remainder (reused-payload replay rule)   // :20869–20978
  (Some(val), None)       ⇒ val (None-dest arms credit here on the FULL block, :21112–21124)
  (None, _)               ⇒ hole ⇒ zeros
```

The non-single-block striped arm (the assemble arm, fan-out under `STRIPED_IO_SEM`) is out of this spec's hot path (`docs/design-read-path.md:313`).

### 1.5 Admission classes at the fill site (where `get_cached_or_fetch_block_traced` deposits; §6)

| Fill (> 256 KiB, incarnation stable) | Hot tier class | NVMe publish | Hold deposit |
|---|---|---|---|
| First touch (ghost miss), `Demand` | **Probation** (`referenced=0, protected=0`) | **skip** (`read_fill_publishes_skipped`) | demand (ledger-visible) |
| First touch, `DemandStream` / `Prefetch` | **ProbationReferenced** (one-lap grace) | skip | `DemandStream` deposits; `Prefetch` NEVER |
| Ghost hit, `Demand` / `Escalation` | **Protected** | publish (deferred; `Escalation` AWAITS) | demand |
| Ghost hit, streaming, governor grants | **ProtectedStream** (payback-exempt) | publish | deposit |
| Ghost hit, streaming, governor denies | ProbationReferenced | skip (`read_admission_stream_transients`) | deposit |
| ≤ 256 KiB | n/a — read LRU `Protected` | always (< 64 KiB inline; else deferred) | never |
| Ranged window (any) | never | never | never |

Sources: `routing.rs:12525–12558` (ghost decision), `:12578–12622` (publish), `:12656–12672` (hold), `:12675–12737` (RAM landing), `docs/design-read-path.md:202–209`.

---

## 2. The tiers

### 2.1 Hot-block tier — the sharded clock (`tiering/memory.rs`)

Structure (`:76–89`):
```
MemoryCacheShard {
  map:            scc::HashMap<Bytes, (Bytes, EntryState)>   // HashMap NOT HashIndex: removal MOVES the 4 MiB value out NOW (:70–75)
  eviction_queue: SegQueue<Bytes>                            // the clock ring
  eviction_lock:  parking_lot::Mutex<()>                     // try-lock ONLY (§13 Q3)
  current_bytes:  AtomicUsize
  entries:        AtomicUsize                                // live count: is_empty O(shards), the RES-10 gate (:81–87)
  max_bytes:      usize
}
EntryState {                                                  // :51–66
  referenced:     AtomicBool   // CONSUMABLE second chance — the clock scan clears it
  protected:      AtomicBool   // STICKY keep-worthiness — set at protected insert or by a promoting get; routes dehydration
  served_bytes:   AtomicU64    // payback credit (governor waste basis)
  stream_admitted: AtomicBool  // transient-window marker
}
EvictClass = Probation | Protected { served_bytes, stream_admitted }   // :19–33
```
Why two bits (`:35–45`): "spare this entry one more lap?" vs "was this entry ever worth keeping?" — every victim has `referenced == false` by construction, so it could never classify. All Relaxed single-word atomics, no cross-word invariant ⇒ racy-tolerant (a lost update = one lap early / one mis-classified victim; `:46–50`).

Operations:
* `get(key, promote, served)` (`:103–145`) — READER-class `read_sync` (the exclusive `OccupiedEntry` lock was 20.1 % of il lane cycles in `bucket::Writer::lock_sync_wait`, `:104–114`); `referenced = true`; `served > 0 ∧ !stream_admitted ⇒ served_bytes += served`; `promote ⇒ protected = true`; clone the `Bytes`.
  * Faces (`:448–485`): `get` (promote), `get_no_promote` (stream sub-read consumption is not keep-worthiness — promotion here manufactured protected victims out of one-pass streams, the PR 4 cage kill's third head, `:12271–12278`), `touch_no_promote` (residency probe, no clone — two refcount RMWs saved per pipelined block), `get_serving(key, bytes)`.
* `put(key, value, protected, referenced, stream, validate)` (`:148–236`):
  1. `val_len > max_bytes` ⇒ remove any smaller stale entry under the key; return (`:161–164`).
  2. Occupied ⇒ `validate()` under the bucket WRITER lock; refusal REMOVES the entry (absence is correctness-safe) (`:169–184`); replace the value; `referenced = true`; `protected ⇒ protected = true, stream_admitted = stream` (a probation re-put never demotes — sticky) (`:185–198`).
  3. Vacant ⇒ `validate()`; push the key on the ring; `current_bytes += len`; `entries += 1`; insert with the class bits (`:200–225`).
  4. `current_bytes > max_bytes ⇒ try_evict`.
* `try_evict` (`:271–336`) — under the try-locked latch, `max_loops = max(2 × ring_len, 512)`: pop a key; `referenced ⇒ clear + re-push`; else remove, classify by the STICKY bit, return `(key, value, class)`.
* `shed_to(target)` (`:242–269`) — the R5 Red clamp: second chances IGNORED ("the clamp is an order, not a scan"), `max_loops = max(2 × ring_len, 64)`.
* `remove(key)` (`:338–370`) — absent key = ONE reader-lock `contains_sync` probe, no alloc (every W1 patch purges tiers the write path never populated, 400 k+/s); present ⇒ writer-lock remove; then `reclaim_eviction_queue` — a drain-and-refill pass when `nodes > 2 × live + EVICTION_QUEUE_RECLAIM_SLACK (64)`, skipped while the evictor holds the latch (`:386–408, 414`).

Shard count and budget:
* `shard_count_from(cores) = max(next_pow2(cores), 16)` on the fleet-share-divided root (`cache/lru.rs:10–16`).
* `with_capacity_min_shard(budget, 16 MiB)` HALVES the count while `budget / shards < 16 MiB` (never below 1): core-count sharding gave the default 128 MiB budget ONE 4 MiB block per shard, so hash-colliding keys could never coexist — 6.1 k whole-block refetches / 20 s on a FITTING 64 MiB set (`cache/mod.rs:166–175`, `lru.rs:92–110`). Production floors the total at `shards × 4 MiB` (`lru.rs:126–129`).
* Budget: `SQUEEZEFS_READ_HOT_BLOCK_CACHE_MB × MiB`, default `max(2 × 4 MiB, read_mem_limit / 4)`; `read_mem_limit` = `--read-mem-cache-size` else RAM / 10 (`cache/mod.rs:107–113, 132–142`). The 2-block floor = one stream's consume-behind window. **0 short-circuits the tier AND degrades admission `second-touch → always`** (`routing.rs:11467–11476`).

The `LruCache` wrapper (`cache/lru.rs:39–78`):
* `PutClass → (protected, referenced, stream)`: `Protected (1,1,0)`, `ProtectedStream (1,1,1)`, `Probation (0,0,0)`, `ProbationReferenced (0,1,0)` (`:321–326`).
* `put_class_validated` (`:304–391`): Yellow+ ⇒ `freeze_at = current_bytes` before the insert and `shed_to(freeze_at)` after (net-zero growth, `:315–331`); every victim reported to the governor at the SOURCE before any routing (`:341–343`); `drop_probation_evictions` (hot tier) ⇒ probation victims die here, `hot_block_evictions += 1`, `hot_block_probation_drops += 1` (16384 slots × 4 MiB of live Bytes was an 8 GiB cgroup kill, `:344–359`); protected ⇒ `try_send` on the channel iff a worker took the receiver ∧ `channel_bytes + len ≤ bound`, else `evict_channel_drops` (`:360–384`).
* `EVICT_CHANNEL_BYTE_BOUND = 256 MiB`; bound = `min(256 MiB, shed_target)` at Yellow+ (`:153, 170–180`).
* `take_evict_rx` ARMS the send side; worker-less caches drop victims at the source (`:216–225`).

Dehydration worker for the hot tier (`cache/mod.rs:278–350`, task `hot_block_dehydrate`):
```
for (key, data, class) in rx:            // bounded 2 s recv, exits when the last TieredCache drops (:52–68)
  channel_gauge.evict_channel_sub(len); hot_block_evictions += 1
  Probation ⇒ hot_block_probation_drops += 1                                   // the R1b gate flip
  Protected ⇒ level ≥ Yellow ⇒ mem_budget_dehydrate_paused += 1                // protected INCLUDED
             nvme.has_cached_read_block(key) ⇒ hot_block_dehydrate_skips += 1   // every ghost-admitted fill already published — ~100 % duplicate multi-MiB writes on the rand-4k row
             else run_blocking(cache_read_block_validated_self(key, data))
```

Laws: hot puts sit ONLY inside the publishable window (`routing.rs:12675–12737`); NVMe-tier hits are NEVER copied into RAM (the no-RAM-repromote rule — the incarnation word cannot prove ENTRY provenance of a tier entry from a dying incarnation whose purge is still in flight; all-zero block reads until remount, `:12303–12315`); the write path's `upload_full_block` never puts (write streams are the other scan, `docs/design-read-path.md:254`).

### 2.2 The ≤ 256 KiB read LRU

* The SAME `LruCache`; every insert `Protected` (`cache/lru.rs:260–262`); budget `read_mem_limit`.
* Its worker keeps the historical `blocks/` key filter and dehydrates every class, `< 64 KiB` inline else on the blocking pool, through `cache_read_block_validated_self` (`cache/mod.rs:233–276`).
* Gate `READ_SIZE_CLASS_BOUNDARY_BYTES = 256 KiB` (`routing.rs:677–681`): one 10 GiB stream must not flush every warm small entry (`docs/design-read-path.md:221`). EOF-short tails ≤ 256 KiB and 256 KiB-block volumes take the small-fill path BY DESIGN (`:187`).

### 2.3 The read-lane hold (`read_lane.rs:243–605`)

```
ReadLaneHold {
  entries: scc::HashMap<String, HoldEntry>     // HoldEntry { bytes, served: AtomicU64, seq: u64, ledger_visible: bool }  (:243–264)
  fifo:    SegQueue<(seq, key)>                // oldest-first trim order
  seq, bytes, live: AtomicU64
  reclaiming: AtomicBool                       // single-reclaimer latch
}
```
* `insert_class(key, bytes, budget, ledger_visible, validate)` (`:377–422`): `len == 0 ∨ budget == 0 ⇒ return`; Occupied ⇒ KEEP (same incarnation ⇒ identical bytes; the existing provenance wins); Vacant ⇒ `validate()` under the bucket writer lock, insert, `bytes += len`, `live += 1`, push FIFO, `read_lane_holds += 1`, `trim_to(budget)`, `reclaim_fifo()`.
  * `insert_validated` = lane fetch (`ledger_visible = false`); `insert_demand_validated` = demand primary (`true`).
* `serve_with_provenance(key, credit)` (`:436–452`): reader-class; `credit > 0` ∧ `served` crosses `len` ⇒ exactly ONE server observes the crossing ⇒ `retire(key)` (`read_lane_hold_retired`).
* `credit(key, n)` (`:458–476`): consumption through another path (primary slice, hot/NVMe serve, raw dest leg) — hold memory CONVERGES BY CONSUMPTION. Dest arms credit INLINE where the full block length is known: the tail credit read post-slice bytes and credited 0 for every sub-read past a block's first — 570 retired of 121,639 holds, the hold pinned at its budget with 119 k FIFO evictions reading as churn (`routing.rs:20806–20819, 20880–20896`).
* `trim_to(target)` (`:502–524`): pop FIFO; a stale seq tombstone skips; an unconsumed victim ⇒ `read_lane_hold_evicted_unconsumed += 1`; a lane-fetch victim additionally ⇒ `read_lane_hold_ahead_evictions += 1`. Oldest-first IS the ahead-priority mechanism (ahead entries are the newest).
* `purge(key)` (`:480`) — the unified-purge arm; `remove_entry` probes `contains_sync` first (absent-key economy), the scc remover subtracts exactly once (`:538–558`).
* `reclaim_fifo` (`:577–605`): when `backlog > 2 × live + FIFO_RECLAIM_SLACK (32)`, drain-and-refill under the latch; a head-only skim cannot work (a long-lived unconsumed entry sits at the head while tombstones pile up behind it).
* Budget `hold_budget_bytes(mem, bs, streams, depth) = clamp(4 × streams × depth × bs, 4 × bs, mem / READ_LANE_BUDGET_DIVISOR (8))` (`:206–226`): 4× the aggregate pipeline (2× left half the deposits evicted unconsumed under 38-stream FIFO skew); `/8` is junior to the write pipeline's `/4` (`:105–109`). The issue path caches it fast-up / ⅛-per-2 s-epoch down (`:786–814`; a flapping budget trimmed a healthy 9.7 GiB hold to 1.2 GiB). Deposit sites take `max(cached, fallback over the live stream gauge + the derived window cap)` (`routing.rs:13505–13528`).
* Deposits: demand primaries > 256 KiB with `class != Prefetch`, not Red (`routing.rs:12656–12672`); lane fetches, not Red (`:14118–14128`).
* NOT a cache tier: no admission, no ghost, no governor ledger, no dehydration (`cache/mod.rs:34–41`). Measured: qd8 +12.8 %, qd32 +14.5 %, `read_amp` 1.41 → 1.11 (`.benchmarks/2026-08-01-read-lane.md:111–114`).

### 2.4 The NVMe read cache — the mmap segment ring (`tiering/nvme.rs`, `cache/nvme.rs`)

On-media record frame (`tiering/nvme.rs:11–12, 544–548, 633–660`):
```
[ magic u32 LE = 0xCAFEBABE ][ key_len u32 LE ][ val_len u32 LE ][ key bytes ][ pad to 4 KiB ][ value bytes ]
 HEADER_SIZE = 12; target_offset and val_offset both 4 KiB-aligned (alignment = 4096 iff capacity ≥ 4096)
```
* A remove/replace ZEROES the record's magic + `msync(MS_ASYNC)` (`:17–18, 662–669`); a dead extent is `fallocate(PUNCH_HOLE|KEEP_SIZE)` (file) or header-zeroed + `madvise(MADV_DONTNEED)` (anon) (`reclaim_extent`, `:341–392`).
* Files: `<staging_dir>/cache_segment/*`, mode `STAGING_FILE_MODE`, never shrunk (a smaller `set_len` under a live mapping SIGBUSes the daemon — FIND-VS-A, `:418–432`).
* **Cold on purpose**: read-cache segments are WIPED at mount — offsets are not incarnation-stable across mounts and no cross-session incarnation store exists; a resurrected entry under a reused key is the crash twin of the stale-fill family (`cache/nvme.rs:1150–1184`).

Shard (`:177–193`): `RwLock<NvmeShardInner { mmap, map: HashMap<Bytes, BlockMeta{offset, len}>, write_offset, capacity, pending: Vec<(start, end)> }>`.
* **Shard-lock invariant** (Hang-1, `:310–334`): parking_lot's RwLock is writer-preferring, so EVERY shared acquisition is `read_recursive()` (never park behind a QUEUED writer — a guard held across an await would close a dependency cycle and wedge the executor); writers (`put`/`reserve_and_write`/`remove`) run ONLY on the blocking pool.
* `put_impl` (`:531–716`), two phases:
  1. Under the write lock: compute the aligned placement; `block_size > capacity ⇒ refuse`; wrap ⇒ `evict_overlapping(target, capacity)` GEOMETRY-COMPLETE over the map (the queue-order front-run walk clobbered still-indexed entries — generic/074 fstest.3, `:208–254`); bump past in-flight `pending` extents (bounded `pending.len() + 2`; a pathological layout skips the put — a cache put is best-effort); evict overlapping; un-index the old same-key entry; push `pending`; advance the cursor.
  2. memcpy header + key + value and `msync(MS_ASYNC)` OUTSIDE the lock (`:633–676`).
  3. Under the write lock: `validate()` at the INDEX INSERT (the visibility point; a refusal zeroes the magic while `pending` still guards the extent — no reader can observe the bytes), remove `pending`, insert (`:678–713`).
* Read-cache evictions are INDEX-ONLY (`put_discard_evicted`): the materializing flavor's page-in + memcpy of every victim was 44 % of daemon CPU + ~6× spurious device reads (`:503–508`; `cache/nvme.rs:2291–2297`).
* `get` verifies the magic under the read guard (`:463–`); `NvmeCacheReadGuard` (`:293–308`) is an owned `'static` guard over `[offset, offset+len)` — hold discipline: ONE reply or ONE DMA, NEVER into a cache (`:272–292`). The router copies it out BEFORE any await (`routing.rs:20357–20361`).

Shards and budget (`cache/nvme.rs:1057–1101, 1189–1206`):
* `default_shards = max(next_pow2(cores), 16)`; `shard_plan(per_device_capacity, default, max_entry = block_size)` halves while `cap / shards < 2 × block + SHARD_REPLACE_SLACK (64 KiB)` — a shard must fit a same-key crash-safe REPLACE of a block-size entry; `< 10 MiB/device ⇒ 1 shard`.
* Read budget `--read-disk-cache-size` else `aggregate_capacity / 4`, split evenly per cache device (`cache/mod.rs:180–193`).

Validated publish façade (`cache/nvme.rs`):
* `cache_read_block_if(key, data, validate)` (`:2310–2328`): cache-less volume (`staging_dirs.is_empty()`) ⇒ false; `mem_budget::tier_publish_paused()` ⇒ `read_tier_publishes_paused += 1`, false; else `put_discard_evicted_validated`.
* `cache_read_block_validated(key, data, router)` (`:2344–2360`): untracked key ⇒ plain publish; `fill_incarnation == None` ⇒ refuse; else `validate = still(key, before)`. `_self` uses the staging's wired router (unwired ⇒ refuse).
* `has_cached_read_block` = index membership (`:2388`); `remove_cached_read_block` (`:2397`); `get_cached_read_block_range_zero_copy` clamps `[offset, offset+size)` to the value (`:2424–2441`).
* Mem component `read_tier_mmap` floor 0, weight 0, `kernel_reclaimable` — attribution only (logical bytes over reclaimable page cache pinned phantom Red, `fuse_client.rs:26800–26810`).

### 2.5 Budgets as sheddable components (charter §6.2 R5; `fuse_client.rs:26710–26914`)

| Component | Floor | Weight | Gauge | Shed |
|---|---|---|---|---|
| `hot_block_tier` | 64 MiB | 4 | `hot_block.current_bytes()` | `shed_to(target)` — second chances ignored |
| `read_lane_hold` | 0 | 2 | `hold.bytes()` | `trim_to(target)` oldest-first |
| `read_lane_inflight` | 0 | 1 | lane in-flight DMA bytes | no-op: Red clamps depth to 0; converges by completion |
| `read_lru` / `write_lru` | 32 MiB | 2 | `current_bytes()` | `shed_to` |
| `read_lru_evict_channel` / `hot_block_evict_channel` | 0 | 1 | channel bytes | send-side clamp (`mem_budget.rs:927–940`) |
| `prefetch_inflight` | 0 | 1 | `prefetch_inflight_bytes` | `stream_lanes.invalidate_all()` — plans cleared |
| `read_tier_mmap` | 0 | 0 | tier logical bytes | none; `kernel_reclaimable` |
| `buffer_pool` / `aligned_buf_pool` / `zc_fill_pool` | 64 MiB | 2 | allocated bytes | `trim_to` (slab slots never freed; over-capacity backings only) |
| `ranged_buf_pool` | 4 MiB | 2 | allocated | `trim_to` |

Level responses on the read path (`docs/design-read-path.md:402–406`):
* **Yellow** — window growth frozen (`prefetch_window_grows` requires Green, `routing.rs:6598–6605`); hot inserts evict-first (`lru.rs:315–331`); dehydration paused entirely, protected included.
* **Red** — R2 issue stops (`pipeline_touch` returns, `:13076–13079`); ahead depth 0 (`read_lane_depth_blocks`, `read_lane.rs:171–173`); hold deposits paused; ranged escalation paused (`ranged_escalation_candidate`, `routing.rs:14880–14885`).
* **Tier-publish pause** keys on the UNRECLAIMABLE arm, not the level (`cache/nvme.rs:2274–2288`).

### 2.6 THE ONE purge helper (`cache/mod.rs:374–391`)

```
purge_block_key(key):
    read_lru.remove(key)                 // RAM LRU
    hot_block.remove(key)                // hot tier
    read_lane_hold.purge(key)            // the hold — the fifth arm (2026-08-01)
    nvme.remove_cached_read_block(key)   // NVMe read cache
    gds.remove_cached(key)               // unlink `.gds_cache`, ENOENT ignored (§13 Q2)
```
Contract: the ONLY legal way to drop a block key from the caches — displaced-key frees, fill undos (`routing.rs:12757–12762`, `:14137`, `:13651`), incarnation purges, the `read_tier_purge` callback (`:5130–5134`) and the clone/unlink/write-path invalidations all route here; a forgotten tier is the 074 stale-serve class (design R-6, `docs/design-read-path.md:532`). Pinned by a grep-guard (`tests/hot_block_tier_tests.rs:661`). Invalidations deliberately do NOT report to the governor (`lru.rs:76–77`).

---

## 3. Ranged DMA + the dest lease (R3 + copy-elimination phase 1)

### 3.1 Eligibility (`ranged_eligible`, `routing.rs:14858–14867`)
```
ranged_eligible(file, len) =
     ranged_threshold != 0            // SQUEEZEFS_READ_RANGED_THRESHOLD, default 262144; 0 = kill switch
  ∧ len ≤ ranged_threshold
  ∧ block_size > ranged_threshold     // small-block volumes keep whole-block fetches (already request-sized, LRU-cacheable)
  ∧ crypto.is_passthrough()           // decode needs the whole physical block otherwise (static per mount)
  ∧ ¬stream_lanes[file].any_streaming_fresh()   // streams want whole blocks: 1.0× amp + the pipeline
```
Called strictly AFTER the overlay/hot/hold/tier probes missed (`:14856–14857`).

### 3.2 The window (`get_block_range_for_index`, `:14929–15154`)
* `LBA_GRAIN = 4096` (`:26`) — the conservative grain (approved OQ #1: no per-device `logical_block_size` probe until `ranged_read_unaligned_bounces` shows real 512-native waste, `docs/design-read-path.md:541`).
* `aligned_start = start & ~4095`; `aligned_end = min(ceil4k(end), block_size)`; `window = aligned_end − aligned_start`; `bounced = window != req_len` (`:15018–15021`).
* The bounce leg copies ≤ request + 8 KiB; window padding is NEVER served (`docs/design-read-path.md:371`).

### 3.3 The device leg
* `BackendRouter::read_block_range(key, rel_start, len, dest_addr)` (`routing.rs:4006–4057`): debug-asserts LBA alignment of start/len/dest; REFUSES a dead-lifetime key BEFORE the DMA (`incarnation_ok`, `:4028–4033` — the §6.3 silent cross-file serve, made structural); refuses an unhealthy backend; `device.read_block_with_dest(offset + rel_start, len, dest)`.
* `NvmeBlockDev::read_block_with_dest_inner` (`nvme_dev.rs:2695–2810`):
  1. `size == 0 ⇒ Ok(empty)` at the door — a 0-length `ReadFixed` against the registered slab OOPSed on sqz 7.1.8 and SIGKILLed the WORKER THREAD alone, stranding the lane (`:2702–2716`).
  2. `dest == None ∧ size > pool buf ⇒ refuse` (`:2717–2723`).
  3. zcrx lane first if armed (rung 8) (`:2730–2738`).
  4. dest ⇒ `claim_dest_token(addr, size)` — MEM-1: the token rides in the SQE and drops at the CQE so the transport's commit gate parks the slot re-arm while a late DMA can still land (cross-request corruption otherwise, `:154–168, 2740–2757`); a `Bytes::from_static` view.
  5. no dest ⇒ `read_bounce_pool(size)`: ≤ 64 KiB the `RANGED_BUF_POOL` slab (registered as ONE fixed buffer ⇒ `ReadFixed`, no per-op GUP), else the whole-block pool (`cache/pool.rs:832–838`).
  6. reads round-robin the lane pool (`read_lane_pick`, `:2769–2774`); (the per-read timeout at `:2798–2801` is a quirk — §13 Q6).

### 3.4 The dest lease
* `RangedDest{ptr, cap, lease}` (`routing.rs:6666–6713`): offered ONLY on the zero-copy leg (window == request ∧ dest 4 KiB-aligned); the callee DMAs the served length at offset 0 and zeroes `served..requested` (the reused-payload replay rule); `with_lease()` is ATTRIBUTION only (`:6669–6675`).
* Admission composes `hint.dest_lease` (kernel ent payload, not an arena, lever on; `fuse_client.rs:34156`) with the geometry (`routing.rs:20044–20050`) and `key ∉ inflight` — joining a cohort is strictly cheaper than a second DMA (`:20575–20582`).
* The validated serve counts `read_dest_lease_bytes += req_len` ONCE (a rebind retry re-DMAs and `read_dest_dma_bytes` counts every pass; the engagement instrument accounts SERVED bytes so `dest → lease` closes byte-for-byte; `:15103–15114`).
* **Never published, never single-flighted** (`:14897–14900, 14925–14928`): a partial payload must not exist under a whole-block tier key; deduping 4 KiB fetches under a 4 MiB key would serialize independent sub-reads for no byte savings (design alternative D, `docs/design-read-path.md:460–462`).
* **Lease windows are ghost-exempt** (`:14945–14958`): the ghost keys on the BLOCK, so the second sub-block window of one sequential pass would read as a "re-read" and self-escalate every block's tail into whole-block fills (2× device bytes AND the very copy the lease deletes). Lease traffic is device-true BY DESIGN (the CPU-wall trade).
* **Yield**: a leaseable request stamps `lanes.dest_lease_ms`; BOTH speculative issue arms decline while the stamp is < 2 s old — a pooled ahead fill for a window the demand read DMAs itself is a pure double-fetch (`:13049–13070`, `:13716–13726`, `:6645–6657`).
* **Measured** (`.benchmarks/2026-08-06-read-dest-lease.md:126–127`): side medians 10.05 vs 7.81 GB/s = **+28.6 %** order-independent; daemon CPU/byte 0.158 vs 0.316 s/GB = **−50 %**; field 27.98/27.74 vs 21.47/21.98 GB/s = +28.3 % (`:227–230`). The serve dest-copy was 1.00 CPU passes/byte (`:6`).

### 3.5 The ranged loop (`:15030–15154`)
```
key = resolved_key
for attempt in 0..MAX_REBINDS (8):                                   // :14938, :15031
  key == None ⇒ Ok(None) (hole)
  attempt ≥ 4 ⇒ sleep 1 << min(attempt−4, 3) ms                      // the FIND-RW5-A backoff tail, :15039–15043
  tracked; before = fill_incarnation(key)
  ranged_reads += 1; ranged_read_bytes += window; governor.note_foreground(window)   // the clamp's denominator, :15047–15055
  bounced ⇒ ranged_read_unaligned_bounces += 1
  bytes = read_block_range(key, aligned_start, window, dest?.ptr)   // dest ⇒ read_dest_dma_bytes += window; else read_fill_dma_bytes += window
  incarnation_ok = !tracked ∨ still(key, before)                     // bytes in hand FIRST, :15089–15091
  current = current_block_binding(path, b)
  incarnation_ok ∧ current == key ⇒ dest ? {lease ⇒ read_dest_lease_bytes += req_len; dest_bytes(ptr, req_len)} : bytes.slice(from..from+req_len); return
  stale_binding_rebinds += 1; ranged_read_rebinds += 1; key = current  // :15132–15140
// exhaustion ⇒ the whole-block validated walk, then slice_whole_for_ranged (dest copy + tail zero | zero-copy Bytes slice | counted bounce)   // :15143–15210
```
Device-true: EVERY request size on a passthrough volume takes the window path; the primitive skips the ghost (`:20567–20572`, `:14914–14916`).

Port mapping: `src/io/nvme_dev.zig` gains `readRange(offset, len, dest: ?[]align(4096) u8)` beside `readBlock` — `dma_ptr = dest` when given (no bounce; the slot table owns the dest token: a parked re-arm until the CQE), else the existing `acquireReadBuf` path (`:251–258`); `Buf.Kind.none` already names "an aligned caller buffer" (`:69`).

---

## 4. Result-carrying single-flight whole-block fetch (R1a)

`get_cached_or_fetch_block_traced(key, class) -> (ReadBlockValue, serve_valid)` (`routing.rs:12235–12871`):
```
is_damaged_mapping(key) ⇒ EIO before any probe                             // §5.6a quarantine, :12243–12247 (§13 Q15)
WAIT_SLICE = 50 ms; MAX_WAIT = 60 s; deadline = now + MAX_WAIT              // :12252–12260 (§13 Q6)
loop:
  hot_block.get_no_promote(key)    ⇒ cache_hits, hot_block_hits; return (Bytes, true)      // :12270–12287
  read_lru.get(key)                ⇒ cache_hits; return (Bytes, true)                      // :12289–12298
  nvme.read_cached_block(key)      ⇒ cache_hits; NO RAM re-promote; return (Bytes, true)   // :12300–12319
  hold.serve_with_provenance(key, 0) ⇒ cache_hits, read_lane_serves; ledger_visible ⇒ hold_serve_admission; return   // :12330–12352
  now ≥ deadline ⇒ Err(TimedOut)
  if entry = inflight_block_reads.get(key):                                  // :12360
     rx = tx.subscribe(); sf_waited = true
     re-check hot/LRU (subscribe-after-send race) ⇒ return                    // :12366–12379
     registry empty ⇒ continue (primary finished)
     match timeout(50 ms, rx.wait()):
        Some(res) ⇒ serve_valid = res.serve_valid ∧ res.verdict_word.is_none_or(|w| still(key, w))   // STORED-SNAPSHOT REVALIDATION, generic/209, :12392–12416
                    singleflight_waiter_result_serves += 1; record(SfWait); return (res.bytes, serve_valid)
        None | lagged | closed | timeout ⇒ continue                           // one waiter becomes the new primary, :12426–12431
  // become the PRIMARY
  (tx, _) = sqz_flight::channel()                                             // a shared ONESHOT storing exactly one terminal value, :12435–12439
  insert_sync(key, tx) lost ⇒ continue
  cache_misses += 1; fill_t0 = now
  guard = InflightBlockReadGuard { key, registry, tx, completed: false }      // :12461–12466; drop semantics :7604–7633
  before = fill_incarnation(key)                                              // :12477
  bytes  = fetch_block_from_remote(key)?                                      // read_nvme_block (phase fetch_dma; read_fill_dma_bytes += raw) → process_read_async (phase decode), :12163–12182
  serve_valid = !key_incarnation_tracked(key)                                 // untracked keys are vacuously valid, never published, :12489
  publishable = before.filter(|b| still(key, b))                              // :12490–12493
  if before = publishable:
     adm_t0 = now
     class == DemandStream ⇒ governor.note_foreground(len)                    // the stream's own device spend funds the trickle, :12520–12524
     ghost_admit = len ≤ 256 KiB ? true : match tier_admission { Always ⇒ true; Never ⇒ false;
                   SecondTouch ⇒ hit = ghost.check_and_record(key); hit ⇒ read_tier_admission_ghost_hits += 1;
                                 hit ∧ (!class.streaming() ∨ governor.allow_stream_admission(len)) }     // :12525–12558
     !ghost_admit ⇒ read_fill_publishes_skipped += 1; else len ≥ 64 KiB ⇒ read_tier_admissions += 1
     deposit_still = || still(key, before)
     ghost_admit ∧ len < 64 KiB ⇒ nvme.cache_read_block_if(key, bytes, deposit_still)            // inline small publish
     ghost_admit ∧ len ≥ 64 KiB ⇒ deferred_publish = Some((nvme, router, key, bytes, before))     // PERF-11, :12586–12622
     TEST_FILL_PRE_DEPOSIT_STALL_MS seam; record(Admission)
     dep_t0 = now
     read_lane.enabled ∧ class != Prefetch ∧ len > 256 KiB ∧ level != Red ⇒ hold.insert_demand_validated(key, bytes, hold_budget(), deposit_still)   // :12656–12672
     len ≤ 256 KiB ⇒ read_lru.put_class_validated(key, bytes, Protected, deposit_still)
     else hot_block_misses += 1; hot_class = (ghost_admit ∧ SecondTouch) ? (streaming ? ProtectedStream : Protected)
                                           : (streaming ? ProbationReferenced : Probation);
          hot_block.put_class_validated(key, bytes, hot_class, deposit_still)                      // :12675–12737
     serve_valid = still(key, before)                                          // the seqlock COMPLETION — publish-then-revalidate, :12753–12755
     !serve_valid ⇒ purge_block_key(key); deferred_publish = None               // undo across every tier, :12757–12763
     record(Deposit)
  tx.send(Some(FillResult { bytes, serve_valid, verdict_word: publishable.filter(serve_valid) }))   // :12777–12780
  guard.completed = true; record(FillTotal)
  read_lane.enabled ⇒ read_lane.probe_on_fill_bytes(len)                       // the engage governor's delivery signal, :12790–12793
  if deferred_publish:                                                          // :12811–12858
     publish = run_blocking(move || { let _guard = guard;                       // the GUARD RIDES THE PUBLISH — the registry entry outlives the publish
                                      TEST_TIER_PUBLISH_DELAY_MS seam
                                      !still(key, before) ⇒ return
                                      TEST_TIER_PUBLISH_MID_WINDOW_STALL_MS seam
                                      nvme.cache_read_block_if(key, bytes, || still(key, before))
                                      TEST_TIER_PUBLISH_POST_PUT_STALL_MS seam })
     class == Escalation ⇒ publish.await                                        // the admission IS the serve contract (hybrid_io_tests)
  return (Bytes(bytes), serve_valid)
```
Guard drop semantics (`:7604–7633`, design §5.2 `docs/design-read-path.md:175`): success ⇒ CLOSE-ONLY (no second value; a late subscriber reads the stored flight value or re-checks the caches); failure (fetch error) or future-drop mid-fetch ⇒ send `None` before closing so live waiters fail fast into the re-check loop.

Why the guard rides the publish (PERF-11, `:12586–12605`): > 256 KiB blocks never enter the RAM LRU, so a late publish let the next sub-block read miss every tier, find no in-flight entry, become a fresh primary and refetch the whole block — the elbencho row-2 6.3× `get_obj` multiplier (`.benchmarks/…attribution.md:63`). The ordering is a property of the GUARD, not of the caller's return.

Contract (`docs/design-read-path.md:177`): a completed fill is servable to every cohort member without a second device fetch, unconditionally, and to immediately-following readers while it remains hot-resident. Non-serving callers (prefetch) use `get_cached_or_fetch_block` (`:12189–12197`), the unvalidated entry point — legal because a warmer serves no one.

---

## 5. Prefetch: AIMD per lane, K = 4 offset lanes, streaming classification, the ahead lane

### 5.1 Lanes and classification (`StreamLanes`, `routing.rs:6617–6658, 7396–7603`)

```
StreamLanes { lanes: [StreamLane; 4], issue_owner_idx, issue_owner_ms, foreign_since_match, dest_lease_ms }   // :6617–6658
StreamLane  { next_expected_offset, run_reads, last_seen_ms, classified,
              next_prefetch_block, window (start 2), inflight, rl_inflight, unconsumed, consumed_edge, issued_base,
              generation, last_counted_epoch, suppress_until_edge, detect_streak, pending_blocks: scc::HashSet<u64> }   // :6393–6474
lane_pending_key(generation, block) = generation << 32 | block                  // a tag, never an identity, :6480–6482
```
One `StreamLanes` per file path in a moka entry (`stream_lanes.get_or_insert_with`, claim-only mint; `:12944–12952`). K = 4 so concurrent sequential readers of one file do not reset each other; > K degrade to random — a later pipeline start, never wrongness (`:6613–6616`).

`observe(offset, len, claim) -> Option<LaneRef{lane, streaming}>` (`:7479–7602`):
1. Exact-contiguity match on any lane's `next_expected_offset` ⇒ `foreign_since_match = 0`; advance; `run += 1`; `run ≥ 4 ⇒ classified = true` (`read_streams_classified += 1` once) ⇒ `streaming = true`; else `streaming = false` (`:7483–7504`).
2. CLASSIFIED-MEMBERSHIP tolerance: a fresh (< 2 s) classified lane accepts `|offset − expected| ≤ 64 × len`, advancing max-forward (a libaio qd8 completion swap otherwise wedged the lane forever: 0.7 % arrival match, 1,233 declassify/reclassify events per 70 s row, ~3 % engagement; 4 KiB ⇒ ±256 KiB keeps random traffic foreign — 0 % member on the random harm shapes, 88–100 % on seq reorder; `:7505–7548`).
3. `!claim ⇒ None` — warm ring serves may continue a lane but never mint one (the per-op claim path measured −6..−16 % on the 1M-IOPS rand-4k rows; `:7468–7475, 7549–7551`).
4. Foreign run: `foreign_since_match ≥ FOREIGN_DECLASSIFY_RUN (16)` clears every lane's classification + run credit (a spurious stream would veto the ranged path file-wide for 2 s: 20–40 GiB/20 s of ungoverned whole-block fetches; `:6632–6644, 7552–7565`).
5. Claim the stalest lane iff idle ≥ 2 s: `generation += 1`, run 1, `classified = false`, window 2, plan/edges/streak reset; `inflight`/`rl_inflight` NOT reset (tasks decrement at settle; the generation bump lands their completions as wasted) (`:7566–7600`).

`any_streaming_fresh()` = any lane `classified ∧ now − last_seen < 2000 ms` (`:7453–7459`) — the ranged veto and the `DemandStream` class. The 2 s staleness constant is used everywhere (lane claim, freshness, epoch length, issue-owner rotation, dest-lease stamp).

### 5.2 Window economy — pure functions (table-pinned by `tests/read_prefetch_window_tests.rs`)
```
R2_WINDOW_START = 2                                                                      // :7394
derived_prefetch_window_cap(share_pct, hot_budget, bs) = bs == 0 ? 4 : clamp(share% × hot_budget / bs, 4, 4096)   // :6550–6558
prefetch_issue_admits(window, cap, resident_share, in_flight, unconsumed) =
     resident_share > 0 ∧ unconsumed < resident_share ∧ in_flight + unconsumed < min(window, cap)   // :6575–6587
prefetch_window_grows(will_wait, overran, streak, green) = green ∧ (will_wait ∨ (overran ∧ streak == 0))   // :6598–6605
resident_share = share_pct × hot_budget / 100 / bs / active_streams                       // NO floor, :13119–13122
```
* Cap floor 4: the AIMD start of 2 must double once; cap 4096 bounds a pathological budget/block ratio. Explicit `SQUEEZEFS_READ_PREFETCH_WINDOW` wins verbatim ≤ 4096; `0` = kill switch (`:11485–11487, 12902–12904`).
* The 2026-07-29 split: in-flight DMA bytes are R5-charged transients (`prefetch_inflight_bytes`), not hot-tier residents; charging them against the resident share made the 16-stream default a structurally depth-1 pipeline (prefetch covered 6 % of the seq-row fetches) (`:6566–6574`).
* No floor on `resident_share`: a `.max(1)` kept 4 contention lanes thrashing a 2-block budget — 104 fetches for 48 unique (`:13106–13111`).
* `share_pct` = `SQUEEZEFS_READ_PREFETCH_SHARE_PCT`, default 50, clamp 1..100 (`:11488–11490`).
* `active_streams` = `StreamActivityGauge` (`:6484–6539`): two 2 s epochs, each lane counted once per epoch via `last_counted_epoch`, `active = max(cur, prev).max(1)`, NO decrement path (moka evicts entries silently — an inc/dec counter leaks upward); exported `prefetch_active_streams`.

### 5.3 `pipeline_touch` (`:12922–13249`) — once per striped request, before the serve probes
```
pipeline_touch(path, meta, bs, offset, len, start_block, end_block, will_wait_inflight, first_key, lane_claim, dest_leaseable):
  prefetch_disabled() ∨ meta.file_type != "striped" ⇒ return
  lanes = stream_lanes.get(path) ?? (lane_claim ? insert_with(StreamLanes::new) : return)
  (lane, streaming) = lanes.observe(offset, len, lane_claim)? else return
  generation = lane.generation
  // consume bookkeeping — per BLOCK advanced (a per-request drain was 4× too fast, :12967–12972)
  issued_edge = lane.next_prefetch_block; issued_base = lane.issued_base; ce = lane.consumed_edge
  if end_block ≥ ce:
     lane.consumed_edge = end_block + 1
     drain `unconsumed` once per block in [max(ce, issued_base), min(end_block, issued_edge − 1)]       // :12977–12988
     if issued_base ≤ start_block < issued_edge ∧ first_key = k:                                        // consume-time evicted-unconsumed detector
        resident = hot.touch_no_promote(k) ∨ lru.touch_no_promote(k) ∨ nvme.has_cached_read_block(k)
                 ∨ (lane armed ∧ hold.contains(k)) ∨ k ∈ inflight_block_reads                            // :12998–13007
        !resident ∧ lane.pending_blocks.contains(lane_pending_key(generation, start_block)) ⇒ NO VERDICT   // the parked-task screen, :13008–13022
        !resident ⇒ prefetch_evicted_unconsumed += 1; window = max(window / 2, 2);                         // AIMD collapse
                    streak = min(detect_streak++, 5); suppress_until_edge = end_block + 1 + (2 << streak)   // progress-clocked quiescence, cap 64 blocks, :13023–13039
        resident  ⇒ detect_streak = 0
  dest_leaseable ⇒ lanes.dest_lease_ms = now                                 // stamp BEFORE the streaming check, :13054–13056
  !streaming ⇒ return
  dest_leaseable ∨ lanes.dest_lease_fresh(now) ⇒ return                      // the issue arms decline, :13068–13070
  level == Red ⇒ return                                                       // :13076–13079
  overran = issued_edge > issued_base ∧ end_block ≥ issued_edge
  prefetch_window_grows(will_wait, overran, streak, level == Green) ⇒ prefetch_foreground_waits += 1; window = min(window × 2, cap)   // :13093–13104
  active = stream_gauge.touch(lane); resident_share = share × hot_budget / 100 / bs / active
  plan_bound = min(window, cap) → prefetch_window_hwm (max)                   // :13123–13131
  r2_declines = lane armed ? resident_share < R2_WINDOW_START : resident_share == 0     // :13144–13148
  r2_declines ⇒ read_lane_top_up(...); return                                 // §5.5
  lane.next_prefetch_block ≤ end_block ⇒ next = issued_base = end_block + 1   // rebase: the skipped span was never issued, :13178–13181
  consumed_edge < suppress_until_edge ⇒ return                                // quiescence gate
  total_blocks = ceil(meta.size / bs)
  loop:
     !prefetch_issue_admits(window, cap, resident_share, inflight, unconsumed) ⇒ break
     next = lane.next_prefetch_block; next ≥ total_blocks ⇒ break
     CAS next → next + 1 or continue
     inflight += 1; prefetch_issued += 1; prefetch_inflight_bytes += bs
     pending_blocks.insert(lane_pending_key(generation, next))               // mark BEFORE spawn (a mark after could lose the remove), :13214–13219
     !spawn_prefetch_task(...) ⇒ roll back: pending.remove, inflight −= 1, inflight_bytes −= bs, prefetch_wasted += 1; break   // issued ≡ completed + wasted, :13229–13246
```
Why the R2-declines boundary is `< 2` when the lane is armed (`:13132–13148`): a share of 1 cannot express the starting plan and its hot-probation landings are clock-churned by the demand fills sharing the tier — field 16-job row: hot residency ≈ `hot_budget / aggregate fill rate` ≈ 9 ms, 575 of 823 issues evicted-unconsumed, every lane quiesced while foreground reads waited 5.4 ms on demand fills. Under `SQUEEZEFS_READ_LANE=0` the boundary stays `== 0` (exact prior behavior — the lever's contract).

Why the quiescence arm is PROGRESS-clocked (`:6437–6451`): a 2 s wall-clock arm froze healthy lanes wholesale — 283 of 4,096 blocks pipelined, row 2 at 0.64× row 1. Geometric span growth separates a transient budget wobble (streak resets on the next clean consume) from sustained starvation (spans double until the fetch ratio converges to ~1.0×).

Why `issued_base` exists (`:6422–6430`): without it the plan-REBASE span (blocks the reader skipped past, never issued) counted as pipeline coverage and every foreground fetch inside it fired the detector — ≈ 1,700 false positives on the row-2 shape, freezing healthy lanes at issued ≈ 33.

### 5.4 The prefetch task (`spawn_prefetch_task`, `:13357–13499`)
```
spawn_bg(async {                                                   // foreground always wins (bg_admit)
  TEST_PREFETCH_TASK_DELAY_MS seam at first poll                   // the parked-task window held open deterministically
  settle = |completed| { pending.remove(key(generation, block)); inflight −= 1; prefetch_inflight_bytes −= bs;
                         live = lane.generation == generation
                         completed ∧ live ⇒ prefetch_completed += 1; block ≥ consumed_edge ⇒ unconsumed += 1   // a slow fill behind the edge has no drain left, :13397–13405
                         else prefetch_wasted += 1 }
  stale generation ⇒ settle(false)
  pre-step layout entry ⇒ settle(false)                           // a pre-step map names bindings the writer may have freed
  key = block_key_in(meta, block): Hole ⇒ settle(true); NeedsAuthority ⇒ load_striped_block_keys
  GDS arm (preserved verbatim) ⇒ settle(true)
  nvme tier resident ⇒ prefetcher.prefetch(addr, len) (page prefetch); settle(true)
  get_cached_or_fetch_block_traced(key, FillClass::Prefetch) ⇒ settle(ok)   // FULL ghost semantics, :13488–13497
})
```
Why prefetch keeps full ghost semantics (`:12211–12222`): a ghost BYPASS (tried first) meant a genuinely re-read stream never admitted to the disk tier — warm re-reads stayed device-bound forever (9.2 vs the 16.6 GiB/s lineage). Same-pass fake heat is prevented MECHANICALLY (the one-lap clock grace + progress-clocked quiescence keep one pass ≈ one miss per key), so a second recorded miss is real cross-pass heat.

Abandonment (`docs/design-read-path.md:311`): deliberately no io_uring cancel plumbing — a stale-generation fill completes its probation put and is first in eviction line; `prefetch_wasted` prices the cost before anyone builds machinery for it.

### 5.5 The ahead lane — probe-governed (`read_lane_top_up`, `:13695–13920`; `read_lane.rs`)
```
read_lane_top_up(path, meta, bs, end_block, lanes, lane_idx, generation, active_streams, level, will_wait):
  !lane.enabled ∨ bs ≤ 256 KiB ⇒ return                                  // :13712–13715
  lanes.dest_lease_fresh(now) ⇒ return                                   // BOTH entries decline (touch-driven and completion-driven), :13716–13726
  file-level single issuer: owner != lane ∧ owner set ∧ now − owner_ms < 2 s ⇒ return; else claim   // :13727–13740
  budget_cap = effective_mem_budget() / 8
  depth = read_lane_depth_blocks(pin, governed, bs, streams, red, budget_cap)   // red ⇒ 0; min(pin ?? governed, cap / bs / streams); read_lane.rs:163–182
  if !depth_pinned:                                                      // the probe-epoch drive, :13764–13771
     will_wait ∨ depth == 0 ⇒ note_probe_saturation()
     headroom = !red ∧ depth < budget_cap / bs / streams
     probe_epoch_tick(headroom)                                          // composes headroom ∧ !pressure (ahead evictions moved), read_lane.rs:738–759
  read_lane_depth_target = depth
  depth == 0 ⇒ return
  set_hold_budget(hold_budget_bytes(mem, bs, streams, depth + derived_cap))   // the hold must cover the ahead window AND the demand population's retirement lag, :13789–13800
  rebase as R2; consumed_edge < suppress_until_edge ⇒ return
  examine_cap = max(derived_cap, depth); walk_horizon = end_block + 1 + examine_cap           // the reader-tied horizon, :13832–13838
  loop:                                                                  // the marginal-issue walk
     !lane_issue_admits(rl_inflight, depth, lane.inflight_bytes(), bs, budget_cap) ⇒
          rl_inflight ≥ depth ∧ !pinned ⇒ note_probe_saturation()        // the depth-bound exit IS saturation; the cap exit is NOT (headroom exhaustion), :13849–13858
          break
     examined ≥ examine_cap ⇒ break
     next = cursor; next ≥ total ∨ next > walk_horizon ⇒ break
     lane_block_covered(meta, next) ⇒ CAS cursor; examined += 1; read_lane_covered_skips += 1; continue   // ONE sync probe, never a task, never a depth slot, :13872–13882
     CAS cursor or continue; rl_inflight += 1; lane.add_inflight(bs); pending.insert(...)
     !spawn_read_lane_task(...) ⇒ roll back; read_lane_wasted += 1; break
```
* `lane_block_covered(meta, block)` = Hole ∨ key ∈ inflight ∨ hold.contains ∨ hot.get_no_promote ∨ lru.get_no_promote ∨ nvme.has (`NeedsAuthority` reads UNCOVERED; `:13665–13680`). Without the walk ~92 % of depth-1 issues skip-settled through full task latency and probe delivery could never respond (`:13820–13824`).
* `lane_issue_admits(rl_inflight, depth, inflight_bytes, bs, cap) = rl_inflight < depth ∧ inflight_bytes + bs ≤ cap` (`read_lane.rs:186–194`).
* The lane task (`:13945–14060`): same fencing as the prefetch task; resident anywhere ⇒ skip; else `lane_fetch_block`; a LIVE completed settle **re-drives `read_lane_top_up`** from `consumed_edge − 1` (contract 14: touch-driven-only issue degenerated fill concurrency to request-arrival cadence — the field's flat D4/D8/D16 ladder at ~50 in flight vs the raw row's 160; `:13929–13943`); an error/stale settle never re-drives (a failing device cannot self-loop).
* `lane_fetch_block(key)` (`:14076–14156`) — the LEDGER-INVISIBLE fill: `is_damaged_mapping ⇒ EIO`; `insert_sync(key, tx)` lost ⇒ `Ok(())` (the reader dedupes onto the owner); `cache_misses += 1`; guard; `before`; fetch; `publishable ⇒ level != Red ⇒ hold.insert_validated(key, bytes, budget, still)`; `serve_valid = still`; `!serve_valid ⇒ purge_block_key`; `read_lane_fetches += 1`, `read_lane_fetch_bytes += len`, `probe_on_fill_bytes(len)`; send `FillResult{bytes, serve_valid, verdict_word}`; phase `fill_total` (lane fetches count — field D4 undercounted 24,748 vs 372 k). No ghost, no governor, no tier publication.

The engage governor = `write_pipeline_core::ProbeCore` verbatim (`:120–298`):
* `PROBE_EPOCH_MS = 500` (two governor windows: long enough for a windowed rate, short enough that a full ramp to ×32 at +¼/epoch completes in seconds); `PROBE_MUL_ONE = 64` (Q6 fixed point); `PROBE_MUL_MAX = ×32` (a runaway guard — the R5 cap is the operative bound); `PROBE_COOLDOWN_EPOCHS = 8` (dead-gain latency tax ≈ 1/(N+1) of epochs, the BBR duty cycle).
* Per epoch (the CAS winner rolls; `bps = epoch_bytes × 1000 / elapsed`):
  * PROBING: `saturated ∧ bps ≥ baseline + max(baseline/16, 1)` ⇒ ADOPT (`adopted_bps = bps`, re-arm immediately); else RETREAT to `prev_mul` (+ cooldown iff saturated), `backoffs += 1`.
  * HOLD: `!saturated` ⇒ decay `mul − mul/8` toward 1.0 (the latency guard); `collapsed = mul > 1.0 ∧ adopted > 0 ∧ bps < adopted − adopted/8` ⇒ step back `mul − mul/5` (senior to launching — a collapsed baseline must never seed a probe); cooldown ticks down; `headroom ∧ mul < max ∧ bps > 0` ⇒ LAUNCH `mul + mul/4`, `ups += 1`.
* `probe_governed_depth(mul) = (mul − 64) × 4 / 64` blocks: 64→0, 80→1, 100→2, 125→3, 156→5, 195→8 (`read_lane.rs:119–141`, pinned table). Delivery = TOTAL completed fill bytes (demand + lane), so an ahead lane that merely displaces demand fetches reads as dead gain and retreats.
* `probe_epoch_tick(headroom)`: `saturated = sat_marks moved since the last roll`; `pressure = read_lane_hold_ahead_evictions moved`; `roll(now, saturated, headroom ∧ !pressure)` (`read_lane.rs:738–759`).

**The two FALSIFIED open-loop derivations — recorded so they are never re-derived** (`.benchmarks/2026-08-01-read-lane.md:76–101`; `docs/design-read-path.md:299`; `read_lane.rs:149–158`):
1. **Pure BDP** (measured lane bandwidth × measured latency floor): the write campaign's self-fulfilling-equilibrium lesson reproduced on reads — the target converges to sustaining the CURRENT plateau; `read_lane_depth_target` floor-locked at 2 with 32+ streams; a depth-8 pin changed nothing (25.72 vs 26.28 GB/s).
2. **The §5.5 AIMD window as the ahead depth** (engaged at depth 2/4/16 alike): every engaged row LOST to hold-only — 21.07/20.87/20.79/19.33 vs 25.98 GB/s at qd8, `read_amp` 1.15–1.27 vs 0.71; ~650 GB/row of ahead deposits died FIFO-unconsumed (`read_lane_hold_evicted_unconsumed ≈ lane fetch bytes`) because the client's own qd × streams (256 × 1 MiB) already covered the fabric BDP.
Both were DELETED (no dead code); the closed-loop probe is what shipped. Under a demand-covered venue the governor probes and correctly retreats — `backoffs ≈ ups ∧ read_lane_fetches ≈ 0` is the designed shape, not a bug.

### 5.6 Ring-side feed (`ring_read_lane_touch`, `:13277–`)
IPC ring reads observe ONCE at the sink — warm serves AFTER completion (silent consumption must advance the consume edge), misses BEFORE the ladder (the 4th contiguous op's classification vetoes direct-drive for the 5th); demoted ops carry `lane_pre_fed` so the handler never observes twice (16 double-observations declassify, `:7286–7296`). Rung 7 wires the caller; the hook belongs to rung 4.

---

## 6. Scan-resistant admission (R1b)

### 6.1 The measurement that motivates it
* Reading 16 GiB cold: device reads 16.35 GiB (1.02×) **plus 16.9 GiB of tier WRITES** — one 4 MiB mmap memcpy + dirty-page writeback per cold block, fighting the read stream for the same device (`.benchmarks/2026-07-11-…attribution.md:300–301`; `docs/design-read-path.md:66`).
* Pre-fix perf: 44 % of daemon CPU in memcpy during read storms (`:66`).
* Skipping a publish is ALWAYS correctness-safe (absence ⇒ the next reader goes to the device); the risky direction was never "don't cache", it was "cache wrongly" (`:211`). Admission (don't insert) beats eviction (insert then regret): ring geometry was exonerated — `get_obj/unique = 1.002` with geometry untouched (`:450`).

### 6.2 Policy knob
`SQUEEZEFS_READ_TIER_ADMISSION ∈ {always, second-touch, never}`, default `second-touch` (`routing.rs:11458–11466`); hot budget 0 ⇒ `always` with a warning (`:11467–11476`). Scope: fills > 256 KiB (`:12525–12529`).

### 6.3 The ghost table (`GhostTable`, `:5977–6044`)
```
SLOTS = 2^16 AtomicU32 (~256 KiB, once per router); EPOCH_MISSES = 2^15
slot = xxh3(key) & 0xFFFF
tag(hash, epoch) = ((hash >> 16) as u32) ^ (epoch × 0x9E3779B9), never 0 (0 = empty)
check_and_record(key):
   seen = slots[slot]; hit = seen == tag(epoch) ∨ seen == tag(epoch − 1)    // TWO compares — the window SLIDES one epoch at a time
   slots[slot] = tag(epoch)                                                 // record regardless
   !hit ⇒ misses += 1 ≥ 2^15 ⇒ misses = 0; epoch += 1                       // racy-tolerant: a double roll narrows one window
```
* A single-epoch match was an earlier draft; it created a table-wide admission dead zone at every flip (`docs/design-read-path.md:193`).
* Collision model, stated honestly: direct-mapped single-tag — a 1 TiB interleaved cold scan (256 K blocks) wraps the table ~4× and can overwrite a warm record before its second touch; the cost is a third/fourth-touch admission absorbed meanwhile by the hot tier (R-1). The 2-way tag set is the pre-agreed upgrade if `read_tier_admission_ghost_hits` underperforms the interleaved-scan gate row (`:193, 542`). Holds only hashes — no payload, no filenames (`:478`).

### 6.4 Second touch ⇒ protected + validated publish
At the fill site (§4): `hit = ghost.check_and_record(key)` ⇒ `read_tier_admission_ghost_hits`; `ghost_admit = hit ∧ (!class.streaming() ∨ governor.allow_stream_admission(len))`; `!ghost_admit ⇒ read_fill_publishes_skipped`; else `read_tier_admissions` (≥ 64 KiB).
Hot class ladder (`routing.rs:12697–12730`):
* admitted ∧ second-touch ⇒ `Protected` (`ProtectedStream` if streaming — payback-exempt so within-pass consumption never dilutes the waste ledger);
* else streaming ⇒ `ProbationReferenced` — the one-lap grace: without it the clock evicts the pipeline's FUTURE to keep the stream's PAST (595 hot-evict refetches on row 2); a transient-window `DemandStream` fill loses the clock race mid-consumption to sibling streams (kern seq-1M −9 %, fetch ratio 1.19× vs 1.07×);
* else `Probation`.
Publish: `< 64 KiB` inline `cache_read_block_if`; else the deferred validated publish (§4). **Dehydration gate**: only `Protected` hot victims dehydrate; probation-and-never-read victims drop (`cache/mod.rs:292–347`). Dedupe at the channel mouth (`hot_block_dehydrate_skips`).

### 6.5 The hold-serve ceremony (`hold_serve_admission`, `routing.rs:13563–13653`)
A serve of a DEMAND-deposited hold entry after every tier probe missed stands in for exactly the device refetch the pre-lane read would have paid, so it runs the fill-path admission arm verbatim:
```
before = fill_incarnation(key).filter(still) else return                 // outside the publishable window ⇒ no ledger touch
ghost_admit as §6.4 (second-touch ⇒ check_and_record; streaming ⇒ allow_stream_admission(len))
!ghost_admit ⇒ read_fill_publishes_skipped += 1
else read_tier_admissions += 1; run_blocking(nvme.cache_read_block_if(key, held, still)).await    // AWAITED: the tier copy is visible at the serve boundary
hot_block_misses += 1; hot_class ladder as §6.4; hot_block.put_class_validated(key, held, class, still)
!still(key, before) ⇒ purge_block_key(key)
```
Two deliberate differences from the fill arm: no `note_foreground` (the serve pays ZERO device bytes — no self-funding) and no re-deposit (`:13548–13554`). Lane-fetch deposits (`ledger_visible = false`) never reach it (`read_lane_tests` contract 2). On the sync fast path the ceremony is DISPATCHED to a handler lane because it awaits — `tpc_spawn_guarded("hold_serve_admission")`, never a runtime inject queue (the ~130 µs/op term; `:18382–18420`).

### 6.6 The admission governor — the bounded-waste law (`AdmissionGovernor`, `:6104–6387`)
* Finding (2026-07-26, `:6104–6111`): cold-dominated random 4k over a working set ≫ budget collapsed the hybrid posture ~20× — ghost escalations fetched whole 4 MiB blocks that evicted before reuse; the per-key cooldown alone under-bounded it ~7× via 2^16-slot collisions.
```
EPOCH_MS = 2000 (test seam TEST_ADMISSION_EPOCH_MS)                                      // :6173–6208
roll(): CAS epoch → shift evicted/wasted/foreground cur → prev (0 if non-adjacent); tokens = foreground_prev × fill_pct / 100   // mint once; leftovers die, :6211–6239
note_foreground(bytes): roll; foreground_cur += bytes                                      // ranged device-read site + DemandStream fills, :6249–6253
on_eviction(len, class): Protected only; roll; evicted_cur += len; waste = len − served_bytes;
                          waste > 0 ⇒ wasted_cur += waste; read_admission_wasted_bytes += waste;
                          served == 0 ∧ !stream_admitted ⇒ read_admission_evicted_unhit += 1    // :6259–6287
clamp_engaged(block) = roll; wasted ≥ 2 × block ∧ 2 × wasted ≥ evicted → gauge read_admission_governor_clamped   // :6295–6303
reserve_tokens(block) = tokens.fetch_update(checked_sub(block)).is_ok()                     // ONE CAS, :6336–6341
allow_escalation(block)       = !(clamp ∧ !reserve) else read_admission_governor_denials += 1; false   // :6346–6355
allow_stream_admission(block) = !(clamp ∧ !reserve) else read_admission_stream_transients += 1; false   // :6370–6379
escalation_would_admit(block) = NON-reserving peek (the rung-7 direct-drive prelude)             // :6317–6326
```
* Eviction-side ratio ON PURPOSE: an admission burst must not dilute the ratio and unclamp itself (measured as the clamp flapping open at ~6× the fill budget, `:6160–6163`).
* The token CAS: a check-then-add herd under 256-deep O_DIRECT admitted ~6× the budget — 93 admits/s where 5 % of foreground allowed 15 (`:6328–6335`); the earlier two-window spend arithmetic had a roll-boundary race re-admitting a herd per boundary (exactly 2× the budget) and an oscillation (`:6149–6158`).
* Denials record NO cooldown so genuinely hot keys retry and win the trickle (skew convergence, `:6129–6132`).
* **The bound**: `wasted_bytes ÷ device read bytes ≤ fill_pct % (default 5)` under churn (`docs/design-read-path.md:497`); `SQUEEZEFS_READ_ADMISSION_FILL_PCT` 0..=100 (`cache/mod.rs:148–163`). Fitting sets produce no evictions ⇒ no waste ⇒ never clamp ⇒ byte-identical warm-up (`:6134–6137`).

### 6.7 The transient stream window (2026-07-29, `docs/design-read-path.md:198`)
A classified stream's ghost hit is NOT admission by right: `DemandStream`/`Prefetch` ghost hits admit only through `allow_stream_admission`; granted ⇒ `ProtectedStream` (exempt from `evicted_unhit`; `get_serving` credits nothing — probation would have served the within-pass consumption identically, so the admission's marginal value is cross-pass retention only); denied ⇒ `ProbationReferenced` + skip. Field: pass-2+ stream refills admitted protected + published, `read_admission_wasted_bytes` +454 GB/120 s ≈ 42 % of device reads; the within-pass credit diluted the ratio and held the clamp open for co-tenants.

---

## 7. Incarnation validation before/during/after; the rebind ladder; the serialized settle

### 7.1 The seqlock discipline — every fill, verbatim
```
before = fill_incarnation(key)                 // None = unstable ⇒ never publish (snapshot, incarnation_core.rs:60–67)
bytes  = device read
publishable = before.filter(|b| still(key, b)) // the AFTER check — the Acquire fence orders the payload read before this load (:69–80)
deposit INTO every tier with validate = || still(key, before)   // finding 17: validated AT the insert's visibility point, :12564–12577
serve_valid = still(key, before)               // the completion check after the puts
!serve_valid ⇒ purge_block_key(key)            // undo — a poisoned entry is at worst transient, never sticky (:12738–12762)
```
* Writers `retire` (gen+1, unstable) BEFORE touching device bytes under a reused key and `publish` (stable) only after the durable write; `retire`'s trailing `Release` fence pairs with `still`'s `Acquire` fence (found by the loom models, `incarnation_core.rs:36–51, 69–80`). `UNSTABLE_FIRST = 1 << 1`, `STABLE_FIRST = 1`, `UNKNOWN_STABLE = u64::MAX` (`:28–34`).
* Why finding 17 (`routing.rs:12566–12572`): the put-then-revalidate-then-undo shape exposed its undo window — a blocking-pool deschedule across a W1 patch deposited the dead incarnation and a post-ACK read served it (the ~5 %/run `data_path_correctness` flake).
* Untracked keys (legacy `…/part_N`, unknown backends) are vacuously valid and never cache-published (`:12486–12489, 5157–5165`).
* The device leg REFUSES a dead-lifetime key before the DMA (`:4028–4033, 4066–4071`).
* Waiters re-run `still(key, verdict_word)` at THEIR serve instant (generic/209, `:12392–12416`).

### 7.2 The binding-validated walk (`get_block_for_index_class`, `:14379–14596`)
Proof (`:14277–14287`): bytes for key K serve block `b` only when (a) the fill was incarnation-valid ∧ (b) the CURRENT map still binds `b → K` once the bytes are in hand. If K were rebound to `b` after its observed incarnation died, it must have been freed (gen+1) and republished (gen+1) in between — (a) would have failed — so (a) ∧ (b) proves the serve is the block's current content, linearized at the recheck. The recheck MUST follow the read (`:14549–14551`).
```
MAX_REBINDS = 24; SETTLE_HANDOFF = 8; BACKOFF_AFTER = 4; CONTENDED_BEFORE_ESCALATE = 2     // :14431–14434
ladder_cap = escalate_contended ? SETTLE_HANDOFF : (TEST_REBIND_LADDER_CAP ?: MAX_REBINDS)
fill_class = override ?? (!device_true ∧ lanes[file].any_streaming_fresh() ? DemandStream : Demand)   // ONCE per call, :14451–14462
key = resolved_key (BORROWED) ?? current_block_binding(path, b)                             // a stale-ABSENT None is re-resolved fresh, :14463–14487
for attempt in 0..ladder_cap:
  key == None ⇒ Ok(None)                                                                    // a FRESH-map hole
  attempt ≥ 4 ⇒ sleep 1 << min(attempt − 4, 3) ms                                           // allocator free-list reuse cycles one offset through alloc→DMA→publish — the loser needs the storm to pause
  escalated = escalate_contended ∧ losses ≥ 2
  escalated ⇒ guard = BLOCK_FLUSH_LOCKS(ino, b, ReadEscalate)                              // :14500–14512
  fetched = device_true ∨ escalated ? fetch_block_device_true(key) : get_cached_or_fetch_block_traced(key, fill_class)
  Err(e) ⇒ current == key ⇒ return Err(e)                                                   // a real I/O/corruption error
           else losses += 1; stale_binding_rebinds += 1; key = current; continue            // a dead incarnation fails AEAD/LZ4 on transformed volumes — a LOSS, not corruption, :14520–14547
  TEST_BINDING_RECHECK_DELAY_MS seam
  still_bound = block_binding_is(path, b, key); record(BindingCheck)
  incarnation_valid ∧ still_bound ⇒ Ok(Some(val))
  losses += 1; stale_binding_rebinds += 1; key = current_block_binding(path, b)             // only a LOSS materializes a String, :14562–14573
escalate_contended ⇒ get_block_for_index_settled(path, b)                                   // §7.3
stripe_held_settle  ⇒ get_block_for_index_settled_stripe_held(path, b)                      // the fold-seed twin, (3.5) only
else Err("did not settle after {ladder_cap} binding rebinds")                               // non-escalating callers keep the loud EIO
```
* Why device-direct when escalated (`:14405–14412`): the cohort fill of the shared single-flight was produced OUTSIDE the stripe by a non-escalated reader, so inheriting it re-imports exactly the invalidity the stripe was taken to exclude (the generic/464 rebind-exhaustion EIO).
* `fetch_block_device_true` (`:14245–14264`): no tiers, no single-flight (N diagnostic readers = N device reads), no ghost, no `cache_hits/misses`; decode still applies; returns the same serve-validity verdict.
* The cap is a LATENCY knob for escalating callers (8: after two stripe-held losses more chases rarely win and the settle is one guaranteed fetch) and a CORRECTNESS envelope for non-escalating ones (24, unchanged) (`:14426–14430`).

### 7.3 Exhaustion ⇒ ONE serialized settle, never EIO for a pure read (`get_block_for_index_settled`, `:14642–14680`)
```
SETTLE_ATTEMPTS = 4                                           // retries exist ONLY for the invariant-violation case
stale_binding_escalations += 1
for attempt in 0..4:
  attempt > 0 ⇒ sleep 1 << min(attempt − 1, 3) ms
  _block = BLOCK_FLUSH_LOCKS(ino, b, ReadSettle)              // (3): excludes every stripe-holding custodian (write-through checkout, writeback, fold, W1 patch, upload, mover)
  _map   = INODE_META_LOCKS(ino)                              // (3.5): the one-merge domain — every map publish funnels through merge_block_mappings* here, incl. mutators that never take (3)
  _serve = settle_serve_window(ino)                           // finding 25 rung A: exclude SERVED publishes; None on solo mounts
  match settled_resolve_fetch_locked(path, b):                // resolve the binding UNDER both locks + a device-direct validated fetch, :14741
     Hole ⇒ Ok(None) | Fetched(val) ⇒ Ok(Some(val)) | Lost ⇒ continue   // Lost = a mutator outside both disciplines: tripwire read_settle_lost_serialized (:14826)
Err("did not settle after 4 serialized settle attempts — see invariant_tripwires")
```
* Acquisition order (3) → (3.5) is the write path's own extended order (`stripe_locks.rs`; nothing acquires (3)/(1) while holding (3.5), so the order is acyclic); the arm frees nothing (`:14614–14620`).
* Holding the same locks every publisher needs SERIALIZES with — never spins against — the writer: the storm parks for one device-fetch RTT and the reader provably wins (`:14619–14622`).
* Field motivation (2026-08-04): an escalated attempt's fetch key is a binding snapshot taken OUTSIDE the guard, so under continuous legal displacement the reader chased the writer one generation behind forever — the rebind-starvation EIO (`:14413–14417`).
* Stripe-holding SEED callers (the fold's `fetch_seed_image`) take the `(3.5)`-only twin — re-taking (3) would self-deadlock; counted `seed_settle_escalations` (`:14682–14710`).
* Test seam `TEST_REBIND_LADDER_CAP` overrides the non-escalating cap (24 consecutive storm wins is a schedule lottery on a clock-capped box; `:5925–5934`).

### 7.4 The window-class escalation (handler)
Fingerprint churn past 2 lock-free re-reads, or a LIVE overlay record inside the window ⇒ `read_window_settled` per block under (3) + (3.5), `overlay_window_escalations += 1` (`fuse_client.rs:34344–34359`). Lock order overall: (1) read guard → DROPPED → (3) → (3.5); never (3.5) then (1)/(3) (charter §2).

---

## 8. The zc fill-pool serve and the read copy ledger

### 8.1 The fill pool as ONE `MAP_SHARED` memfd slab (`cache/pool.rs:756–801`)
```
ZC_FILL_POOL: Lazy<Option<Arc<AlignedBufPool>>> =
   !bool_knob("SQUEEZEFS_READ_ZC_SERVE", true) ⇒ None
   capacity = block_pool_capacity_from(cores) = max(cores × 16, 64)                    // :319–321
   AlignedBufPool::new_memfd_slabbed(capacity, 4 MiB):                                 // :531–578
      memfd_create("sqz-read-fill-pool", MFD_CLOEXEC); ftruncate(total); mmap(MAP_SHARED, PROT_RW)
   Err ⇒ log loud; None (the lever declines — copy economics, never correctness)
zc_fill_fd_offset(ptr, len) -> Option<(fd, off)>   // iff [ptr, ptr+len) lies on the slab — THE one probe every fd-source site runs, :380–390, 799–801
read_bounce_pool(size) = size ≤ RANGED_BUF_SIZE (64 KiB) ? RANGED_BUF_POOL : ZC_FILL_POOL ?? ALIGNED_BUF_POOL   // :832–838
```
* A SEPARATE pool from `ALIGNED_BUF_POOL` on purpose: the write path's `ActiveBlockBuf` backings keep their substrate, so the A/B moves the READ serve and nothing else (`:764–768`).
* The same bytes are reachable by VA (fills, decode, slicing) and by fd (`READ_FIXED(fd → slot)` on the transport ring) — the ZcBounce two-way-reachability law (`:365–370`; `env_knobs.rs:245`).
* Serve sites: the router's hot, hold and cold fill-slice arms via `zc_fd_source(zc, dest, src) = zc minted ∧ transport dest exists ∧ slab memory` (`routing.rs:7344–7349`) hand back the `Bytes` slice itself and count `note_zc_pool_serve(len, warm)` → `read_zc_pool_serve_bytes` (+ `read_zc_pool_serve_warm_bytes` for hot/hold) (`:7352–7361`); the fast probe's `offer_fd_body` (`fuse_client.rs:9137–9154`); the handler re-probes at reply time (`:34386–34389`). The transport bridges it — the port's `Handle.replyFdBody` (`filesystem_vtable.zig:77–80`). `fuse3_zc_fd_body_fallbacks` (an fd bridge re-staged through the bounce — never a lost reply) must stay ≈ 0.
* Non-pool sources (NVMe read-cache mmap, transform decode output, over-capacity fresh backings) keep the copy path, counted (`env_knobs.rs:245`).
* Measured (`.benchmarks/2026-09-05-r4-read-zc-serve.md:270–278`): daemon CPU per GiB **−24 % cold / −25 % warm** on the clean reversed bracket (559 vs 725, 566 vs 756), A-B-B-A both orders, closure exact, fd-body fallbacks 0. Passes/byte 2.0 warm / 2.69 cold → 1.0 / 1.69.
* Mem component `zc_fill_pool`: floor 64 MiB, weight 2, `trim_to` frees over-capacity backings only (slab slots are one mapping) (`fuse_client.rs:26899–26914`).

### 8.2 The read copy ledger — closure identities (`docs/design-read-path.md:505–507`; `fuse_client.rs:5639–5766`)
Every daemon CPU pass over read payload bytes is attributed AT the copy:
* `read_copy_dest_bytes` — serve copies into the registered dest (hot/hold/NVMe/fill-slice/ranged-bounce/staged/compose arms).
* `read_copy_warm_serve_bytes` ⊆ dest — the tier-buffer subset; partitioned EXACTLY by `read_copy_hot_serve_bytes + read_copy_hold_serve_bytes + read_copy_cache_serve_bytes` (counted at the copy, so the subset law holds under a rebind retry).
* `read_copy_fill_slice_bytes` ⊆ dest − warm — the cold whole-block fill → dest slice-out (the EXA row's one daemon pass).
* `read_copy_bounce_bytes` — heap bounces (None-dest tier serves; pooled-repr slices).
* `read_dest_dma_bytes` — device DMA straight into the dest (raw full-block leg, ranged zero-copy leg); `read_dest_lease_bytes` ⊆ dest_dma (validated lease serves).
* `read_zc_serve_bytes` — the zc direct leg (zero daemon passes); `read_zc_pool_serve_bytes` — fd-source serves (`_warm_bytes` the hot/hold subset).
* `read_fill_dma_bytes` — device DMA into pooled intermediates (`fill_dma / user ≈ read_amp`, the RX-copy pricing check).
* `nt_read_serve_bytes` — the NT-store subset of dest copies (`SQUEEZEFS_NT_READ_SERVE` default on, floor `SQUEEZEFS_NT_READ_SERVE_MIN = 256 KiB`); arena dests are EXEMPT — NT lost −2.8 % on il (the client's `slab_read` consumes within ~one op), won +11 % on ring-ent dests (`routing.rs:7363–7386`).

Closure (the row-validity instrument):
* kernel: `dest + bounce + dest_dma + zc_serve + zc_pool_serve ≈ user bytes` (× the ramp);
* `warm_serve ≡ hot + hold + cache`;
* `lease → dest` byte-for-byte on a leaseable row; `lease ≈ served ∧ dest → ~0 ∧ fill_dma → ~0` on a cold aligned streaming row;
* il: `dest + arena + dest_dma ≈ ipc_bytes_out`.
Twenty per-op READ counters are `ShardedAtomic`, folded on read (`fuse_client.rs:5398–5766`; charter §10).

---

## 9. Hybrid I/O and `-o direct_device_true`

* **Law** (the 2026-07-15 directive — "hybrid I/O between buffered and O_DIRECT, best of both worlds, regardless of if the user is requesting O_DIRECT or not", `docs/design-read-path.md:200`): O_DIRECT is NOT an admission class. The flag labels observability only (`read_odirect_requests`, `read_odirect_tier_serves`, `read_odirect_ghost_admits`); O_DIRECT reads serve from every tier on hit (binding-validated like all serves) and their misses ride the same ghost second-touch admission as buffered.
* **Ranged second touch escalates** (`ranged_escalation_candidate`, `routing.rs:14880–14885`; dispatch `:14954–15003`): `ghost_hit ∧ level != Red ∧ !cooldown.recently_escalated(key)` ∧ `governor.allow_escalation(bs)` ⇒ `cooldown.record(key)`, `ranged_read_ghost_escalations += 1`, odirect ⇒ `read_odirect_ghost_admits += 1`, ONE whole-block `get_block_for_index_class(..., Some(FillClass::Escalation))` whose publish is AWAITED, then `slice_whole_for_ranged`.
* `EscalationCooldown` (`:6046–6102`): `2^16` direct-mapped `AtomicU32` tags, wall-clock `EPOCH_SECS = 32`, two-epoch match ⇒ a key re-escalates at most every ~32–64 s. The ghost proves REUSE, the cooldown bounds CHURN: fitting sets converge once and stop touching the dispatch; beyond-tier sets degrade to the device-true ranged path between windows instead of re-fetching + re-publishing 4 MiB per eviction (the R-5 spiral in tier form — the 16.9 GiB tax resurrected as churn).
* **The ruler**: `-o direct_device_true` (a daemon-level option stripped from the kernel string) or `SQUEEZEFS_DIRECT_DEVICE_TRUE=1` (`:11498–11506`; `set_direct_device_true`, `:11848–11851`). `device_true = armed ∧ hint.odirect`:
  * skip the classifier/pipeline (prefetch fills are publishes an escape-mode reader would never consume — 2× amplification otherwise), skip every tier serve probe (`:20015–20022`);
  * no ghost record, no escalation, no cooldown (`:14914–14916`);
  * no single-flight — N diagnostic readers = N device reads; no `cache_hits/misses` accounting (`:14234–14244`);
  * passthrough ⇒ EVERY request size is a ranged window (`:20567–20572`); transform ⇒ the whole-block device-true fetch (decode still applies, `:20850–20854`);
  * `read_device_true_reads += 1`; the `.stats` mode flag `direct_device_true: true`;
  * the fast probe DEMOTES device-true O_DIRECT (a tier serve would turn a device-true row warm — measurement fraud; `fuse_client.rs:26361–26364`); the sync ladder skips its hot/hold/cache legs (`routing.rs:18287–18293`);
  * overlay probes STAY — RYW correctness is never diagnostic-optional (`:20020–20022`).
* Measured (`.benchmarks/2026-07-15-hybrid-io.md:99–107`): warm fitting 768 MiB set 536,193–536,983 IOPS at 0 device r/s (1.96× JuiceFS warm); cold first pass over 16 GiB 316–323 k IOPS at ≈ 1.87× bytes (4,256 escalations ≈ the 4 Ki-block population — the bounded one-window admission tax); escape 335,543–335,828 IOPS at `get_obj Δ / ops = 1.0000`, zero serves/admits/escalations/tier bytes.

---

## 10. The warm ladder — the try-only probe the transport calls on the queue thread (charter §4 arm (a))

Reference chain: `fast_dispatch.rs:1–80` → `Filesystem::read_fast_probe` (`fuse_client.rs:26345–26477`) → `ipc_read_probe_locked` (`:16195–16288`) → `try_read_range_sync` (`routing.rs:18192–18445`). Port seam: `filesystem_vtable.zig:122–127` `probe_read(ctx, req, window) ?Reply` — SYNC, try-only, never a device touch, never an allocation.

What the term kills (`fast_dispatch.rs:5–16`): the hop reap thread → inbound queue → session dispatch task → lane spawn → first poll measured `queue_wait` 69 µs + `dispatch_lag` 89 µs = 158 µs of a 439 µs 4 KiB random read (36 %), with the device at 40 µs; unloaded: 6.7 µs hop vs 53 ns inline serve.

Rungs, in order, and what each MAY touch:
```
0 pre-screen (fuse_client.rs:26354–26385):
    virtual ino ∨ size == 0                       ⇒ Demote
    ServeStamp::begin()                            // the reader's purge-generation drain
    odirect ∧ direct_device_true()                ⇒ Demote     // a tier serve would make a device-true row warm
    retained extents on the ino                   ⇒ Demote     // the handler overlays them
    size > window.cap                             ⇒ Demote     // FUSE-4e: never a short read below EOF (the kernel would zero-fill)
    sink = Window{addr, cap, fd_body} | Heap(vec)  // the heap stand-in exists only for sink-less in-process suites
1 per-inode TRY-lock: inode_lock(ino).try_read() fails ⇒ Demote   (:26383–26386)   // port: op_guard.Table.tryAcquire(.shared)
    (the guard drops BEFORE any continuation; served bytes are already captured, :26387–26391)
2 size authority (ipc_read_probe_locked, :16205–16239):
    attr_cache miss                               ⇒ Miss        // the async getattr is unreachable here
    meta = router.metadata_cache.get(ino) (ONE get, shared with the lane touch)
    layout_entry_pre_step(meta)                   ⇒ Miss        // a pre-step entry names bindings the writer may have freed
    size = meta.size ?? attr.size; offset ≥ size  ⇒ Eof (0 bytes)
    multi-block                                   ⇒ Miss        // flush first
    device_overlay::any_open_fast() ∧ overlays.get(ino, b) ⇒ Miss   // the OVERWRITE population's old key has stale tier entries (R1), :16241–16252
3 active buffer (:16254–16287):
    present: extent repr ∨ !covered_contains      ⇒ Miss
             else Hit(snapshot.slice(rel))         // CoW snapshot; copied out AFTER the guard drops
    absent: → rung 4
4 sync tier legs (try_read_range_sync), each writing INTO the sink:
    "staged": W2 rider runs ⇒ None; ring blob ⇒ write_at + zero-pad; Served(read_len, Staged)       (:18208–18223)
    "inline": data_key ⇒ write_at + zero-pad; Served(read_len, Inline)                              (:18231–18240)
    "striped":
       multi-block ⇒ None; W2 runs ⇒ None                                                            (:18249–18260)
       staging ring active_block_path(file, b) ⇒ Served(n, Staged)                                   (:18269–18285)
       direct_device_true() ⇒ None                                                                   (:18290–18293)
       key = block_map[b] | partial sync resolve | part_N; NeedsAuthority/tombstone/hole ⇒ None     (:18294–18321)
       hot.get_serving(key, len) ⇒ hot_block_hits; offer_fd_body else write_at; hold.credit; Served(n, Hot)        (:18324–18345)
       lane armed: hold.serve_with_provenance(key, 0) ⇒ cache_hits, read_lane_serves, read_lane_serve_bytes, ipc_hold_probe_serves;
                   offer_fd_body else write_at; hold.credit; ledger_visible ⇒ tpc_spawn_guarded(hold_serve_admission); Served(n, Hold)   (:18361–18422)
                   else ipc_hold_probe_misses                                                        (:18423–18427)
       nvme.get_cached_read_block_range_zero_copy(key, rel, len) ⇒ cache_hits; write_at; Served(n, Cache)   (:18438–18444)
       else None ⇒ Miss ⇒ Demote
5 side effects on a serve (fuse_client.rs:26450–26468): fuse_ops += 1; odirect ⇒ read_odirect_requests else note_page_instantiation(ino) BEFORE the reply;
    router.ring_read_lane_touch(meta, ino, offset, served, missed = false)     // silent consumption must advance the consume edge
    ledger: Window ⇒ read_copy_dest_bytes (+ warm per-arm split); Heap ⇒ read_copy_bounce_bytes; fd body ⇒ read_zc_pool_serve_{,warm_}bytes
    Served(bytes) | ServedFd{body, fd, off}; silent on Demote (the handler owns every count)
```
Binding currency on rung 4 is structural: the caller holds the inode read guard and the key came from the CURRENT map; this daemon's writers update that map synchronously under the same lock (`routing.rs:18430–18437`).
Transport counts: `transport_fast_dispatch_serves` / `fuse3_zc_read_fusions` / `transport_fast_dispatch_demotes`; partition `serves + fusions + demotes ≡ READs`; `zc_read_fusions + demotes ≡ fuse3_read_inplace_replies` (`fast_dispatch.rs:67–75`).
Nothing in rungs 0–5 awaits, spins, allocates per op, or touches a device.

---

## 11. Knobs, gauges, counters, histograms, constants

### 11.1 Knobs (class per charter §1.6: delete / `SQUEEZEFS_TEST_*` seam / A/B lever; `env_knobs.rs:207–245, 300`)

| Knob | Kind / range | Default | Class | Doc |
|---|---|---|---|---|
| `SQUEEZEFS_READ_TIER_ADMISSION` | enum `always\|second-touch\|never` | `second-touch` | A/B lever (`always` = the pre-R1b control) | R1b policy; hot budget 0 ⇒ `always` |
| `SQUEEZEFS_READ_HOT_BLOCK_CACHE_MB` | int 0..2^30 | derived `max(8 MiB, read_mem/4)` | A/B lever (0 = tier off) | R4 budget |
| `SQUEEZEFS_READ_ADMISSION_FILL_PCT` | int 0..=100 | 5 | A/B lever | the governor's bounded-waste share |
| `SQUEEZEFS_READ_PREFETCH_WINDOW` | int 0..2^20 (≤ 4096 applied) | derived cap | A/B lever (0 = kill switch) | R2 window cap override |
| `SQUEEZEFS_READ_PREFETCH_SHARE_PCT` | int 1..=100 | 50 | A/B lever | prefetch share of the hot budget |
| `SQUEEZEFS_READ_RANGED_THRESHOLD` | int bytes | 262144 | A/B lever (0 = whole-block only) | R3 threshold |
| `SQUEEZEFS_READ_DEST_LEASE` | bool | on | A/B lever (0 = the fill+serve-copy control) | the dest lease |
| `SQUEEZEFS_READ_LANE` | bool | on | A/B lever (0 = the A0 control) | hold + lane |
| `SQUEEZEFS_READ_LANE_DEPTH` | int 0..2^16 | derived (engage governor) | A/B lever (pin; 0 = hold-only) | ahead depth |
| `SQUEEZEFS_READ_ZC_SERVE` | bool | on | A/B lever | the memfd fill pool |
| `SQUEEZEFS_DIRECT_DEVICE_TRUE` | bool | off | A/B lever (the measurement ruler) | device-true O_DIRECT |
| `SQUEEZEFS_NT_READ_SERVE` / `_MIN` | bool / int bytes | on / 262144 | A/B lever | NT dest copies |
| `SQUEEZEFS_IL_READ_DEST` | bool | on | A/B lever (rung 7) | arena dests |
| `SQUEEZEFS_FUSE_READ_FAST_DISPATCH` | bool | on | A/B lever (rung 3's transport) | arm (a) |
| `SQUEEZEFS_READ_MOSTLY_CACHE` | bool | on | delete (§13 Q16) | a container-choice lever |
| Test seams (`routing.rs:5900–5962, 6173–6180`): `TEST_TIER_PUBLISH_DELAY_MS`, `TEST_TIER_PUBLISH_MID_WINDOW_STALL_MS`, `TEST_TIER_PUBLISH_POST_PUT_STALL_MS`, `TEST_BINDING_RECHECK_DELAY_MS`, `TEST_REBIND_LADDER_CAP`, `TEST_FILL_PRE_DEPOSIT_STALL_MS`, `TEST_PREFETCH_TASK_DELAY_MS`, `TEST_ADMISSION_EPOCH_MS`; the handler's `read_window_stall` / `read_validate_stall` parks | process atomics | 0 | `SQUEEZEFS_TEST_*` seams in the port | one relaxed load each, zero cost unset |

Mount flags: `--read-mem-cache-size`, `--read-disk-cache-size` (size strings `128GB`/`50%`, `cache/mod.rs:395–453` — §13 Q5), `-o direct_device_true`.

### 11.2 Counters / gauges / histograms (`fuse_client.rs:5396–5766, 7921–7926`)

Serve ladder:
* `cache_hits`, `cache_misses`, `get_obj` (raw device-read OPS incl. ranged; data reads only — the health probe rides `probe_read_block`, uncounted).
* `hot_block_hits`, `hot_block_misses`, `hot_block_evictions`, `hot_block_probation_drops`, `hot_block_dehydrate_skips`, `hot_block_current_bytes`.
* `read_lane_serves`, `read_lane_serve_bytes`, `read_lane_holds`, `read_lane_hold_retired`, `read_lane_hold_bytes`, `read_lane_hold_evicted_unconsumed` (≈ 0 healthy), `read_lane_hold_ahead_evictions`, `ipc_hold_probe_serves` / `ipc_hold_probe_misses`.
* `singleflight_waiter_result_serves`.
Admission:
* `read_fill_publishes_skipped`, `read_tier_admissions`, `read_tier_admission_ghost_hits`, `read_admission_evicted_unhit`, `read_admission_wasted_bytes`, `read_admission_governor_denials` (≈ 0 on fitting sets), `read_admission_governor_clamped` (gauge), `read_admission_stream_transients`, `read_tier_publishes_paused`, `mem_budget_dehydrate_paused`.
* `read_streams_classified`, `read_odirect_requests`, `read_odirect_tier_serves`, `read_odirect_ghost_admits`, `read_device_true_reads`, `direct_device_true` (mode flag).
Prefetch:
* `prefetch_issued`, `prefetch_completed`, `prefetch_wasted` (`issued ≡ completed + wasted`), `prefetch_inflight_bytes`, `prefetch_window_hwm`, `prefetch_foreground_waits`, `prefetch_evicted_unconsumed` (the spiral detector), `prefetch_active_streams`.
Ahead lane:
* `read_lane_fetches`, `read_lane_fetch_bytes`, `read_lane_depth_target`, `read_lane_depth_probe_ups` / `_backoffs` (`backoffs ≈ ups ∧ fetches ≈ 0` = the falsified-venue shape, by design), `read_lane_covered_skips`, `read_lane_wasted`, `read_lane_inflight_bytes` (→ 0 idle; a floor = a leaked settle), `read_lane_armed`.
Ranged:
* `ranged_reads`, `ranged_read_bytes`, `ranged_read_unaligned_bounces`, `ranged_read_rebinds`, `ranged_read_ghost_escalations`.
Validation:
* `stale_binding_rebinds` (≈ 0 growth on quiet workloads), `stale_binding_escalations`, `seed_settle_escalations`, `overlay_window_escalations`, `staged_identity_retries`, `staged_payload_lost_reads`, `read_settle_stale_head_refetches`.
Ledger (§8.2):
* `read_copy_{dest,warm_serve,hot_serve,hold_serve,cache_serve,fill_slice,bounce}_bytes`, `read_dest_dma_bytes`, `read_dest_lease_bytes`, `read_zc_serve_bytes`, `read_zc_pool_serve_bytes`, `read_zc_pool_serve_warm_bytes`, `read_fill_dma_bytes`, `nt_read_serve_bytes`.
Transport:
* `transport_fast_dispatch_serves` / `_demotes`, `fuse3_zc_read_fusions`, `fuse3_zc_fd_body_replies` / `_fallbacks`.
**Must-stay-0 tripwires**: `read_dest_overruns`; `fuse3_zc_fd_body_fallbacks` (≈ 0); `read_settle_lost_serialized` (an `invariant_tripwires` label); `read_admission_evicted_unhit` on fitting sets; `prefetch_evicted_unconsumed` and `read_lane_hold_evicted_unconsumed` on clean streaming rows.
Histograms (exact-sum, per-thread sharded, folded on read):
* `read_serve_phase_ns` — 10 phases (§1.1).
* `read_fill_phase_ns{dev_queue, dev_service, fetch_dma, decode, admission, deposit, fill_total}` (`fuse_client.rs:4151–4169`; lane fetches count into `fill_total`).
Mem budget: the §2.5 components; `mem_budget_tier_publish_paused`.

### 11.3 Constants with WHY and measured numbers

| Constant | Value | WHY (file:line) |
|---|---|---|
| `READ_SIZE_CLASS_BOUNDARY_BYTES` | 256 KiB | the pre-existing no-RAM-LRU population boundary; one stream must not flush warm small entries (`routing.rs:677–681`) |
| `LBA_GRAIN` | 4096 | the conservative O_DIRECT grain, approved OQ #1 (`routing.rs:26`) |
| small-publish boundary | 64 KiB | inline publish vs deferred (`routing.rs:12563, 12580`) — see §13 Q13 |
| hot budget default | `max(2 × 4 MiB, read_mem/4)` | 2 blocks = one stream's consume-behind window (`cache/mod.rs:126–142`) |
| hot min shard | 16 MiB (≥ 4 default blocks) | 1 block/shard ⇒ perpetual churn: 6.1 k refetches/20 s on a fitting set (`cache/mod.rs:166–175`) |
| shard count | `max(next_pow2(cores), 16)` | power-of-two mask; 16 = the shipped small-box posture (`lru.rs:10–16`) |
| `EVICT_CHANNEL_BYTE_BOUND` | 256 MiB | 16384 × 4 MiB of live Bytes was the cage OOM (`lru.rs:44–53, 153`) |
| `EVICTION_QUEUE_RECLAIM_SLACK` / `FIFO_RECLAIM_SLACK` | 64 / 32 | amortization floors below the `2 × live` term (`memory.rs:411–414`; `read_lane.rs:285–288`) |
| `GhostTable::SLOTS` / `EPOCH_MISSES` | 2^16 / 2^15 | ~256 KiB; the two-epoch sliding window (`routing.rs:5998–5999`) |
| `EscalationCooldown::EPOCH_SECS` | 32 | the ~32–64 s re-escalation bound (`:6070`) |
| `AdmissionGovernor::EPOCH_MS` | 2000 | the house 2 s epoch (`:6183`) |
| clamp noise floor | 2 blocks | `wasted ≥ 2 × block` (`:6300`) |
| K lanes | 4 | multi-reader single-file shapes (`:6613–6618`) |
| run threshold | 4 contiguous requests | the earliest classification (block 0's 4th 1 MiB request) (`:7461–7466`) |
| membership tolerance | ±64 × len | a qd-64 reorder span of the stream's own requests; 4 KiB ⇒ ±256 KiB (`:7515–7521`) |
| `FOREIGN_DECLASSIFY_RUN` | 16 | 20–40 GiB/20 s of ungoverned fetches otherwise (`:6632–6644, 7440`) |
| staleness | 2000 ms | every lane/stamp/epoch (`:7456, 7576, 6510`) |
| `R2_WINDOW_START` | 2 | the AIMD start (`:7394`) |
| window cap rails | [4, 4096] | the start must double once; a pathological ratio bound (`:6546–6558`) |
| quiescence span | `2 << min(streak, 5)` | geometric, cap 64 blocks (`:13036–13039`) |
| single-flight `WAIT_SLICE` / `MAX_WAIT` | 50 ms / 60 s | FUSE must not wedge (`:12252–12253`) — §13 Q6 |
| `MAX_REBINDS` / `SETTLE_HANDOFF` / `BACKOFF_AFTER` / `CONTENDED_BEFORE_ESCALATE` | 24 / 8 / 4 / 2 | §7.2 (`:14426–14434`) |
| ranged `MAX_REBINDS` | 8 | then the whole-block loop (`:14938`) |
| `SETTLE_ATTEMPTS` | 4 | the invariant-violation case only (`:14649`) |
| identity loop | 64 | staged identity re-resolves (`:19616`) |
| warm-op yield | 128 | the tokio coop constant, kept because OQ-5 was measured at it (`:19538–19540`) — §13 Q4 |
| `READ_LANE_BUDGET_DIVISOR` | 8 | junior to the write pipeline's /4 (`read_lane.rs:105–109`) |
| hold budget | `clamp(4 × streams × depth × bs, 4 bs, mem/8)` | 2× left half the deposits evicted under FIFO skew (`read_lane.rs:206–226`) |
| hold shrink decay | ⅛ per 2 s | a flapping budget trimmed a 9.7 GiB hold to 1.2 GiB (`read_lane.rs:647–652`) |
| `PROBE_EPOCH_MS` / `PROBE_MUL_ONE` / `PROBE_MUL_MAX` / `PROBE_COOLDOWN_EPOCHS` | 500 / 64 / ×32 / 8 | `write_pipeline_core.rs:120–137` |
| probe gain / adopt / collapse / decay | +¼ / ≥ +1/16 / < 7/8 ⇒ ×0.8 / −⅛ | `:148–165, 243–294` |
| `RANGED_BUF_SIZE` | 64 KiB | THP zeroing under GUP of 4 MiB backings was 40 % of daemon CPU on the miss path (`pool.rs:803–813`) |
| pool capacities | `max(cores × 16, 64)` / `max(cores × 64, 512)` | `pool.rs:313–328` |
| `BLOCK_MAGIC` / `HEADER_SIZE` | `0xCAFEBABE` / 12 | the mmap record frame (`tiering/nvme.rs:11–12`) — §13 Q9 |
| `SHARD_REPLACE_SLACK` | 64 KiB | two key frames + pad + alignment per copy (`cache/nvme.rs:1057–1060`) |
| mem components | §2.5 | `fuse_client.rs:26710–26914` |

Measured numbers to meet or beat: 16.9 GiB tier writes per 16 GiB read eliminated; 1024× rand-4k amplification → 1.0×; 6.3× refetch churn → 1.002; 44 % memcpy share; +28.6 % GB/s and −50 % CPU/byte (lease); −24 % / −25 % CPU/GiB (zc serve); +12.8 % / +14.5 % and `read_amp` 1.41 → 1.11 (hold); 536 k warm / 316–323 k cold / 335 k device-true IOPS (hybrid); 158 of 439 µs (fast dispatch).

---

## 12. Tests (name + one sentence; venue)

Venue legend: **unit** = portable (pure functions, model structs, codecs); **device** = a file-backed data volume (Linux); **mount** = the FUSE mount (Linux venue; the sqz kernel for zc legs).

| Suite / test | Contract | Venue |
|---|---|---|
| `hot_block_tier_tests::probation_evicts_before_protected_and_classes_are_carried` (`:203`) | probation victims leave first and evictions carry their class | device |
| `…::probationary_referenced_fill_survives_one_lap_but_stays_probation_class` (`:240`) | the one-lap grace is not keep-worthiness | device |
| `…::probation_get_promotes_sticky_protected` (`:295`) | a block-level re-access sets the sticky bit | device |
| `…::eviction_channel_carries_victim_classes` (`:359`) | the channel is typed by class | device |
| `…::validated_fills_land_hot_and_nvme_hits_never_repromote` (`:396`) | fill-only landing; the no-repromote rule | device |
| `…::displaced_key_purges_hot_tier_and_reads_serve_new_bytes` (`:469`) | the unified purge on displacement | device |
| `…::stale_hot_entry_under_dead_key_is_never_served` (`:505`) | the incarnation undo | device |
| `…::cache_less_volume_gets_hot_tier` / `hot_budget_zero_short_circuits` (`:550, :579`) | a cache-less format still has a RAM tier; budget 0 disables | device |
| `…::purge_census_grep_guard` / `purge_block_key_unlinks_gds_cache_including_prefixed_keys` / `gds_filename_construction_is_unified` (`:661, :621, :732`) | ONE purge helper, every tier, one name construction | unit (grep) / device |
| `read_tier_admission_tests::second_touch_admission_first_skip_then_publish` (`:231`) | first miss skips the publish; the second within the window publishes | device |
| `…::small_block_volume_keeps_first_touch_publish` (`:287`) | ≤ 256 KiB fills keep today's behavior | device |
| `…::o_direct_flag_reaches_the_classifier` (`:309`) | `fuse_read_in.flags` plumbs | mount |
| `…::two_interleaved_readers_both_classify` (`:338`) | K = 4 lanes classify two readers of one file | device |
| `…::admission_always_restores_first_touch_publish` / `hot_budget_zero_auto_degrades_to_always` / `default_admission_is_second_touch` (`:368, :391, :641`) | knob semantics | device |
| `…::dehydration_gate_drops_untouched_probation_and_dehydrates_protected` (`:428`) | the protected-only gate | device |
| `…::dehydration_skips_tier_resident_protected_victims` (`:513`) | the channel-mouth dedupe | device |
| `…::hold_serve_counts_as_ghost_touch_through_the_fetch_loop` (`:577`) | a ledger-visible hold serve runs the ceremony | device |
| `read_tier_refetch_churn_tests::fetched_blocks_are_tier_visible_and_never_refetched` (`:219`, A–D) | 1 fetch cold; 0 refetch on the second sub-read; 1 fetch for 4 concurrent resolvers; 8 for 8 blocks (`get_obj` deltas) | device |
| `…::result_carrying_single_flight_decouples_waiters_from_publish` (`:369`, E–H) | waiters served from the carried fill under a delayed publish; a late subscriber is a cache re-check; a failing primary fails the cohort promptly with 0 device reads; an aborted primary's waiters recover with exactly one refetch | device |
| `…::tier_publish_is_detached_from_the_serve_but_still_precedes_the_guard_drop` (`:553`) | PERF-11's guard-owns-the-publish ordering | device |
| `read_prefetch_pipeline_tests::pipeline_phases` (`:226`, A–H) | A dedupe/zero waste; B abandonment stops issue within one window; C evict-before-consume halves the window and arms quiescence; D multi-stream contention bounds `get_obj` overshoot; E cap + kill switch; F lane-leak self-repair ≤ 2 epochs; G spawn-shed accounting; H the parked-task screen expresses no verdict | device |
| `read_prefetch_window_tests::window_cap_derives_from_the_hot_budget` / `window_cap_is_railed` / `inflight_depth_is_not_resident_clamped` / `landed_unconsumed_is_resident_bounded` / `plan_is_window_bounded_and_zero_share_never_speculates` / `growth_fires_on_foreground_wait_or_clean_overrun` / `overrun_growth_never_feeds_demonstrated_starvation` / `growth_is_green_gated` (`:49–143`) | the pure window tables | unit |
| `ranged_read_tests::ranged_phases` (`:215`, A–E) | N cold 4 KiB reads = N windows (`get_obj` = N); ghost convergence to a published whole block; threshold 0; compressed volumes never range; the zero-copy-leg alignment matrix | device |
| `…::byte_exactness_odd_offsets_and_eof` (`:466`) | odd offsets/EOF straddles vs a whole-block reference | device |
| `…::raw_dest_leg_refuses_transform_configs` (`:520`) | the sibling-leg hygiene (small-block compressed volume serves decoded bytes) | device |
| `…::rebind_under_movement_never_foreign_bytes` (`:574`) | concurrent overwrite/truncate ⇒ rebind-or-hole, never foreign bytes | device |
| `read_dest_lease_tests::dest_lease_serves_cold_windows_by_dma_and_stands_the_fill_machinery_down` (`:340`) | `lease ≈ served`, `dest → 0`, `fill_dma → 0`, speculative arms ≈ 0; lever off = byte-identical control | device |
| `read_dest_bound_tests::a_serve_longer_than_the_dest_window_is_refused_not_overrun` / `a_covering_window_still_serves_into_the_destination` / `the_window_refuses_exactly_what_exceeds_it` (`:209, :250, :303`) | FUSE-4e: `read_dest_overruns` + the copy leg, canary intact | device |
| `read_lane_tests::depth_derivation_tables` / `probe_governed_depth_mapping_table` (`:72, :164`) | Red ⇒ 0, `cap/bs/streams`; 64→0 … 195→8 | unit |
| `…::engage_governor_probe_cycle_drives_the_default_depth` / `governor_refuses_to_probe_into_ahead_eviction_pressure` (`:192, :276`) | launch/adopt/retreat/cool-down; pressure = zero headroom | unit |
| `…::hold_trim_retains_ahead_entries_and_classes_its_evictions` / `hold_store_unit_semantics` / `hold_fifo_does_not_accumulate_retirement_tombstones` / `hold_fifo_reclamation_keeps_live_nodes_and_oldest_first_trim` (`:311, :625, :720, :749`) | the hold's insert/serve/credit/retire/trim/reclaim laws | unit |
| `…::dest_armed_loop_serves_credit_true_block_coverage` (`:408`) | every serve arm credits its TRUE block coverage | device |
| `…::lane_walk_skips_covered_blocks_without_spending_depth` / `lane_completions_refill_the_pipeline_without_a_touch` (`:493, :557`) | covered skips; completion-driven refill | device |
| `…::lane_engages_on_zero_share_streams_and_stays_ledger_invisible` / `lane_engages_below_the_r2_start_window` / `reordered_qd_stream_keeps_membership_and_lane_engagement` (`:1005, :1132, :1199`) | the R2-declines boundary; qd-reorder membership | device |
| `…::hold_serves_cohort_stragglers_across_hot_eviction_without_refetch` (`:1274`) | the cohort-stability win | device |
| `…::lever_off_is_exact_prior_behavior` / `red_stops_lane_speculation_and_deposits_and_converges` / `overwritten_blocks_never_serve_stale_from_the_hold` (`:1346, :1408, :1448`) | A0 control; Red posture; the purge arm | device |
| `read_admission_governor_tests::governor_phases` (`:214`) | waste → clamp → denials; no cooldown on denial; skew converges | device |
| `…::herd_phase` / `min_shard_constructor_reduces_shards` (`:487, :556`) | the token CAS under a herd; the min-shard halving | device / unit |
| `read_stream_transient_tests::beyond_budget_stream_loop_stays_transient_and_bounds_the_waste_ledger` / `fitting_disk_tier_reread_stream_still_converges` / `stream_admitted_entries_credit_no_within_pass_payback` / `governor_stream_denials_engage_clamp_and_stay_off_the_ranged_instrument` (`:269, :360, :426, :472`) | the transient window's four laws | device |
| `hybrid_io_tests::hybrid_phases` (`:227`, A–F) | O_DIRECT tier serve from RAM; first touch device-true; second touch escalates once; Red pauses; buffered rides the same policy; the stats surface | mount |
| `…::escape_direct_device_true_is_device_true` / `mount_option_is_daemon_level_and_stripped` / `mixed_direct_buffered_coherence_091_shape` / `escape_never_touches_the_write_path` (`:460, :581, :606, :712`) | the ruler's contract | mount |
| `read_zc_pool_serve_tests::armed_fill_pool_is_fd_addressable_both_ways` / `fd_source_serve_elides_the_copy_on_pool_backed_arms_only` / `fast_probe_answers_served_fd_for_pool_backed_hot_blocks` (`:281, :341, :572`) | VA/fd two-way reachability; the copy deleted on pool arms only; the probe's `ServedFd` | device (memfd) |
| `read_copy_ledger_tests::ledger_closes_and_cold_none_dest_slice_is_zero_copy` / `warm_tier_serves_split_out_of_the_dest_bucket` / `warm_arms_and_the_cold_fill_slice_are_attributed_per_arm` (`:306, :563, :713`) | closure; `warm ⊆ dest`; `hot + hold + cache ≡ warm` | device |
| `read_fast_dispatch_tests::warm_active_buffer_read_is_served_byte_exact_and_eof_clamped` / `cold_block_demotes_then_the_tier_resident_block_serves_into_the_dest_window` / `a_writer_holding_the_inode_lock_demotes_in_bounded_time` / `virtual_inodes_demote` / `served_op_records_zero_ingress_and_the_fast_dispatch_trace_chain` (`:216, :251, :345, :381, :396`) | the try-only ladder's contract | device |
| `…::live_armed_session_reads_are_all_fast_dispatched_and_close_against_inplace_replies` (`:646`) | the partition law on an armed session | mount |
| `rebind_starvation_tests::read_survives_stripe_free_displacement_storm_no_rebind_eio` / `read_survives_stripe_holding_promotion_storm_no_rebind_eio` / `fold_seed_survives_stripe_free_displacement_storm_no_rebind_eio` / `a_stale_cached_head_never_wedges_the_settle_into_eio` (`:358, :438, :601, :683`) | exhaustion ⇒ settle, never EIO | device |
| `…::fetch_error_on_current_binding_stays_loud_eio` (`:531`) | a current-binding error stays loud | device |
| `read_saturation_tests::clamped_sequential_ring_stream_classifies_and_rides_whole_blocks` / `sequential_ring_stream_engages_the_prefetch_pipeline` / `fully_warm_sequential_ring_rereads_stay_laneless` / `buffered_ring_stream_classifies_exactly_once` (`:568, :645, :699, :763`) | the ring feed; `lane_pre_fed`; warm serves never mint lanes | device |
| `read_serve_phase_tests::phase_families_are_always_on_with_exact_keys` / `cold_striped_read_records_the_full_serve_chain` / `cohort_waiters_record_sf_wait` / `recording_lands_in_exactly_the_named_phase` (`:254–439`) | always-on exact-sum families | device |
| `mw_block_key_incarnation_tests::the_incarnation_codec_is_canonical_and_refuses_junk` et al. (`:227–386`) | the key's incarnation decoration | unit |
| Model checks (port): `incarnation_core` retire/publish/snapshot/validate with its WEAKENING (drop `retire`'s Release fence or `still`'s Acquire fence); `ProbeCore` single-roll + bound with a non-CAS roll as the weakening (`incarnation_core.rs:12–14`; `write_pipeline_core.rs:166–170`) | `src/model/explorer.zig` `expectHolds` / `expectFails` | unit |

---

## 13. QUIRKS per decision 0002 — and the better design

| # | Reference quirk | Why it is poor | What the port does |
|---|---|---|---|
| Q1 | The READ handler calls `backend.getattr().await` INSIDE the per-inode read guard on an attr-cache miss (`fuse_client.rs:33876–33899`) | Ledger row 84: backend I/O under lock-order level (1) | Resolve the size BEFORE the guard; the short shared section covers only the active-buffer hit (row 84) |
| Q2 | Five block-key tiers including a GDS `.gds_cache` FILE tier purged by `unlink(2)` inside the one purge helper (`cache/mod.rs:382–390`); the "fourth tier" existed because two filename constructions diverged | A syscall inside a hot-path purge for a feature (GDS) rung 4 does not port | The ONE purge covers exactly the tiers that exist (hot, read LRU, hold, NVMe read cache); a static rail in `tests/rails.zig` asserts every block-key store registers a purge arm — the grep-guard made structural |
| Q3 | `parking_lot::RwLock` + `spawn_blocking` for the read-cache shard, with `read_recursive` everywhere to dodge the writer-preferring park (`tiering/nvme.rs:310–334`); a `parking_lot::Mutex` eviction latch per RAM shard (`memory.rs:79`) | Charter §1.3 bans blocking locks on the data hot path; the Hang-1 wedge was a symptom of that lock class | Read-cache index: an arc-swap-style immutable index snapshot + per-entry pin counts (the reference's own pre-agreed fallback, `tiering/nvme.rs:289–292`); the segment write on the io_uring file layer; the clock latch is a `try`-only atomic flag |
| Q4 | A thread-local 128-op counter yielding warm reads (`routing.rs:19545–19555`) | A tokio coop artifact; the port's lanes are fibers with explicit yield points | The lane scheduler's fairness law (`exec`): a fiber completing N ops without parking yields to the run queue — one rule in the executor, no per-module counter |
| Q5 | Knob reads outside the registry: `std::env::var("SQUEEZEFS_READ_HOT_BLOCK_CACHE_MB")` / `…_FILL_PCT` parsed by hand (`cache/mod.rs:132–163`); cache sizes parsed as `f64` (`parse_size_string`, `:395–453`) | Two parsers (row 14's class); float semantics for byte counts | Registry rows through `knobs.intKnob` with the ONE parser; sizes are integer bytes or an integer percentage |
| Q6 | A 60 s `MAX_WAIT` + 50 ms slices in the single-flight loop answering `TimedOut` (`routing.rs:12252–12253, 12354–12359`); a per-read device timeout (`nvme_dev.rs:2798–2801`) | Per-op timeout wrappers are banned (charter §4; row 34); a device that stops answering is a VOLUME event | Waiters park on the flight's completion cell unbounded; the lost-wake class is the primitive's 2 s ticked backstop (charter §9); the watchdog + `overdueCensus` name a wedged device |
| Q7 | Prefetch/lane tasks are heap `spawn_bg` futures per block with an `scc::HashSet` parked-task screen (`routing.rs:13369`, `:6473`) | Per-op allocation (charter §12); the screen exists only to tell "issued" from "fetched" | Fills are fiber slots from the lane's preallocated table; a lane-local issued-span bitmap (`[issued_base, next)` ∧ a per-block `first_polled` bit) replaces the set |
| Q8 | `sqz_flight` + `broadcast` (capacity 64 → 4 → a shared oneshot) with `Lagged`/`Closed` arms the waiters loop on (`:12426–12431`) | Three generations of one primitive; the loop exists because the channel could lag | ONE `sync/completion` cell per flight storing the terminal `FillResult`; a late subscriber reads the stored value — no lag, no loop |
| Q9 | Read-cache record frame `{magic 0xCAFEBABE LE, key_len u32, val_len u32}` with no checksum, a 4 KiB key pad, tombstone by zeroing 4 magic bytes + `msync(MS_ASYNC)` (`tiering/nvme.rs:11–12, 633–676`) | An on-media frame without a checksum (charter §12); an integer magic (row 38); the cache is wiped at mount anyway, so `msync` buys nothing | Frame = `extern struct {magic "SQZRCACH", key_len u32, val_len u32, incarnation u64, xxh3 u64}`; no `msync` (a read cache owes no durability) |
| Q10 | `memcpy` into the mmap tier — the publish IS the 16.9 GiB writeback tax; the publish runs on the blocking pool | The design kills the tax by admission but keeps a 4 MiB CPU memcpy for every admitted block | Admitted publishes are ONE `WRITE_FIXED` from the fill-pool slab to the cache segment on the io_uring file layer (rung 1), never a CPU pass; serves read through the registered path or an `mmap` of the cache file (A/B at rung 4) |
| Q11 | `SystemTime::now()` wall-clock milliseconds for every lane/epoch stamp (`routing.rs:7442–7447`, `:6080–6082`) | A wall clock steps; a 2 s epoch across an NTP step misfires | `core/clock.monotonicNs` for every staleness/epoch stamp |
| Q12 | The hot-tier `get` bumps three atomics and clones the `Bytes` (a refcount RMW on the payload's shared line) per hit (`memory.rs:115–144`) | A shared-line RMW per op on the hot path (charter §10) | Entries are refcounted slots in the fill-pool slab; serve pins are per-thread epoch guards; `referenced`/`protected` are bytes in a per-shard side array; `served_bytes` is a `ShardedCounter` kept only for protected entries |
| Q13 | A "mirrored" constant pair (`READ_LANE_MIN_FILL_BYTES = READ_SIZE_CLASS_BOUNDARY_BYTES`, `read_lane.rs:111–116`) and a 64 KiB inline-publish boundary with no WHY (`routing.rs:12563, 12580`) | A free-floating boundary (charter §1.5) | ONE `read_size_class` module: 256 KiB = the read-LRU population bound (tie test), 64 KiB = `RANGED_BUF_SIZE` (inline-publishable ⇔ fits the ranged bounce slab — the WHY on the line) |
| Q14 | `ZcReadServe` carries a `Box<dyn Fn>` injected fetch for tests (`routing.rs:6859–6865`) | A boxed closure in a hot-path handle | The transport `Tx` vtable already has `replyPrefilled`; the direct-leg fetch is a vtable entry on the same face (the kmbuf/zc wave adds it); tests pass a fake `Tx` |
| Q15 | `is_damaged_mapping` checked at the fill site AND the lane fetch AND the device-true fetch (`:12243, 14079, 14250`) | Three copies of one refusal | ONE refusal at `read_block_range` / `read_block_with_dest` (the device leg already refuses dead-lifetime keys there) |
| Q16 | `SQUEEZEFS_READ_MOSTLY_CACHE` wrapping moka for the metadata/stream-lane maps (`env_knobs.rs:227`) | A knob over a container choice | The port's concurrent map is one type; no lever |
| Q17 | The `READ_BUDGET`/`WAIT_SLICE`/`MAX_WAIT`/`SETTLE_ATTEMPTS` sleeps are `sqz_time::sleep` on a global timer thread (`:12384, 14494, 14656`) | Row 63: a cross-thread wake hop per arm | Lane-local timer heaps (`exec/timer_heap.zig`) for the backoff tails |

Phase recording (`read_serve_phase_record(phase, t0) -> Instant` returning the clock read so the next phase abuts, `fuse_client.rs:4360–4373`) is correct and kept: `op_trace.OpTrace` span form (row 87), exact-sum asserted.

Charter-level items the port must NOT soften (laws, not quirks): precedence (overlays first); the binding-validated serve proof with the recheck AFTER bytes in hand; the insert-time `validate`; ONE purge; ranged never published / never single-flighted; the lease's ghost exemption; the hold's ledger-visible ceremony; the governor's eviction-side ratio and token CAS; the per-block consume drain; the parked-task "no verdict" rule; the closed-loop ahead depth (the two falsified derivations stay falsified); the Red/Yellow responses; the read copy ledger closure.
