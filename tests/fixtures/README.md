# Test fixtures

Every byte here is produced by code in this repository. No fixture is copied
from the Rust reference (decision 0002 §3: the reference oracle is retired for
the trees; the Zig-side invariants are the oracle).

## `findvsa_pred_bset.v5.bin`

The FIND-VS-A predecessor leaf: a populated bset image of the shape the
reference's capture had — one directory's dentries hash-shuffled among their
inodes, `Put`s and Δtime `Delta`s, no tombstones, a `journal_seq_horizon`
above every record seq — in the format-v5 record frame (16 B header) over v4
keys (the kind byte on every key).

Generator: `src/meta/bset.zig` `findVsAFixtureImage` — directory ino 2 with
`findvsa_fixture_files` = 1640 children, one inode `Put` + one dentry `Put`
each (names `f<i>-<random u32>`, `hash54` seeded with
`findvsa_fixture_seed` = `0xF1D5_A000_0000_0004`), a Δtime on every seventh
inode, seqs ascending from `horizon − 4 × files` through one
`std.Random.DefaultPrng(seed)`, memcmp-sorted `(key, seq)`; built with
`buildBset(…, horizon = 72_220_661)`. 254,932 bytes, 3,515 records.

The pin test in `bset.zig` ("the committed FIND-VS-A fixture is the
generator's bytes …") regenerates the image in memory on every run and
compares it byte for byte to this file, so the file and the generator can
never drift. Regenerate only with an intentional format change, from the
repository root:

```
zig run --dep squeezefs -Mroot=tests/fixtures/gen_findvsa_pred_bset.zig -Msqueezefs=src/squeezefs.zig
```

then review the diff and commit the new bytes with that change.
