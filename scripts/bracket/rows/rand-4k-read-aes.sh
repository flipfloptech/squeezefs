# Random 4 KiB O_DIRECT reads on a TRANSFORMED volume (AES-256-GCM frames (raw bodies + a 16 B tag per frame); decision 0003
# §K7/§K10): the framed image's sub-block read — the table grain + one 64 KiB
# frame window per miss — decoded on a per-lane codec. The headline IOPS is the
# transform's throughput row (the decode was serialized behind ONE context:
# 849 IOPS on zstd where lz4 ran 7.8 k); the bound is the incompressible
# frame's: (4 + 68) / 4 × (1 + f) — the prepare's dataset is fio's default
# (incompressible) fill, so every frame is stored raw; the compressible
# shape is `scripts/bracket/xform-probe.sh` with PROBE_SRC.
ROW_KIND=data-read
ROW_NEEDS_SHIM=0
ROW_FORMAT_OPTS="--encrypt aes-256-gcm --encrypt-key @KEY"
FIO_JOB=rand-4k.fio
FIO_RW=randread
FIO_DIRECT=1
FIO_BS=4k
FIO_PREFIX=rand
FIO_HEADLINE=iops
FIO_READ_AMP_BOUND=18.9
FIO_READ_AMP_WHY="(g + ceil_g(F + tag)) / len = (4 + 68) / 4 = 18 on an incompressible 64 KiB frame, × (1 + f), f = SQUEEZEFS_READ_ADMISSION_FILL_PCT 5 % for the governed ghost escalations"
# shellcheck source=_fio.sh
source "$(dirname "${BASH_SOURCE[0]}")/_fio.sh"
