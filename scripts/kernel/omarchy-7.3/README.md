# The omarchy flavour's patch series (linux 7.3.1)

`scripts/kernel/build-host-kernel --flavour omarchy` builds `linux-omarchy-mainline`, the venue's
default kernel (owner ruling 2026-11-02; decision 0003 §DD,
`docs/evidence/rung-10-omarchy-kernel.md`): kernel.org linux 7.3.1, Omarchy's patches that apply
to it, BORE's own 7.3 patch, Omarchy's AMD/VESA display fixes ported to 7.3.1, and the venue's
`scripts/kernel/config-fragment`, with Omarchy's `config.x86_64` as the seed. None of the patches
is ours; the ported ones are Omarchy's, moved to where 7.3 keeps the code.

`series` lists them in apply order (Omarchy's `prepare()` applies them at `--fuzz=0`):
`omarchy` rows are fetched from omacom/omarchy-pkgs at the pinned commit (99aa5aa, pkgver 7.2.15,
188 patches) and checked by the b2sum on the row; `vendored` rows are the files here.

| | count |
|---|---|
| Omarchy patches at 99aa5aa | 188 |
| applied as they are (fetched) | 82 |
| already in 7.3.1 (reverse-apply at `--fuzz=0`), dropped | 49 |
| apply, but depend on a skipped patch (0403–0406, i915 quirks), dropped | 4 |
| conflict: BORE 0110, replaced by BORE's linux 7.3 patch (vendored) | 1 |
| conflict: display fixes ported to 7.3.1 (vendored) | 8 |
| conflict: display fixes already upstream in another form, dropped | 4 |
| conflict: skipped by the owner's ruling (backport bundles, Intel/other-laptop, mm compaction 8205–8207/8220, ksm 0270, sched-always-inline 0140, smp-preempt 0121, gpu-mem-cgroup 0360) | 40 |

The series is 82 + 9 = 91 patches. BORE: firelzrd/bore-scheduler `1e2f57ad74e8`,
`patches/stable/0001-linux7.3-rc2-bore-7.0.0.patch`, unchanged — it applies to 7.3.1 at
`--fuzz=0` (7.3.1 changes nothing in kernel/sched; the `update_protect_slice()` build break in
CachyOS kernel-patches#193 is in CachyOS's base, not kernel.org's). Each ported patch carries
Omarchy's change and a `ported to 7.3.1:` paragraph saying what moved. 8204 (the opportunistic
compaction hint) is taken with the xe shrinker patches that read it (8209, 8210); without an xe
device nothing reads the hint.

## Refreshing after an Omarchy bump

1. Clone omacom/omarchy-pkgs at the new commit; on a git tree of the kernel.org release, apply its
   `pkgbuilds/linux-omarchy` patches in `source=()` order at `--fuzz=0`, classifying each as
   applies / already (reverse-applies) / conflicts.
2. Rebuild the series: the applying ones as `omarchy` rows (`b2sum`), minus the ones that need a
   skipped patch (compile every object the series touches: `make O=… <dir>/<file>.o`; amdgpu
   display objects as `drivers/gpu/drm/amd/amdgpu/../display/…`); BORE's patch for the release;
   then each display fix: if Omarchy's now applies, make it an `omarchy` row and delete the port
   here; if upstream carries it, drop it; else re-port it (`git format-patch --zero-commit
   --no-signature`) with its `ported to` note.
3. Update the `omarchy-pkgs` (and on a kernel bump the `kernel`) line of `series`, this table,
   then `scripts/kernel/build-host-kernel --local --flavour omarchy --prepare-only` (every patch at
   `--fuzz=0`, the checklist) and a `--no-install` build.

## Every Omarchy patch at 99aa5aa

| patch | source | status | why |
|---|---|---|---|
| 0010-archlinux-base.patch | omarchy | applied |  |
| 0011-kbuild-optimize-for-performance-o3.patch | omarchy | applied |  |
| 0110-bore-7.0.0.patch | BORE | replaced | by BORE's own linux 7.3 patch, `0110-linux7.3-rc2-bore-7.0.0.patch` (Omarchy's is BORE's linux 7.2-rc1 patch) |
| 0120-tlbpull.patch | omarchy | dropped | already in 7.3.1 (reverse-applies at --fuzz=0) |
| 0121-smp-preempt.patch | omarchy | skipped | conflicts; not in the owner's port list (ruling: skip the rest) |
| 0130-sched-detach-tasks.patch | omarchy | applied |  |
| 0140-sched-always-inline.patch | omarchy | skipped | conflicts; sched-always-inline (owner ruling) |
| 0143-sched-hybrid-cluster-balancing.patch | omarchy | dropped | already in 7.3.1 (reverse-applies at --fuzz=0) |
| 0144-sched-nohz-idle-core.patch | omarchy | dropped | already in 7.3.1 (reverse-applies at --fuzz=0) |
| 0147-sched-cache-asym-packing-over-llc-balance.patch | omarchy | applied |  |
| 0150-adios-3.2.0.patch | omarchy | applied |  |
| 0210-pm-updates.patch | omarchy | skipped | conflicts; a backport bundle, ~95–100 % upstream in 7.3.1 (owner ruling 2026-11-02) |
| 0211-pm-fixes-1.patch | omarchy | dropped | already in 7.3.1 (reverse-applies at --fuzz=0) |
| 0213-pm-fixes-3.patch | omarchy | dropped | already in 7.3.1 (reverse-applies at --fuzz=0) |
| 0220-x86-amd-zen5-tlb-sizes.patch | omarchy | applied |  |
| 0250-zsmalloc.patch | omarchy | dropped | already in 7.3.1 (reverse-applies at --fuzz=0) |
| 0260-mglru-exec-protect.patch | omarchy | dropped | already in 7.3.1 (reverse-applies at --fuzz=0) |
| 0270-ksm-rmap-walk.patch | omarchy | skipped | conflicts; ksm (owner ruling) |
| 0280-mm-updates.patch | omarchy | skipped | conflicts; a backport bundle, ~95–100 % upstream in 7.3.1 (owner ruling 2026-11-02) |
| 0290-zstd-bmi2-fallback-aliases.patch | omarchy | applied |  |
| 0291-zstd-bmi2-cpu-feature-dispatch.patch | omarchy | applied |  |
| 0292-crypto-zstd-defer-cstream-init.patch | omarchy | applied |  |
| 0293-crypto-zstd-defer-dstream-init.patch | omarchy | applied |  |
| 0295-af-alg-restrict.patch | omarchy | dropped | already in 7.3.1 (reverse-applies at --fuzz=0) |
| 0296-efi-secure-boot-lockdown.patch | omarchy | applied |  |
| 0297-module-sig-platform-keyring.patch | omarchy | applied |  |
| 0300-btrfs.patch | omarchy | skipped | conflicts; a backport bundle, ~95–100 % upstream in 7.3.1 (owner ruling 2026-11-02) |
| 0301-btrfs-fixes-1.patch | omarchy | dropped | already in 7.3.1 (reverse-applies at --fuzz=0) |
| 0302-btrfs-zstd-decompress-direct-to-page.patch | omarchy | applied |  |
| 0303-btrfs-fixes-2.patch | omarchy | dropped | already in 7.3.1 (reverse-applies at --fuzz=0) |
| 0304-btrfs-fixes-3.patch | omarchy | dropped | already in 7.3.1 (reverse-applies at --fuzz=0) |
| 0305-btrfs-fixes-4.patch | omarchy | dropped | already in 7.3.1 (reverse-applies at --fuzz=0) |
| 0310-fuse-eof-zeroing.patch | omarchy | dropped | already in 7.3.1 (reverse-applies at --fuzz=0) |
| 0311-fuse-perf.patch | omarchy | dropped | already in 7.3.1 (reverse-applies at --fuzz=0) |
| 0312-fuse-writethrough-uptodate.patch | omarchy | dropped | already in 7.3.1 (reverse-applies at --fuzz=0) |
| 0313-fuse-background-wakeup.patch | omarchy | dropped | already in 7.3.1 (reverse-applies at --fuzz=0) |
| 0320-vfs-open-consume-path-reference.patch | omarchy | applied |  |
| 0350-drm-edid-populate-monitor-range-from-displayid-adaptive-sync.patch | vendored | ported | `0350-drm-edid-populate-monitor-range-from-displayid-adaptive-sync-7.3.1.patch`; 7.3 rewrote update_displayid_info()'s loop |
| 0360-gpu-mem-cgroup.patch | omarchy | skipped | conflicts; not in the owner's port list (ruling: skip the rest) |
| 0400-drm-i915-alpm-limit-pr-alpm-to-panel-replay.patch | omarchy | applied |  |
| 0401-psr2-early-transport-panels.patch | omarchy | applied |  |
| 0402-drm-i915-quirk-panel-replay-exit-on-flush.patch | omarchy | skipped | conflicts; Intel-only / other-laptop (i915 panel-replay quirk) (owner ruling) |
| 0403-drm-i915-quirk-dell-xps14-ptl-panel-replay-exit-on-flush.patch | omarchy | dropped | applies, but needs quirk_panel_replay_exit_on_flush from 0402 (skipped): does not compile |
| 0404-drm-i915-quirk-dell-xps16-ptl-panel-replay-exit-on-flush.patch | omarchy | dropped | applies, but needs quirk_panel_replay_exit_on_flush from 0402 (skipped): does not compile |
| 0405-drm-i915-quirk-dell-xps13-dx13260-ptl-psr1-only.patch | omarchy | dropped | its context is 0403/0404's: no longer applies at --fuzz=0 (an Intel/other-laptop quirk) |
| 0406-drm-i915-quirk-dell-xps13-dx13260-wcl-panel-replay-exit-on-flush.patch | omarchy | dropped | applies, but needs quirk_panel_replay_exit_on_flush from 0402 (skipped): does not compile |
| 0411-drm-xe-display-no-stolen-framebuffers.patch | omarchy | dropped | already in 7.3.1 (reverse-applies at --fuzz=0) |
| 0420-safe-window.patch | omarchy | dropped | already in 7.3.1 (reverse-applies at --fuzz=0) |
| 0430-fbc.patch | omarchy | dropped | already in 7.3.1 (reverse-applies at --fuzz=0) |
| 0440-xe3-peak-bandwidth.patch | omarchy | skipped | conflicts; Intel-only / other-laptop (xe3 bandwidth) (owner ruling) |
| 0443-drm-i915-dsb-delayed-vblank-scanline-shift.patch | omarchy | dropped | already in 7.3.1 (reverse-applies at --fuzz=0) |
| 0450-amd-hdmi-vrr-allm.patch | vendored | ported | `0450-amd-hdmi-vrr-allm-7.3.1.patch`; 7.3 split amdgpu_dm.c / reworked the code (the patch says where) |
| 0451-amd-vtem-tmds-links.patch | vendored | ported | `0451-amd-vtem-tmds-links-7.3.1.patch`; 7.3 split amdgpu_dm.c / reworked the code (the patch says where) |
| 0452-amd-hdmi-frl-default.patch | omarchy | applied |  |
| 0453-amd-vtem-null-sink.patch | vendored | ported | `0453-amd-vtem-null-sink-7.3.1.patch`; 7.3 split amdgpu_dm.c / reworked the code (the patch says where) |
| 0454-amd-hdmi-frl-status-polling.patch | omarchy | dropped | already upstream: 7.3's hdmi_frl_status_polling_work() (amdgpu_dm_connector.c) has the same frl_link_rate test |
| 0455-amd-hdmi-frl-cap-on-nondestructive-verify.patch | omarchy | applied |  |
| 0456-amd-dml-frl-compressed-cap-check.patch | omarchy | dropped | already in 7.3.1 (reverse-applies at --fuzz=0) |
| 0460-vesa-displayid-dsc-bpp.patch | vendored | ported | `0460-vesa-displayid-dsc-bpp-7.3.1.patch`; 7.3 split amdgpu_dm.c / reworked the code (the patch says where) |
| 0461-vesa-dsc-passthru-mode-match-fix.patch | vendored | ported | `0461-vesa-dsc-passthru-mode-match-fix-7.3.1.patch`; 7.3 split amdgpu_dm.c / reworked the code (the patch says where) |
| 0472-amdgpu-userq-post-reset-error.patch | omarchy | dropped | already upstream: 7.3.1's amdgpu_userq_post_reset() has the same ret/r fix (plus trace calls) |
| 0474-amd-display-oled-vesa-backlight.patch | omarchy | dropped | already upstream: 7.3.1's amdgpu_dm_update_connector_ext_caps() (amdgpu_dm_backlight.c) selects VESA AUX the same way |
| 0475-revert-drm-i915-dp-as-sdp-vrr-or-pr.patch | omarchy | applied |  |
| 0476-amd-display-ycbcr420-only-modes.patch | omarchy | dropped | superseded upstream: 7.3 picks the encoding in amdgpu_dm_create_validate_stream_for_sink(); a 420-only mode gets YCbCr420 in AUTO; force_yuv420_output is gone |
| 0478-amd-freesync-vcp-code-check-hdmi-pcon.patch | vendored | ported | `0478-amd-freesync-vcp-code-check-hdmi-pcon-7.3.1.patch`; 7.3 split amdgpu_dm.c / reworked the code (the patch says where) |
| 0479-amd-hdmi-allm-hf-vsif-in-vsp-infopacket.patch | vendored | ported | `0479-amd-hdmi-allm-hf-vsif-in-vsp-infopacket-7.3.1.patch`; 7.3 split amdgpu_dm.c / reworked the code (the patch says where) |
| 0481-amd-display-dcn31-compbuf-wait.patch | omarchy | dropped | already in 7.3.1 (reverse-applies at --fuzz=0) |
| 0510-sound-updates.patch | omarchy | skipped | conflicts; a backport bundle, ~95–100 % upstream in 7.3.1 (owner ruling 2026-11-02) |
| 0511-asoc-updates.patch | omarchy | skipped | conflicts; a backport bundle, ~95–100 % upstream in 7.3.1 (owner ruling 2026-11-02) |
| 0512-sound-fixes-1.patch | omarchy | skipped | conflicts; a backport bundle, ~95–100 % upstream in 7.3.1 (owner ruling 2026-11-02) |
| 0513-asoc-fixes-1.patch | omarchy | skipped | conflicts; a backport bundle, ~95–100 % upstream in 7.3.1 (owner ruling 2026-11-02) |
| 0514-sound-fixes-2.patch | omarchy | skipped | conflicts; a backport bundle, ~95–100 % upstream in 7.3.1 (owner ruling 2026-11-02) |
| 0515-soundwire-updates.patch | omarchy | dropped | already in 7.3.1 (reverse-applies at --fuzz=0) |
| 0516-rt766-stream-config-type.patch | omarchy | dropped | already in 7.3.1 (reverse-applies at --fuzz=0) |
| 0517-hda-realtek-rog-strix-g733zw-speakers.patch | omarchy | skipped | conflicts; Intel-only / other-laptop (ROG Strix speakers) (owner ruling) |
| 0518-asoc-amd-yc-acer-aspire-a314-23p.patch | omarchy | applied |  |
| 0520-hda-cs420x-macbookair7-2-quirk-fix.patch | omarchy | applied |  |
| 0521-xps13-sof-quirk.patch | omarchy | applied |  |
| 0522-sound-fixes-3.patch | omarchy | skipped | conflicts; a backport bundle, ~95–100 % upstream in 7.3.1 (owner ruling 2026-11-02) |
| 0523-asoc-fixes-2.patch | omarchy | dropped | already in 7.3.1 (reverse-applies at --fuzz=0) |
| 0524-sound-fixes-4.patch | omarchy | dropped | already in 7.3.1 (reverse-applies at --fuzz=0) |
| 0525-asoc-fixes-3.patch | omarchy | dropped | already in 7.3.1 (reverse-applies at --fuzz=0) |
| 0526-sof-amd-acp7x-suspend-unknown-revision.patch | omarchy | applied |  |
| 0527-sound-fixes-5.patch | omarchy | skipped | conflicts; a backport bundle, ~95–100 % upstream in 7.3.1 (owner ruling 2026-11-02) |
| 0528-asoc-fixes-4.patch | omarchy | dropped | already in 7.3.1 (reverse-applies at --fuzz=0) |
| 0529-sound-fixes-6.patch | omarchy | dropped | already in 7.3.1 (reverse-applies at --fuzz=0) |
| 0530-asoc-fixes-5.patch | omarchy | dropped | already in 7.3.1 (reverse-applies at --fuzz=0) |
| 0531-spi-cs42l43-dell-dx13260-ptl-speaker-id-quirk.patch | omarchy | applied |  |
| 0532-usb-audio-behringer-umc204hd-skip-boot-quirk.patch | omarchy | applied |  |
| 0541-cvs-nova-lake-acpi-id.patch | omarchy | dropped | already in 7.3.1 (reverse-applies at --fuzz=0) |
| 0542-media-cvs-wake-irq-without-claiming-gpio.patch | omarchy | applied |  |
| 0543-media-laptop-cameras.patch | omarchy | skipped | conflicts; Intel-only / other-laptop (Intel IPU laptop cameras) (owner ruling) |
| 0544-ipu-bridge-lenovo-x9-upside-down.patch | omarchy | skipped | conflicts; Intel-only / other-laptop (Intel IPU, Lenovo X9) (owner ruling) |
| 0545-ipu-bridge-dell-16-premium-da16250-upside-down.patch | omarchy | applied |  |
| 0546-ipu-bridge-samsung-galaxy-book5-pro-360-upside-down.patch | omarchy | dropped | already in 7.3.1 (reverse-applies at --fuzz=0) |
| 0547-int3472-imx471-vana-power-enable.patch | omarchy | skipped | conflicts; Intel-only / other-laptop (Intel INT3472 camera PMIC) (owner ruling) |
| 0548-media-cvs-sensor-node-in-bound-callback.patch | omarchy | applied |  |
| 0549-media-imx471-analog-gain-max-960.patch | omarchy | applied |  |
| 0550-media-ipu7-clean-up-mmus-after-auxiliary-devices.patch | omarchy | applied |  |
| 0551-usbio-reset-resume-sabre.patch | omarchy | applied |  |
| 0552-ipu-bridge-revert-himax-hm1092.patch | omarchy | applied |  |
| 0560-input-updates.patch | omarchy | skipped | conflicts; a backport bundle, ~95–100 % upstream in 7.3.1 (owner ruling 2026-11-02) |
| 0561-input-fixes.patch | omarchy | dropped | already in 7.3.1 (reverse-applies at --fuzz=0) |
| 0565-i2c-asue140d-touchpad-100khz.patch | omarchy | applied |  |
| 0566-hid-asus-no-keyboard-init-reports-to-touchpads.patch | omarchy | applied |  |
| 0567-hid-magicmouse-null-deref-without-input-device.patch | omarchy | applied |  |
| 0568-hid-updates.patch | omarchy | skipped | conflicts; a backport bundle, ~95–100 % upstream in 7.3.1 (owner ruling 2026-11-02) |
| 0569-hid-fixes-1.patch | omarchy | dropped | already in 7.3.1 (reverse-applies at --fuzz=0) |
| 0570-hid-fixes-2.patch | omarchy | dropped | already in 7.3.1 (reverse-applies at --fuzz=0) |
| 0571-hid-fixes-3.patch | omarchy | dropped | already in 7.3.1 (reverse-applies at --fuzz=0) |
| 0572-hid-asus-worker-teardown.patch | omarchy | applied |  |
| 0601-thunderbolt-updates.patch | omarchy | skipped | conflicts; a backport bundle, ~95–100 % upstream in 7.3.1 (owner ruling 2026-11-02) |
| 0602-thunderbolt-fixes.patch | omarchy | dropped | already in 7.3.1 (reverse-applies at --fuzz=0) |
| 0603-usb-updates.patch | omarchy | skipped | conflicts; a backport bundle, ~95–100 % upstream in 7.3.1 (owner ruling 2026-11-02) |
| 0604-xhci-fixes.patch | omarchy | dropped | already in 7.3.1 (reverse-applies at --fuzz=0) |
| 0605-thunderbolt-pcie-rescan-after-tunnel.patch | omarchy | applied |  |
| 0606-thunderbolt-fixes-2.patch | omarchy | dropped | already in 7.3.1 (reverse-applies at --fuzz=0) |
| 0607-usb-fixes.patch | omarchy | dropped | already in 7.3.1 (reverse-applies at --fuzz=0) |
| 0608-thunderbolt-fixes-3.patch | omarchy | applied |  |
| 0609-usb-typec-ucsi-recover-from-silent-ppm-completion.patch | omarchy | applied |  |
| 0610-usb-typec-ucsi-acpi-early-cci-poll-dell-xps.patch | omarchy | applied |  |
| 0620-usb-string-sanitize.patch | omarchy | applied |  |
| 0660-btusb-mediatek-mt7922-13d3-3625.patch | omarchy | dropped | already in 7.3.1 (reverse-applies at --fuzz=0) |
| 0662-iwlwifi-mld-skip-tx-when-firmware-dead.patch | omarchy | applied |  |
| 0663-btintel-pcie-serialize-reset-type.patch | omarchy | skipped | conflicts; Intel-only / other-laptop (Intel Bluetooth PCIe) (owner ruling) |
| 0664-btintel-pcie-stale-cache-in-dxstate-fallback.patch | omarchy | applied |  |
| 0665-btintel-pcie-pm-flow-s0ix-s3-s4.patch | omarchy | applied |  |
| 0666-btintel-pcie-verify-serialize-dstate.patch | omarchy | applied |  |
| 0667-btintel-pcie-gp0-cause-w1c-write.patch | omarchy | applied |  |
| 0668-btintel-pcie-dstate-races.patch | omarchy | applied |  |
| 0669-btintel-version-tlv-parse-errors.patch | omarchy | dropped | already in 7.3.1 (reverse-applies at --fuzz=0) |
| 0680-wifi-brcmfmac-pcie-ignore-missing-d3-ack-without-wowl.patch | omarchy | applied |  |
| 0681-wifi-brcmfmac-report-port-authorized-for-4-way-hs-offload.patch | omarchy | applied |  |
| 0700-pci-target-speed-quirk.patch | omarchy | applied |  |
| 0702-ahci-jmb585-32bit-dma-revert.patch | omarchy | applied |  |
| 0703-pci-vmd-scan-bios-enumerated-devices.patch | omarchy | skipped | conflicts; Intel-only / other-laptop (Intel VMD) (owner ruling) |
| 0704-pci-vmd-meteor-lake-mtl016-flush.patch | omarchy | applied |  |
| 0705-pci-toshiba-ahci-ssd-dma-alias-quirk.patch | omarchy | applied |  |
| 0706-pci-vmd-arrow-lake-arl004-flush.patch | omarchy | applied |  |
| 0707-pci-intel-dg1-4905-no-flr-no-bus-reset.patch | omarchy | applied |  |
| 0710-pci-vmd-arrow-lake-hx-bus-restrict-cfg-3.patch | omarchy | dropped | already in 7.3.1 (reverse-applies at --fuzz=0) |
| 0711-iommu-amd-perfopt-support.patch | omarchy | applied |  |
| 0712-drm-amdgpu-perfopt-in-identity-domain.patch | omarchy | applied |  |
| 0713-pci-vmd-nova-lake-dunlow-device-ids.patch | omarchy | dropped | already in 7.3.1 (reverse-applies at --fuzz=0) |
| 0740-hwmon-updates.patch | omarchy | skipped | conflicts; a backport bundle, ~95–100 % upstream in 7.3.1 (owner ruling 2026-11-02) |
| 0741-hwmon-fixes-1.patch | omarchy | dropped | already in 7.3.1 (reverse-applies at --fuzz=0) |
| 0742-hwmon-fixes-2.patch | omarchy | dropped | already in 7.3.1 (reverse-applies at --fuzz=0) |
| 0770-acpi-pm-acer-swift3-sf314-56g-power-resource.patch | omarchy | applied |  |
| 0771-gpiolib-acpi-quirk-irq-active-low-dell-xps.patch | omarchy | applied |  |
| 0772-acpi-pm-casper-excalibur-g900-power-resource.patch | omarchy | applied |  |
| 0773-acpi-pm-acer-aspire-a515-53g-power-resource.patch | omarchy | applied |  |
| 0774-acpi-video-acer-nitro-an515-46-native-backlight.patch | omarchy | dropped | already in 7.3.1 (reverse-applies at --fuzz=0) |
| 0775-acpi-button-razer-blade-pro-17-lid-open.patch | omarchy | applied |  |
| 0776-acpi-pm-s2idle-ixx-gpe-wake-indicator.patch | omarchy | applied |  |
| 0777-acpi-pm-sleep-drop-unused-sleep-no-lps0.patch | omarchy | applied |  |
| 0800-platform-updates.patch | omarchy | skipped | conflicts; a backport bundle, ~95–100 % upstream in 7.3.1 (owner ruling 2026-11-02) |
| 0801-amd-pmf-util-unbind-use-after-free.patch | omarchy | applied |  |
| 0802-platform-fixes.patch | omarchy | dropped | already in 7.3.1 (reverse-applies at --fuzz=0) |
| 0803-acpi-battery-hook-api-acpi-prefix.patch | omarchy | applied |  |
| 0804-acpi-battery-unified-hooks-acpi-sbs.patch | omarchy | skipped | conflicts; Intel-only / other-laptop (other laptops' ACPI SBS battery hooks) (owner ruling) |
| 0805-hwmon-applesmc-charge-control-end-threshold.patch | omarchy | applied |  |
| 0806-platform-chrome-cros-usbpd-notify-parent-goog0004.patch | omarchy | applied |  |
| 0807-platform-msi-wmi-hotkeys-vendor-backlight.patch | omarchy | applied |  |
| 0808-hid-amd-sfh-tablet-mode-switch.patch | omarchy | dropped | already in 7.3.1 (reverse-applies at --fuzz=0) |
| 0850-futex-wait-multiple.patch | omarchy | applied |  |
| 0851-futex-wait-multiple-fixes.patch | omarchy | applied |  |
| 0852-futex-wait-multiple-abi-fixes.patch | omarchy | applied |  |
| 0860-module-add-module-kallsyms-on-each-addr.patch | omarchy | applied |  |
| 0861-ftrace-drop-weak-function-locations-of-modules.patch | omarchy | applied |  |
| 0863-ftrace-free-empty-module-page-group.patch | omarchy | applied |  |
| 8204-mm-opportunistic-compaction.patch | omarchy | applied |  |
| 8205-mm-hint-uses-allocation-order.patch | omarchy | skipped | conflicts; the mm compaction series (owner ruling) |
| 8206-mm-carry-order-and-hint-in-one-word.patch | omarchy | skipped | conflicts; the mm compaction series (owner ruling) |
| 8207-mm-classify-huge-page-allocations-as-failable.patch | omarchy | skipped | conflicts; the mm compaction series (owner ruling) |
| 8209-xe-shrinker-use-opportunistic-hint.patch | omarchy | applied |  |
| 8210-xe-shrinker-single-backup-decision.patch | omarchy | applied |  |
| 8211-xe-shrinker-skip-active-vm.patch | omarchy | skipped | conflicts; Intel xe shrinker (owner ruling) |
| 8212-xe-shrinker-backup-in-kswapd-only.patch | omarchy | skipped | conflicts; Intel xe shrinker (owner ruling) |
| 8213-xe-hibernation-writeback-gated.patch | omarchy | skipped | conflicts; Intel xe shrinker (owner ruling) |
| 8214-xe-shrinker-protect-only-small-active-vms.patch | omarchy | skipped | conflicts; Intel xe shrinker (owner ruling) |
| 8218-xe-shrinker-protect-active-vms-until-kswapd-fails.patch | omarchy | skipped | conflicts; Intel xe shrinker (owner ruling) |
| 8219-xe-oom-wait-for-victim-gpu-memory.patch | omarchy | skipped | conflicts; Intel xe shrinker (owner ruling) |
| 8220-mm-page_alloc-clear-kswapd-failures-when-a-zone-recovers.patch | omarchy | skipped | conflicts; the mm compaction series (owner ruling) |
| 8221-xe-shrinker-dont-abort-walk-on-ebusy.patch | omarchy | applied |  |
| 8222-xe-shrinker-stop-walk-at-budget-after-ebusy.patch | omarchy | applied |  |
| 8223-drm-xe-bo-add-addr-iter.patch | omarchy | applied |  |
| 8224-drm-xe-display-dpt-remapped-addr-iter.patch | omarchy | applied |  |
| 8225-xe-shrinker-active-vm-ram-divisor.patch | omarchy | skipped | conflicts; Intel xe shrinker (owner ruling) |
