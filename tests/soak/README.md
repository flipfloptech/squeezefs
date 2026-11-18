# PR-6 soak and load runs (decision 0004 §8.3)

These runs are not part of `zig build gate`. They take a venue slot and can
run for hours. From the repo root, on a Linux venue:

```
SQZ_VENUE_SLOT=1 scripts/venue tests/soak/load.sh
SQZ_VENUE_SLOT=1 scripts/venue tests/soak/rows.sh
SQZ_VENUE_SLOT=1 scripts/venue tests/soak/succession.sh
SQZ_VENUE_SLOT=1 scripts/venue tests/soak/succession.sh --ring 1024
```

* `load.sh` — F1, F2, F4, F7a and F7b, each three times, under `cpu+smoke` load.
* `rows.sh` — the eviction / re-join / KILL smoke rows three times, and the
  §DM churn unit once per pass.
* `succession.sh` — the 20-succession soak at the derived ring, or at the
  1 GiB ring (`--ring 1024`). Writers stay up across the successions.

Logs go under `~/tmp/pr6-soak/` unless `SQZ_SOAK_LOG_DIR` is set.
