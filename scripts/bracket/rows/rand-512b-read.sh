# Random 512 B O_DIRECT reads at a 512 B offset grain (decision 0003 §K row 1):
# the ranged DMA is 4 KiB-outward, so one 512 B read costs one 4 KiB LBA —
# bound 8.0 (the venue's namespaces are 4 KiB-LBA: a 512 B DMA is EINVAL there;
# a 512 B-LBA device would admit a 512 B grain and the bound would be 1.0).
ROW_KIND=data-read
ROW_NEEDS_SHIM=0
FIO_JOB=rand-bs.fio
FIO_RW=randread
FIO_DIRECT=1
FIO_BS=512
FIO_BLOCKALIGN=512
FIO_PREFIX=rand
FIO_HEADLINE=iops
FIO_READ_AMP_BOUND=8.4
FIO_READ_AMP_WHY="W(off,512)/512 = 4096/512 = 8 at a 512 B grain, × (1 + f) with f = SQUEEZEFS_READ_ADMISSION_FILL_PCT 5 % for the ghost escalations the governor admits"
# shellcheck source=_fio.sh
source "$(dirname "${BASH_SOURCE[0]}")/_fio.sh"
