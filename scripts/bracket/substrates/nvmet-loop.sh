# Substrate `nvmet-loop`: the LATENCY-shaped substrate (charter §11 — loop
# hides bandwidth effects; use it for IOPS, fsync and metadata rows).
NVMET_TRTYPE=loop
# shellcheck source=nvmet-common.sh
source "$(dirname "${BASH_SOURCE[0]}")/nvmet-common.sh"
