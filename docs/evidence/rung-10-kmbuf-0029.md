# Rung 10 — the kmbuf region's short bulk allocation: kernel patch 0029 (2026-10-15)

**Defect.** The reference series' patch 0003 (kernel-managed buffer rings,
`io_region_allocate_pages_multi_buf`) backs each buffer with one compound block of
`get_order(buf_size)`, else with `alloc_pages_bulk_node`, and refuses the registration (ENOMEM)
on any short count. The bulk allocator does not reclaim. It batches only above a zone's low
watermark plus the request, otherwise it returns ONE page. Under memcg kmem accounting (any host
with a memory cgroup, kamrui included), the region's `GFP_KERNEL_ACCOUNT` always gets that one
page. So a region registers only if every 1 MiB buffer gets an order-8 block. A costly-order
`GFP_KERNEL` request gets one round of reclaim plus async compaction, which for an unmovable
request scans only compatible pageblocks. Seen as `KmbufRingEnomem` at mount: 1 of 60 rejoin
runs, every venue mount while LTO links held 13 of 16 GB, and rung 6's 3 of 11.

**Patch.** `scripts/kernel/patches-7.2/0029-io_uring-kbuf-finish-a-short-bulk-allocation-with-reclaiming-pages.patch`,
one hunk in `io_uring/memmap.c` (+16). A short buffer is finished with `alloc_page(gfp)`: the
same gfp, which may direct-reclaim, as `vm_area_alloc_pages` does. ENOMEM comes only when one
page cannot be had or a fatal signal is pending; the failure path still frees every page. On
kamrui (`/build/kernel-0029/`): a fresh linux-7.2.6 + 0001–0029 applies with `patch -p1
--fuzz=0`, `io_uring/memmap.o` builds with W=1 and no warnings, and checkpatch flags only the
Signed-off-by (left for the owner). `build-venue-kernel` picks it up: it copies
`patches-7.2/*.patch` into the reference series, which `build-kernel.sh` applies in name order
with `--fuzz=0`. Decision 0002 row 540.

**Repro, before** (kamrui, 7.2.6-sqz `#1 … Thu Oct 15 02:43:54 UTC`, no 0029, run under a venue
slot by `/build/kernel-0029/repro.sh`, which is outside the tree). Each mount ran `squeezefs mount
--daemon` with `SQUEEZEFS_FUSE_OVER_IO_URING_Q_DEPTH=32` and `SQUEEZEFS_FUSE_MAX_WRITE=1048576`:
zero_copy, 12 queues × 32 × 1 MiB. Page cache came from reading a 4 TiB sparse file's holes.

| page-cache churn | mounts at once | refused / mounts | MemFree (min while mounting) | Cached |
|---|---|---|---|---|
| sequential reads (ext4 large folios), ± 4 streamers, ± a 44/50 GiB anon hog | 1 | 0 / 23 | 0.37–20 GiB | 3–53 GiB |
| sequential reads, 4 streamers (± a 44 GiB hog at 8) | 6, 8, 12 | 0 / 78 (up to 6 kernel ENOMEMs per mount, absorbed by the retry ladder) | ≥ 372 MiB | 6–54 GiB |
| fio buffered 4 KiB random reads (`fadvise_hint`: order-0 folios), 6 jobs | 1 | **10 / 10** | 345–472 MiB | 53–54 GiB |
| the same | 12 | 25 / 36 | 377 MiB (trial 3; trials 1–2's lines were cut) | 51 GiB |
| the same + a 40 GiB anon hog | 8 | 16 / 24 (0 / 8 while the hog populated) | 397–452 MiB | 16.5–16.9 GiB |

Every refusal is the typed one after five retries over 1.2 s: `kmbuf ring registration ENOMEM
(8192 pages at once: 32 buffers × 1048576 B at bgid 0, opcode 38) after 5 retries`. Reclaiming
large folios hands order-8 blocks straight back to the buddy allocator; reclaiming order-0 page
cache leaves scattered pages. After each single-mount refusal, ZONE_NORMAL held 2–4 MiB of free
order ≥ 8 blocks (ZONE_DMA32's ~196 MiB sits below its lowmem reserve). A second 12-mount random
run lost its pressure partway, when another process freed ~48 GiB of page cache, and refused 0
of 24.

**Built, not installed.** Run with `scripts/kernel/build-host-kernel kamrui-dev --no-install`.
The new flag stops after `bindeb-pkg` and prints the `dpkg -i` line. The make now holds the
venue's gate lock and every slot, as a full gate does, and the launch no longer waits out the
build over ssh. Packages (built 13:16–13:33 UTC; they reuse the names of the packages kamrui
runs, so check the hashes): `/build/kernel/linux-image-7.2.6-sqz_7.2.6-1_amd64.deb` (sha256
`d5e9579e302e…`) and `/build/kernel/linux-headers-7.2.6-sqz_7.2.6-1_amd64.deb` (`d2c77d8f0b34…`).

**Installed (2026-10-15).** kamrui: both packages (hashes checked) installed and rebooted at
13:44 UTC under the gate lock and every slot; `uname -v` = `Thu Oct 15 13:29:48 UTC 2026`. VM:
`build-venue-kernel` (Image sha256 `1e7a39a903a7…`, build `13:42:41 UTC`), boot-tested with
`container run --rm -k`, set as the default, `sqz-zig` recreated.

**After, on kamrui's new kernel** (same binary, same repro): `RAND=6 PAR=1 TRIALS=10` — **0 of 10**
refused (before: 10 of 10), at MemFree 277–591 MiB, 55.8 GiB cached and 0 MiB free at order ≥ 8 in
ZONE_NORMAL, so every buffer's compound attempt failed and the page-by-page finish carried it.
`RAND=6 PAR=12 TRIALS=3` — 0 of 36 refused; only trial 1 ran under pressure (min MemFree 561 MiB,
12 of 12 mounted), trials 2–3 started with the page cache drained (23–40 GiB free).
