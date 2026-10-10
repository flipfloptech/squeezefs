# The fuzz campaign (`zig build fuzz`)

Charter §11: *"Fuzz every untrusted decoder … with a stable-toolchain
property-test mirror ('never panic on arbitrary bytes; round-trip'). A lying
length/count is never an allocation authority"*; *"nightly fuzz"*.

## The pieces

| piece | where |
|---|---|
| The census (the naming law: a `pub fn decode*/parse*/from*` over a const byte slice; the fuzz closure of a `test "fuzz: …"`) | `src/gates/census.zig` |
| The rail: every decoder named by a fuzz test in its file; every fuzz test calls the ONE entry; names unique tree-wide | `tests/rails.zig` "fuzz census" |
| The ONE entry with the replay seam (`SQUEEZEFS_TEST_FUZZ_REPLAY`) and the corpus framing helper | `src/core/fuzzkit.zig` |
| The campaign driver: per target a fuzz-mode rebuild, an overhead launch, an adaptive calibration, the budgeted run; persistent corpus; crash save + minimize; the report | `src/gates/fuzz_campaign.zig` |
| Knobs | `SQUEEZEFS_TEST_FUZZ_SECONDS` (default 60), `SQUEEZEFS_TEST_FUZZ_CORPUS_DIR` (default `/build/fuzz-corpus`; elsewhere `fuzz-corpus/` under the repo, gitignored), `SQUEEZEFS_TEST_FUZZ_REPLAY` |

## Running it

```
scripts/venue zig build fuzz                               # the nightly: 60 s per target
scripts/venue "SQUEEZEFS_TEST_FUZZ_SECONDS=10 zig build fuzz"   # a short campaign
scripts/venue "zig build fuzz -Dfuzz-filter='the knob parsers'" # one target
```

Each target runs the build runner's fuzzer in its iteration-limited mode
(`zig build test -Dtest-filter=<name> --fuzz=<cycles>`), one fuzz test per
launch, so every exec is counted exactly (the runner's `FUZZING REPORT`);
the budget is converted to cycles by a calibration launch timed on the
venue. The fuzzer's corpus dir (`<cache>/f/<hex(wyhash(qualified name))>/`)
is relocated onto `<corpus root>/<slug>/` with a symlink left in the cache,
and the slug dir's `.name` file lets the next campaign recreate the symlink
after a cache wipe. The campaign exits 1 on any crash.

## A crash

The runner saves the failing input to `<cache>/f/crash`; the driver copies
it to `<corpus root>/<slug>/crashes/<unix s>.bin`, checks it reproduces
through the replay seam under the stable toolchain, and shrinks it by greedy
chunk removal to `<unix s>.min.bin` (each probe is one
`SQUEEZEFS_TEST_FUZZ_REPLAY=<probe> zig build test -Dtest-filter=<name>`;
the replay path rides the run step's environment, so a probe is never a
cached pass). The report names the replay command. The engineer's next
commit is the red repro — `fuzzkit.slice(&.{ … the minimized bytes … })` in
the target's corpus — and the fix. The build runner's limited mode exits 0
on a crash and, rarely, saves an EMPTY artefact (its input capture raced the
crash): the report says so and asks for a re-run of that target.

## Report format

`target | file | execs | corpus | coverage | crashes | s` — execs is the
cumulative-run delta the runner reports across the target's launches, corpus
the number of input files in the slug dir after the run, coverage the
runner's seen/total PCs for the target's binary, s the wall time of the
target including its rebuild.

## The first campaign (2026-10-10, the venue, 10 s per target)

73 targets × 10 s = 730 s budget (wall ≈ 28 min with the per-target fuzz-mode
rebuilds); corpus root `/build/fuzz-corpus` (10 MB after the run);
**84,424,445 execs, 0 crashes.**

| target | file | execs | corpus | coverage | crashes | s |
|---|---|---:|---:|---:|---:|---:|
| fuzz: AlignedBuf.fromSlice copies any slice at any power-of-two alignment up to a page | src/io/uring_fs.zig | 349435 | 6 | 479/22568 | 0 | 26 |
| fuzz: SessionHeader.decode never panics on arbitrary bytes and a header that decodes carries a valid geometry | src/ipc/layout.zig | 732168 | 3 | 71/22373 | 0 | 22 |
| fuzz: bset parse and the frame probe are total on arbitrary and restamped bytes | src/meta/bset.zig | 794615 | 27 | 188/22528 | 0 | 23 |
| fuzz: classify is total; a valid page re-encodes byte-identically | src/meta/sym/appender_page.zig | 527723 | 7 | 138/22775 | 0 | 27 |
| fuzz: decodeFrom and decodePayload are total on arbitrary bytes and a decoded fixed value re-encodes to its bytes | src/wire/codec.zig | 1586547 | 7 | 103/22616 | 0 | 27 |
| fuzz: decodeHeader is total on arbitrary bytes | src/data/transform.zig | 1238760 | 14 | 129/22432 | 0 | 31 |
| fuzz: every PDU decoder is total on arbitrary bytes and exactly the encoder's form decodes | src/ops/zcrx/pdu.zig | 1951169 | 11 | 116/22458 | 0 | 33 |
| fuzz: every argv word parser is total on arbitrary text | src/cli/args.zig | 1902260 | 116 | 318/22561 | 0 | 35 |
| fuzz: every data-plane record decoder is total and encode∘decode is the identity | src/data/sym_planes/records.zig | 1360335 | 13 | 134/22612 | 0 | 25 |
| fuzz: every decoder is total on arbitrary bytes | src/fuse/abi.zig | 1088458 | 17 | 500/22802 | 0 | 24 |
| fuzz: every key decoder and the interior value decoder are total on arbitrary bytes; whatever decodes is byte-identical to its encoding | src/meta/key.zig | 1254485 | 14 | 155/22638 | 0 | 25 |
| fuzz: every record value decoder is total on arbitrary bytes | src/meta/record.zig | 5402852 | 209 | 806/22746 | 0 | 102 |
| fuzz: every screen is total over arbitrary words and arbitrary bounds (never a panic) | src/wire/screen.zig | 1218338 | 26 | 265/24131 | 0 | 29 |
| fuzz: every tree-0 decoder is total on arbitrary bytes and canonical where it accepts | src/meta/sym/control_records.zig | 1209375 | 78 | 373/22897 | 0 | 23 |
| fuzz: every verb decoder is total on arbitrary bytes | src/wire/verbs.zig | 521281 | 420 | 2018/25163 | 0 | 27 |
| fuzz: no admin payload decoder panics on arbitrary bytes | src/ops/jobs/admin_frames.zig | 964254 | 9 | 144/22712 | 0 | 22 |
| fuzz: no job payload decoder panics on arbitrary bytes | src/ops/jobs/frames.zig | 1266283 | 9 | 154/22647 | 0 | 28 |
| fuzz: the -o option string and the seconds parsers are total on arbitrary text | src/fuse/mount_options.zig | 1138949 | 39 | 214/22629 | 0 | 22 |
| fuzz: the .stats term reader is total on arbitrary JSON-shaped text | src/cli/bench.zig | 188000 | 29 | 176/22462 | 0 | 27 |
| fuzz: the /proc/<pid>/stat cpu-field parser is total on arbitrary text | src/ipc/host/service.zig | 1274757 | 3 | 80/22387 | 0 | 23 |
| fuzz: the CQE gates are total and admit only in-area spans | src/ops/zcrx/abi.zig | 1166171 | 2 | 79/22401 | 0 | 22 |
| fuzz: the IP-literal parser is total on arbitrary text | src/ops/zcrx/ethtool.zig | 1248355 | 58 | 170/22464 | 0 | 24 |
| fuzz: the URI parser is total on arbitrary text — every accepted member is absolute, every refusal prints | src/cli/uri.zig | 1129444 | 15 | 145/22436 | 0 | 26 |
| fuzz: the admission policy name parser is total on arbitrary text | src/data/admission.zig | 2848368 | 4 | 79/22375 | 0 | 45 |
| fuzz: the appender directory header decoder is total on arbitrary and restamped pages; a decoded header re-encodes byte-identically | src/meta/sym/appender_dir.zig | 417017 | 5 | 73/22623 | 0 | 29 |
| fuzz: the boot-id uuid and the /proc/<pid>/stat start-ticks parsers are total on arbitrary text | src/meta/guard/proc.zig | 1274410 | 31 | 148/22397 | 0 | 29 |
| fuzz: the cgroup and proc text readers are total on arbitrary text | src/mem/budget.zig | 683085 | 42 | 217/22413 | 0 | 29 |
| fuzz: the compression and encryption name parsers are total on arbitrary text | src/data/transform.zig | 2117625 | 3 | 87/22387 | 0 | 41 |
| fuzz: the cpu-list parser is total on arbitrary sysfs text | src/core/derive.zig | 1100415 | 40 | 166/22373 | 0 | 28 |
| fuzz: the cpuset and kernel-cmdline isolation parsers are total on arbitrary text | src/exec/placement.zig | 834239 | 49 | 224/22418 | 0 | 25 |
| fuzz: the data bitmap page decoder is total on arbitrary and restamped pages; a decoded page re-encodes byte-identically | src/data/sym_planes/data_bitmap.zig | 571957 | 2 | 111/22582 | 0 | 26 |
| fuzz: the data superblock decoders are total on arbitrary bytes | src/data/data_superblock.zig | 814613 | 5 | 114/22705 | 0 | 32 |
| fuzz: the endpoint and bind text parsers are total and a parsed endpoint round-trips through its printed form | src/wire/endpoint.zig | 1061752 | 107 | 340/22796 | 0 | 24 |
| fuzz: the enroll decoder is total | src/wire/auth.zig | 1169223 | 2 | 39/22394 | 0 | 24 |
| fuzz: the extent key, alloc record and bitmap page decoders are total; a restamped page that decodes re-encodes byte-identically | src/meta/alloc_ext.zig | 461957 | 11 | 75/22603 | 0 | 26 |
| fuzz: the frame and header decoders are total on arbitrary bytes | src/wire/frame.zig | 921308 | 6 | 106/22496 | 0 | 26 |
| fuzz: the fsck report and shard parsers are total on arbitrary text and a parsed report re-serializes to a parseable report | src/ops/fsck/finding.zig | 374343 | 155 | 1058/25674 | 0 | 29 |
| fuzz: the host identity pair decoder is total — an over-long NQN or a malformed uuid is null, never a panic | src/ops/zcrx/association.zig | 1094181 | 15 | 104/22577 | 0 | 22 |
| fuzz: the il0 blob, the admin datagrams and every fixed ctl frame decode totally; whatever decodes re-encodes to its bytes | src/ipc/wire.zig | 1174882 | 8 | 151/22699 | 0 | 23 |
| fuzz: the intent decoder and its stored form are total and encode∘decode is the identity | src/meta/sym/xv/intent.zig | 957463 | 40 | 284/22792 | 0 | 22 |
| fuzz: the job record, job shard and job-id decoders are total on arbitrary bytes; whatever decodes re-encodes to itself | src/ops/jobs/records.zig | 1268549 | 23 | 159/22705 | 0 | 20 |
| fuzz: the journal page header and record frame decoders are total; a decoded frame re-encodes to the bytes it consumed | src/meta/journal.zig | 1166905 | 16 | 102/22637 | 0 | 20 |
| fuzz: the kit's own entry runs its corpus and the empty stream (the census fixture) | src/core/fuzzkit.zig | 1375527 | 1 | 68/22371 | 0 | 20 |
| fuzz: the knob parsers are total on arbitrary environment text and canonical spellings are fixpoints | src/core/knobs.zig | 1171108 | 42 | 194/22595 | 0 | 22 |
| fuzz: the layout, delta, mapping and xattr-layout decoders are total; whatever decodes re-encodes byte-identically | src/data/layout.zig | 1020265 | 73 | 262/22704 | 0 | 27 |
| fuzz: the ledger decoder is total on arbitrary bytes | src/ops/nvmet/ledger.zig | 1163623 | 5 | 30/22577 | 0 | 21 |
| fuzz: the ledger slot decoder is total on arbitrary and restamped slots; a decoded record re-encodes to its image | src/meta/ledger.zig | 704414 | 20 | 523/22969 | 0 | 21 |
| fuzz: the logical_block_size sysfs parser is total on arbitrary text | src/io/align_probe.zig | 1186884 | 34 | 149/22378 | 0 | 20 |
| fuzz: the lz4 decompressor is total on arbitrary bytes | src/data/transform.zig | 1140485 | 23 | 111/22416 | 0 | 20 |
| fuzz: the manager endpoint record decoder is total on arbitrary bytes | src/meta/sym/joined.zig | 1366664 | 2 | 62/22405 | 0 | 20 |
| fuzz: the max_pages_limit sysfs parser is total on arbitrary text | src/fuse/geometry.zig | 1322204 | 31 | 146/22375 | 0 | 21 |
| fuzz: the member row decoder is total on arbitrary and restamped claims; a decoded row re-encodes byte-identically | src/meta/sym/membership/shard.zig | 1080526 | 4 | 56/22587 | 0 | 21 |
| fuzz: the node header page decoder is total on arbitrary and restamped pages; a decoded header survives encode∘decode | src/meta/node.zig | 530160 | 3 | 59/22615 | 0 | 27 |
| fuzz: the node-seq incarnations record decoder is total and a decoded value re-encodes to its bytes | src/meta/node_seq.zig | 1425772 | 4 | 29/22561 | 0 | 21 |
| fuzz: the nvmf_dev_read read-back parser is total on arbitrary text | src/ops/nvmet/initiator.zig | 1335780 | 74 | 226/22472 | 0 | 20 |
| fuzz: the parser is total on arbitrary bytes at arbitrary chunk grains | src/ops/zcrx/pdu_stream.zig | 1319049 | 15 | 54/22465 | 0 | 20 |
| fuzz: the peer-id decoder is total and a decoded identity re-encodes to its 12 bytes | src/wire/verbs.zig | 1400054 | 2 | 23/22557 | 0 | 19 |
| fuzz: the read-cache frame header decoder is total on arbitrary and restamped headers; a decoded header re-encodes byte-identically | src/data/nvme_read_cache.zig | 832232 | 2 | 47/22579 | 0 | 20 |
| fuzz: the record header reader, the verifier and the marker decoder are total | src/data/staging.zig | 536065 | 4 | 54/22440 | 0 | 22 |
| fuzz: the request path is total on arbitrary bytes | src/fuse/codec.zig | 1134759 | 7 | 134/22692 | 0 | 21 |
| fuzz: the schedstat and stat cpu-time parsers are total on arbitrary text | src/exec/cpu_class.zig | 999907 | 40 | 153/22372 | 0 | 20 |
| fuzz: the set_config decoder is total on arbitrary bytes | src/data/set_config.zig | 1177716 | 3 | 28/22963 | 0 | 23 |
| fuzz: the spill record decoder never panics on arbitrary bytes | src/data/extent_overlay.zig | 577049 | 4 | 119/22416 | 0 | 21 |
| fuzz: the superblock sector decoders are total on arbitrary and restamped sectors under every known-mask; a decoded superblock survives encode∘decode | src/meta/superblock.zig | 513997 | 4 | 124/22502 | 0 | 22 |
| fuzz: the supervisor's waiting-count parser is total on arbitrary text | src/cli/supervisor.zig | 1397555 | 30 | 102/22375 | 0 | 21 |
| fuzz: the sysfs namespace-name and address parsers are total on arbitrary text | src/ops/zcrx/nic_census.zig | 1348358 | 70 | 235/22482 | 0 | 20 |
| fuzz: the target-override seam parser is total on arbitrary text | src/ops/zcrx/probe.zig | 1256232 | 123 | 299/22595 | 0 | 19 |
| fuzz: the trtype and uuid attribute parsers are total on arbitrary configfs text | src/ops/nvmet/configfs.zig | 1405324 | 19 | 122/22401 | 0 | 19 |
| fuzz: the uring command and the reservation report image decoders are total; a lying regctl never allocates past the image | src/meta/guard/nvme_cmd.zig | 465934 | 7 | 492/22586 | 0 | 23 |
| fuzz: the volume-id text parser is total on arbitrary text | src/data/router.zig | 1418661 | 3 | 81/22440 | 0 | 19 |
| fuzz: the volume-id word parser is total on arbitrary text | src/ops/lifecycle/lattice.zig | 1415123 | 2 | 83/22442 | 0 | 20 |
| fuzz: the write-cache sysfs classifier is total on arbitrary text | src/io/nvme_derive.zig | 1420681 | 10 | 98/22377 | 0 | 19 |
| fuzz: the writer-claim decoder is total on arbitrary and restamped records; a decoded claim re-encodes byte-identically | src/meta/guard/claim.zig | 1156031 | 4 | 56/22587 | 0 | 20 |

Total: 84,424,445 execs, 0 crashes.
