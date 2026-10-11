# The nvmet substrates (charter §8, §11): the venue's REAL kernel nvmet target
# stood up through the product's own verbs — `nvmeof share` / `nvmeof connect`
# — with no nvme-cli, no modprobe, no losetup, zero mocks (the fidelity tier's
# recipe, `src/ops/nvmet/fidelity.zig`). One namespace per subsystem: a meta
# target + DATA_VOLUMES data targets per set; the backing files live on /build
# (so the full substrate chain is `nvmeXnY ← nvmet-{loop,tcp} ← file on
# /dev/vdc`: the namespace's diskstats are exclusive to this mount and exact;
# the disk's are shared). Every object is pid-tagged (`brk-<pid>`), rides its
# own port-id slice and share-ledger dir, and is torn down by manifest.

NVMET_TRTYPE=${NVMET_TRTYPE:?substrate must set NVMET_TRTYPE (loop|tcp)}
# A namespace is exclusive to this mount: its diskstats read/write sectors
# are the ruler for `device ÷ user` (verified: the namespace counts every
# direct read issued to it even when the backing file sits in the VM's page
# cache — `docs/evidence/rung-09-read-amplification.md` §1).
DATA_DEVICES_EXCLUSIVE=1
NVMET_PORT_BASE=$(( 4500 + ($$ % 400) ))
NVMET_SHARES=()
NVMET_CONNECTS=()

nvmet_env() {
  # The venue has an empty /etc/machine-id, so the derived host identity
  # refuses by design; the bracket presents an explicit RFC 4122-shaped pair.
  local tag; tag=$(printf '%08x' $(( $$ & 0xffffffff )))
  export SQUEEZEFS_HOSTNQN="nqn.2014-08.org.nvmexpress:uuid:${tag}-b2b2-4b2b-8b2b-b2b2b2b2b2b2"
  export SQUEEZEFS_HOSTID="${tag}-b2b2-4b2b-8b2b-b2b2b2b2b2b2"
  export SQUEEZEFS_NVMEOF_STATE_DIR="$SCRATCH/nvmet-state"
  export SQUEEZEFS_NVMET_PORT_ID_BASE=55000
  mkdir -p "$SQUEEZEFS_NVMEOF_STATE_DIR"
  mountpoint -q /sys/kernel/config || mount -t configfs none /sys/kernel/config
}

# share <bin> <name> <size> <port> → NVMET_LAST = the subsystem nqn. (Plain
# calls, never `$(…)`: the manifest arrays must grow in THIS shell.)
nvmet_share() {
  local bin=$1 name=$2 size=$3 port=$4
  local nqn="nqn.2026-10.io.squeezefs:brk-$$-$name" backing="$SCRATCH/$name.img"
  local out
  if [ "$NVMET_TRTYPE" = loop ]; then
    out=$("$bin" nvmeof share --subnqn "$nqn" --transport loop --create-size "$size" "$backing" 2>&1) || { echo "$out" >&2; log "share $name failed"; exit 1; }
  else
    out=$("$bin" nvmeof share --subnqn "$nqn" --ip 127.0.0.1 --port "$port" --create-size "$size" "$backing" 2>&1) || { echo "$out" >&2; log "share $name failed"; exit 1; }
  fi
  echo "$out" >> "$SCRATCH/nvmet.log"
  NVMET_SHARES+=("$nqn")
  NVMET_LAST=$nqn
}

# connect <bin> <nqn> <port> → NVMET_LAST = the namespace device path (/dev/nvmeXnY)
nvmet_connect() {
  local bin=$1 nqn=$2 port=$3 out
  if [ "$NVMET_TRTYPE" = loop ]; then
    out=$("$bin" nvmeof connect --transport loop "$nqn" 2>&1) || { echo "$out" >&2; log "connect $nqn failed"; exit 1; }
  else
    out=$("$bin" nvmeof connect --ip 127.0.0.1 --port "$port" "$nqn" 2>&1) || { echo "$out" >&2; log "connect $nqn failed"; exit 1; }
  fi
  echo "$out" >> "$SCRATCH/nvmet.log"
  local dev
  dev=$(printf '%s\n' "$out" | sed -n 's/^  device //p' | head -1)
  [ -n "$dev" ] || { echo "$out" >&2; log "connect printed no device"; exit 1; }
  NVMET_CONNECTS+=("$nqn")
  # The node appears a moment after the controller; wait for it.
  local i; for i in $(seq 1 100); do [ -b "$dev" ] && break; sleep 0.1; done
  [ -b "$dev" ] || { log "namespace node $dev never appeared"; exit 1; }
  NVMET_LAST=$dev
}

substrate_up() {   # bin nsets
  local bin=$1 nsets=$2 s v port=$NVMET_PORT_BASE disk
  nvmet_env
  disk=$(backing_disk_of /build)
  META_DEV=(); DATA_DEVS_BY_SET=(); META_DISK_DEVS_BY_SET=(); DATA_DISK_DEVS_BY_SET=()
  DISK_DEVS=("$disk")
  for s in $(seq 0 $((nsets - 1))); do
    local nqn dev
    nvmet_share "$bin" "set$s-meta" "$META_SIZE" "$port"; nqn=$NVMET_LAST
    nvmet_connect "$bin" "$nqn" "$port"; dev=$NVMET_LAST; port=$((port + 1))
    META_DEV[$s]=$dev
    META_DISK_DEVS_BY_SET[$s]=$(basename "$dev")
    DISK_DEVS+=("$(basename "$dev")")
    local list="" names=""
    for v in $(seq 0 $((DATA_VOLUMES - 1))); do
      nvmet_share "$bin" "set$s-data$v" "$DATA_SIZE" "$port"; nqn=$NVMET_LAST
      nvmet_connect "$bin" "$nqn" "$port"; dev=$NVMET_LAST; port=$((port + 1))
      list="${list:+$list,}$dev"
      names="${names:+$names }$(basename "$dev")"
      DISK_DEVS+=("$(basename "$dev")")
    done
    DATA_DEVS_BY_SET[$s]=$list
    DATA_DISK_DEVS_BY_SET[$s]=$names
  done
  local tr="nvmet-loop"; [ "$NVMET_TRTYPE" = tcp ] && tr="nvmet-tcp on 127.0.0.1"
  SUBSTRATE_DESC="$tr: $((nsets * (1 + DATA_VOLUMES))) kernel nvmet namespaces (resv_enable=1) connected through \`nvmeof connect\`, backed by sparse files on /build (/dev/$disk)"
  log "substrate: $SUBSTRATE_DESC; meta ${META_DEV[*]}; data ${DATA_DEVS_BY_SET[*]}"
}

substrate_down() {
  local bin=${BIN_A:-squeezefs} nqn
  [ ${#NVMET_CONNECTS[@]} -gt 0 ] && for nqn in "${NVMET_CONNECTS[@]}"; do
    "$bin" nvmeof disconnect "$nqn" >> "$SCRATCH/nvmet.log" 2>&1 || log "disconnect $nqn failed (see nvmet.log)"
  done
  NVMET_CONNECTS=()
  [ ${#NVMET_SHARES[@]} -gt 0 ] && for nqn in "${NVMET_SHARES[@]}"; do
    "$bin" nvmeof unshare "$nqn" >> "$SCRATCH/nvmet.log" 2>&1 || log "unshare $nqn failed (see nvmet.log)"
  done
  NVMET_SHARES=()
  # The residue sweep: anything pid-tagged ours that the manifest missed (a
  # crash between share and the manifest append) is adopted, then unshared —
  # the product tears down only what its ledger records.
  local nqn
  for nqn in $(ls /sys/kernel/config/nvmet/subsystems 2>/dev/null | grep "brk-$$-" || true); do
    log "residue: disconnecting, adopting + unsharing $nqn"
    "$bin" nvmeof disconnect "$nqn" >> "$SCRATCH/nvmet.log" 2>&1 || true
    "$bin" nvmeof adopt "$nqn" >> "$SCRATCH/nvmet.log" 2>&1 || true
    "$bin" nvmeof unshare "$nqn" >> "$SCRATCH/nvmet.log" 2>&1 || log "unshare $nqn failed (see nvmet.log)"
  done
  local left
  left=$(ls /sys/kernel/config/nvmet/subsystems 2>/dev/null | grep -c "brk-$$-" || true)
  [ "$left" = 0 ] || log "RESIDUE: $left nvmet subsystems of brk-$$ survive"
}
