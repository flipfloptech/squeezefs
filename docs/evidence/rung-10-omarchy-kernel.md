# Rung 10 — the venue's default kernel: an Omarchy-flavoured kernel.org 7.3.1 (2026-11-02; decision 0003 §DD)

The owner's rulings (2026-11-02): none of OUR kernel patches, ever; the DEFAULT venue kernel is kernel.org
7.3.1 + Omarchy's patches that apply to 7.3.1 + BORE's own 7.3 patch + Omarchy's AMD/VESA display fixes
ported to 7.3.1 + the venue's config fragment, Omarchy's config as the seed; the pure stock 7.3.1
(`linux-mainline-sqz`, §DB) stays as the SECOND boot entry; skip Omarchy's backport bundles,
Intel-only/other-laptop quirks, the mm compaction series (8205–8207, 8220), ksm 0270 and
sched-always-inline 0140. Branch `chore/omarchy-73-kernel` on `chore/stock-kernel` `355b8ec`.

**The build.** `scripts/kernel/build-host-kernel --local [--flavour omarchy|stock|both]` (default both,
omarchy first; each holds the gate lock and every build slot while it compiles): pkgbase
`linux-omarchy-mainline`, `uname -r` `7.3.1-1-omarchy-mainline` (Omarchy's name space, `-mainline` for
kernel.org's tarball; no Omarchy package has it). The recipe (PKGBUILD functions, `config.x86_64`, keys)
and the Omarchy patches come from omacom/omarchy-pkgs at the commit pinned in
`scripts/kernel/omarchy-7.3/series` (99aa5aa, pkgver 7.2.15), each patch checked by its b2sum in the
series; BORE and the ports are vendored there. Limine's drop-in: `linux-omarchy-mainline,
linux-mainline-sqz, linux-t2, linux-omarchy, linux-omarchy-*, *, *fallback, Snapshots`.

**The series: 91 patches = 82 Omarchy + BORE + 8 ports** (`scripts/kernel/omarchy-7.3/README.md` has all
188 rows). A fuzz-0 dry run of Omarchy's 188 in order on 7.3.1: 86 apply, 49 already in (each
reverse-applies on the final tree), 53 conflict.

| outcome | patches |
|---|---|
| taken as they are | 82 of the 86 (8204 with its xe readers 8209/8210: the hint changes nothing without an xe device) |
| dropped: need a skipped patch | 0403, 0404, 0406 (use `quirk_panel_replay_exit_on_flush` of 0402 — compile error); 0405 (its context is 0403/0404's — no longer applies at `--fuzz=0`); all Intel/Dell XPS quirks |
| replaced | 0110 BORE (= BORE's linux 7.2-rc1 patch) by firelzrd/bore-scheduler `1e2f57ad74e8` `0001-linux7.3-rc2-bore-7.0.0.patch`, unchanged: it applies at `--fuzz=0` (7.3.1's 40 commits touch no kernel/sched; CachyOS kernel-patches#193's `update_protect_slice()` break is CachyOS's base, not kernel.org's) |
| ported (vendored, each with a `ported to 7.3.1:` note) | 0350 (7.3 rewrote `update_displayid_info()`'s loop — it already walks every block; only the Adaptive-Sync parse added); 0450, 0451, 0453, 0460, 0461, 0478, 0479 (7.3 split `amdgpu_dm.c`: the hunks move unchanged to `amdgpu_dm_freesync.c` / `amdgpu_dm_connector.c`; 0460's `drm_displayid_internal.h` hunk re-placed around 7.3's new Display Parameters block; 0450's redundant include dropped) |
| dropped: upstream in another form | 0454 (FRL polling test identical in 7.3), 0472 (userq post-reset error identical), 0474 (VESA AUX backlight identical in `amdgpu_dm_backlight.c`), 0476 (7.3 picks the encoding in `amdgpu_dm_create_validate_stream_for_sink()`: a 420-only mode gets YCbCr420 in AUTO) |
| skipped (ruling) | 16 backport bundles, 9 Intel/other-laptop, 7 xe shrinker, 8205–8207/8220, 0270, 0140, and 0121 smp-preempt, 0360 gpu-mem-cgroup (conflicts outside the port list) |

**Checks.** The config: Omarchy's `config.x86_64` + olddefconfig + the fragment + olddefconfig, checklist OK;
against the stock flavour's final `.config` it differs only by the patches' own options
(`CC_OPTIMIZE_FOR_PERFORMANCE_O3`, `SCHED_BORE`, `MIN_BASE_SLICE_NS=2000000`, `MQ_IOSCHED_ADIOS=m`);
`CONFIG_HZ=1000`. A verification tree rebuilt from the series file with `patch --fuzz=0` equals the
`--prepare-only` tree (`PREPARE-OK`, 91 patches); every configured object the series touches (107)
compiles, and W=1 on the ported, BORE and sched objects shows 0 warnings. The full build: see §DD.6.

**Owed after the reboot (owner):** boot; `sysctl kernel.sched_bore` = 1; s2idle suspend/resume; the
display fixes on an external HDMI 2.1 (VRR/ALLM) and an OLED/DisplayID panel; the gate train on the new
kernel.
