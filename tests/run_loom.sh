#!/bin/bash
# Exhaustive loom model-checking of the lock-free protocol cores
# (inode allocator bitmap, block-key incarnation seqlock, staging budget
# gauge). See loom-models/src/lib.rs for the invariants.
#
# Isolated crate on purpose: a global `--cfg loom` poisons transitive deps
# of the main crate; loom-models depends only on `loom` and #[path]-includes
# the shipped core sources, so the models check the real code.
set -euo pipefail
cd "$(dirname "$0")/../loom-models"
# LOOM_MAX_PREEMPTIONS=3 keeps the exploration exhaustive-in-practice while
# bounded; raise for deeper searches.
RUSTFLAGS="--cfg loom" LOOM_MAX_PREEMPTIONS=3 cargo test --release "$@"
