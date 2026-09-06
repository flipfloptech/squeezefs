//! Criterion micro-benches for the DLM **S6 membership plane** — the
//! renewal path (the heartbeat that replaced a journal transaction) and the
//! read-side enumeration (the census that replaced `listxattr(1)` plus one
//! `getxattr` per client).
//!
//! **WRITTEN, NOT RUN** (ruling **D11**, 2026-08-03: "no cargo test, benchs
//! or release gate yet until we are done implementing the DLM and can have
//! N Readers and Writers"). Each group therefore states its FIELD-derived
//! input shape, a **prediction**, and a **falsification criterion** — the
//! thing that must be true for the S6 design to hold, and the observation
//! that would disprove it. A number here is worthless without those two
//! lines, which is why they are in the file and not in a notebook.
//!
//! # Field shapes (all from measured evidence, never invented)
//!
//! * **Member population: 15,000** — ruling **D1**'s design target ("15 k
//!   nodes all reading AND writing"), and the population §6.5 item 3
//!   measures the old plane failing at.
//! * **Beat cadence: 10 s** — `fuse_client::CLIENT_HEARTBEAT_INTERVAL_SECS`,
//!   unchanged by S6 (the shipped-posture floor of the derived renewal
//!   interval). 15,000 members ÷ 10 s = **1,500 renewals/s offered**, which
//!   is precisely the rate §6.5 item 3 measured the journal plane
//!   serializing 455/s against.
//! * **Census page: 1,024 rows** — `membership_wire::CENSUS_PAGE_MAX`, so a
//!   15 k census is 15 pages.
//! * **`squeezefs clients` call rate: operator-paced** — it is a CLI verb
//!   plus `status`, so single-digit calls per minute at worst; the figure
//!   that matters is the WALL time of one full census, not a throughput.
//! * **Claim-set size** — 1 member is the shipped D0 posture (the
//!   projection path, no record at all); 16 and 128 bracket the multi-writer
//!   sets §6.2 item 7 exists for. Encoded on membership CHANGE only, never
//!   per beat.

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};
use squeezefs::membership::{
    ClaimSet, ClaimSetMember, Grant, JoinOutcome, JoinRequest, LeaseClock, LeaseClocks,
    MemberIdentity, MemberRole, MemberSession, MembershipOwner,
};
use std::hint::black_box;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// The shipped clock parameters with a fabric-class RTT: S3 measured
/// cluster-wire round trips at 0.05–0.25 ms
/// (`.benchmarks/2026-08-05-dlm-s3-cluster-wire.md`), so 250 µs is the
/// pessimistic end of the measured range.
fn clocks() -> LeaseClocks {
    LeaseClocks::with_params(
        Duration::from_secs(45),
        Duration::from_millis(23),
        Duration::from_millis(100),
    )
    .expect("the shipped parameters must be safe")
}

/// An owner holding `members` leases, on a manual clock so no bench
/// iteration can trip a TTL sweep mid-measurement.
fn owner_with(members: usize) -> (Arc<MembershipOwner>, Vec<(String, u64)>, Arc<AtomicU64>) {
    let ticks = Arc::new(AtomicU64::new(1_000));
    let owner = MembershipOwner::arm(
        "bench-owner",
        1,
        0,
        clocks(),
        LeaseClock::manual(Arc::clone(&ticks)),
    )
    .expect("arm");
    let mut ids = Vec::with_capacity(members);
    for i in 0..members {
        let id = format!("bench-member-{i}");
        let role = if i % 2 == 0 {
            MemberRole::Reader
        } else {
            MemberRole::Writer
        };
        let req = JoinRequest {
            id: id.clone(),
            role,
            endpoint: match role {
                MemberRole::Writer => Some(format!("10.0.{}.{}:7100", i / 250, 1 + i % 250)),
                MemberRole::Reader => None,
            },
            pid: 4242,
            boot: "bench-boot".to_string(),
            prior_epoch: None,
            pr_key: 0,
            mount: None,
        };
        match owner.join(req) {
            JoinOutcome::Granted(g) => ids.push((id, g.epoch)),
            JoinOutcome::Refused { reason, .. } => panic!("bench join refused: {reason}"),
            JoinOutcome::UnknownLease { .. } => panic!("bench join met an unknown lease"),
        }
    }
    (owner, ids, ticks)
}

/// **The renewal path** — the heartbeat itself, at 100 / 1,500 / 15,000
/// members. This is what a journal transaction under an exclusive `I{1}`
/// guard (2.198 ms measured saturated `commit_tx_wait`) was replaced with.
///
/// **Prediction:** ≤ 1 µs/renewal, and **flat** across the three
/// populations — the operation is one `scc` probe, two stores and two
/// counter increments, with no walk of the table. At 1 µs, the whole 15 k
/// fleet's 1,500 renewals/s costs ~0.15 % of one core, versus the 3.3 s/s
/// of serialized journal time the old plane needed (1,500 × 2.198 ms) —
/// i.e. structurally impossible before, ~free now.
///
/// **Falsification:** any of — (a) > 5 µs/renewal at 15 k members; (b) the
/// 15 k figure exceeding the 100-member figure by more than measurement
/// noise (that would mean renewal touches the whole table, so the 1,500/s
/// claim rests on an O(N) walk and 15 k is a cliff, not a point); (c)
/// nonzero `meta_kv_journal_entries` growth during the group (pinned as a
/// contract in `tests/dlm_membership_tests.rs`, priced here).
fn bench_renewal(c: &mut Criterion) {
    let mut group = c.benchmark_group("membership_renew");
    for members in [100usize, 1_500, 15_000] {
        let (owner, ids, _ticks) = owner_with(members);
        let mut cursor = 0usize;
        group.bench_with_input(BenchmarkId::from_parameter(members), &members, |b, _| {
            b.iter(|| {
                let (id, epoch) = &ids[cursor % ids.len()];
                cursor += 1;
                // Round-robin so the measurement is a fleet's access
                // pattern (every member beats), not one hot key.
                black_box(owner.renew(id, *epoch, 7))
            })
        });
    }
    group.finish();
}

/// **The read-side enumeration** — one census page out of a full
/// population, which is what `squeezefs clients` / `squeezefs status` / the
/// format preflight pay instead of `listxattr(1)` plus one `getxattr` per
/// client under a shared `I{1}` lock.
///
/// **Prediction:** ≤ 2 ms per 1,024-row page at 15,000 members, so a full
/// census is ≤ ~30 ms of RAM work and ZERO metadata-plane operations. The
/// per-page cost is deliberately **O(N log N)** today (the page filters and
/// sorts the live table by join sequence rather than maintaining an ordered
/// index), which is the honest trade for keeping the HOT path — renewal —
/// an unordered `scc` probe.
///
/// **Falsification:** a full 15 k census (15 pages) exceeding **100 ms**.
/// That is the threshold at which the operator-facing verb stops feeling
/// instant, and the fix is already known and scoped: keep a
/// `BTreeMap<join_seq, id>` index beside the table so a page is a range
/// scan instead of a sort. This bench exists to decide that with a number
/// rather than a preference.
fn bench_census_page(c: &mut Criterion) {
    let mut group = c.benchmark_group("membership_census_page");
    for members in [1_500usize, 15_000] {
        let (owner, _ids, _ticks) = owner_with(members);
        group.bench_with_input(BenchmarkId::from_parameter(members), &members, |b, _| {
            b.iter(|| {
                let (rows, next) = owner.census(0, squeezefs::membership_wire::CENSUS_PAGE_MAX);
                black_box((rows.len(), next))
            })
        });
    }
    group.finish();
}

/// **The §6.8 item-3 bound** — `min_acked_free_epoch`, which a freed-offset
/// grace period will call before reallocating an offset.
///
/// **Prediction:** O(members), ≤ 200 µs at 15,000 — cheap enough to call
/// per reclaim BATCH, and the reason item 3's design should batch rather
/// than ask per offset.
///
/// **Falsification:** > 1 ms at 15 k members, which would make a per-batch
/// call a visible term in the reclaim path and force a maintained minimum
/// (an incremental gauge updated on renewal) instead of a scan.
fn bench_free_epoch_bound(c: &mut Criterion) {
    let mut group = c.benchmark_group("membership_free_epoch_bound");
    for members in [1_500usize, 15_000] {
        let (owner, ids, _ticks) = owner_with(members);
        for (i, (id, epoch)) in ids.iter().enumerate() {
            owner.renew(id, *epoch, (i % 64) as u64);
        }
        group.bench_with_input(BenchmarkId::from_parameter(members), &members, |b, _| {
            b.iter(|| black_box(owner.min_acked_free_epoch()))
        });
    }
    group.finish();
}

/// **The member's per-beat client-side work**: derive the two clocks, adopt
/// a grant (compute `T_self` and the next renewal instant), and probe the
/// fail-stop deadline. A member does this once per beat, so at the shipped
/// 10 s cadence it must be invisible.
///
/// **Prediction:** clock derivation ≤ 1 µs (it reads knobs and the
/// checkpoint cadence), grant adoption ≤ 200 ns, and the `self_fence_due`
/// probe ≤ 20 ns (one atomic load plus a clock read) — so a member can
/// afford to check its own deadline far more often than it renews, which is
/// what makes the asymmetry operationally real rather than nominal.
///
/// **Falsification:** a `self_fence_due` probe above ~100 ns would mean a
/// member cannot cheaply poll its own deadline between renewals, and the
/// self-fence would have to become timer-driven — a schedule the fail-stop
/// path must not depend on.
fn bench_member_clock(c: &mut Criterion) {
    let mut group = c.benchmark_group("membership_member_clock");
    group.bench_function("derive_clocks", |b| {
        b.iter(|| black_box(LeaseClocks::derive(Duration::from_micros(250)).expect("derive")))
    });

    let cl = clocks();
    let grant = Grant {
        epoch: 42,
        term: 7,
        t_owner_ms: cl.t_owner.as_millis() as u64,
        skew_max_ms: cl.skew_max.as_millis() as u64,
        d_purge_ms: cl.d_purge.as_millis() as u64,
        renew_ms: cl.renew_interval.as_millis() as u64,
        granted_at_owner_ms: 1_000,
        lane_supply_blocks: 0,
    };
    let ticks = Arc::new(AtomicU64::new(1_000));
    let clock = LeaseClock::manual(Arc::clone(&ticks));
    group.bench_function("adopt_grant", |b| {
        b.iter(|| {
            black_box(MemberSession::adopt(
                "bench-member",
                MemberRole::Reader,
                &grant,
                1_000,
                clock.clone(),
            ))
        })
    });

    let session = MemberSession::adopt(
        "bench-member",
        MemberRole::Reader,
        &grant,
        1_000,
        clock.clone(),
    );
    group.bench_function("self_fence_due_probe", |b| {
        b.iter(|| black_box(session.self_fence_due()))
    });
    // Keep the tick word alive for the probe's clock reads.
    black_box(ticks.load(Ordering::Relaxed));
    group.finish();
}

/// **§6.2 item 7's record codec** — encode + decode of a claim set at 1
/// (the shipped single-writer posture, which in production is the
/// projection and writes NOTHING), 16 and 128 writer members.
///
/// **Prediction:** linear in members, ≤ 100 µs at 128. It rides a
/// membership CHANGE — a mount, a departure, a takeover — never a beat, so
/// even a millisecond would be immaterial; the bench exists to keep the
/// "never per beat" property honest by making the per-record cost visible
/// beside `membership_registration_commits`.
///
/// **Falsification:** super-linear growth in members (which would mean the
/// record's shape, not the plane, becomes the multi-writer scaling limit),
/// or any appearance of this cost in a renewal profile — that would mean
/// something started rewriting the record per beat, exactly the regression
/// S6 exists to prevent.
fn bench_claim_set_codec(c: &mut Criterion) {
    let mut group = c.benchmark_group("membership_claim_set_codec");
    for members in [1usize, 16, 128] {
        let mut set = ClaimSet::empty(9);
        for i in 0..members {
            set.members.push(ClaimSetMember {
                identity: MemberIdentity {
                    id: format!("writer-{i}-0123456789abcdef0123456789abcdef"),
                    role: MemberRole::Writer,
                    pid: 1000 + i as u32,
                    boot: "0123abcd-4567-89ef-0123-456789abcdef".to_string(),
                    endpoint: Some(format!("10.0.{}.{}:7100", i / 250, 1 + i % 250)),
                    pr_key: 0xdead_beef_0000 + i as u64,
                },
                ts: 1_770_000_000 + i as u64,
            });
        }
        let encoded = set.encode();
        group.bench_with_input(BenchmarkId::new("encode", members), &members, |b, _| {
            b.iter(|| black_box(set.encode().len()))
        });
        group.bench_with_input(BenchmarkId::new("decode", members), &members, |b, _| {
            b.iter(|| black_box(ClaimSet::decode(&encoded).expect("decode").members.len()))
        });
    }
    group.finish();
}

criterion_group!(
    benches,
    bench_renewal,
    bench_census_page,
    bench_free_epoch_bound,
    bench_member_clock,
    bench_claim_set_codec
);
criterion_main!(benches);
