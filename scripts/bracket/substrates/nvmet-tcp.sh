# Substrate `nvmet-tcp`: nvme-tcp on 127.0.0.1 — the FABRIC-sensitive
# substrate (charter §11: anything bandwidth-shaped runs here; the loop
# transport hides what the TCP copy path costs).
NVMET_TRTYPE=tcp
# shellcheck source=nvmet-common.sh
source "$(dirname "${BASH_SOURCE[0]}")/nvmet-common.sh"
