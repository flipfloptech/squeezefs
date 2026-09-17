//! **The symmetric JOIN LADDER** — every RW mount of an ARMED symmetric set
//! is a writer (docs/design-symmetric-metadata.md §7.3, §6.1, §6.2, §8
//! gate 1; PR 12 — `feat/sym-mount-posture`).
//!
//! The co-writer's five admission rungs and the per-volume owner's seven
//! become ONE ladder a plain `mount` walks when `SQUEEZEFS_SYMMETRIC_META=1`
//! names a bit-17 set — no role knob, no authority endpoint, no roster,
//! no offline assignment:
//!
//! | Rung | What | Where it runs | Refuses naming |
//! |---|---|---|---|
//! | 1 **declaration** | the knob, and NONE of the retired posture knobs beside it | [`retired_knob_refusal`] at the top of the mount path, before any volume is opened | the retired knob and its successor (the plane) |
//! | 2 **bits** | every volume carries 7/9/10/11/13/14/15/16 **and 17** | [`check_bits`] right after the routed open | the volume, the bit, the verb that stamps it |
//! | 3 **membership** | this mount is a member of its HOME shard — the shard's S6 owner when it holds the manager lease | the mount path's membership arm, at `auto` when the operator declared no bind | `SQUEEZEFS_MEMBERSHIP_BIND` |
//! | 4 **registrant** | WERO on the metadata namespaces (PR 3's `meta_wero`, taken at the open — since PR 12 for the knob-armed shape too) AND the data namespaces (S7's hold, joined here); the registrant cap is probed on a registrant JOIN (PR 3's `join_wero_as_registrant`, PR 12b's joiner path) — the solo HOLDER's own REGISTER learns it (`pr_registrant_cap`) | [`arm`] | the namespace, the cap, `SQUEEZEFS_SYM_ALLOW_NON_PR` |
//! | 5 **`JoinAppender`** | the region goes `Live` under this mount's identity (PR 2's `join_appender_regions`, at the open) | the open | the page, PR 10's driver |
//! | 6 **`AcquireSlots`** | the native slot + `M` rotor slots (PR 4's arm, at the open) | the open | the manager |
//! | 7 **the planes** | the custody owner, the publish/meta/manager/token services on ONE listener, the ownership plane, the cadence — what a co-writer used to dial and what a solo mount never stood up | [`arm`] → `multi_writer::arm_authority_planes` | `SQUEEZEFS_MW_BIND=off` |
//!
//! `mount_posture` reads `writer` on every RW mount of an armed set; WHICH
//! leases it holds is what the role gauges say (`manager_lease`,
//! `alloc_lease`, `slot_leases_held`, `membership_mode`). The shipped
//! roles — authority / set-authority / partial-authority / co-writer — and
//! their knobs CEASE on an armed mount: they are refused as RETIRED
//! spellings (the `Kind::Enum { retired }` law PR 16 introduced, applied
//! conditionally, because the SAME knobs keep their shipped meaning on an
//! UNARMED mount until the PR-14 flip deletes them — the byte-identical law
//! for `=0` and every bit-17-absent volume).
//!
//! **What this rung delivers, and what it states** (the note's ledger):
//! the ladder as built is walked in full by the SOLO armed mount — the
//! shape every in-process fixture and the fleet rig reach — and its rungs
//! 3/4/7 are what a plain armed mount never had (a listener, a custody
//! owner, a data WERO join, a membership shard). A SECOND RW daemon on the
//! same volume — the wire joiner whose `JoinAppender` / `AcquireSlots`
//! travel to a manager in another process and whose backend commits into a
//! region it does not manage — is the N-daemon backend posture the design's
//! row 12 costs at three weeks; it is not in this rung (its rungs 5–6 over
//! the wire refuse at the D0 gate exactly as before), and the note names it
//! as the one piece of row 12 left standing.

use crate::error::{Result, SqueezefsError};
use crate::meta_backend::RoutedMetaBackend;
use std::path::PathBuf;
use std::sync::Arc;

/// The posture knobs that CEASE on an armed mount, each with the successor
/// the refusal names. `SQUEEZEFS_MW_BIND` is deliberately NOT here: under
/// the plane every writer serves, and the bind is where (§6.1 retires the
/// four below; `off` on an armed mount is refused by rung 7 instead).
pub const RETIRED_ON_ARMED: &[(&str, &str)] = &[
    (
        "SQUEEZEFS_MULTI_WRITER",
        "every RW mount of a symmetric set is a writer by the join ladder \
         (design-symmetric-metadata §7.3) — the plane arms the device-enforced class itself",
    ),
    (
        "SQUEEZEFS_MW_ROLE",
        "there are no roles under the symmetric plane: `mount_posture` reads `writer` on \
         every RW mount and the lease gauges say what it holds (§11)",
    ),
    (
        "SQUEEZEFS_MW_AUTHORITY",
        "there is no set authority to dial: a foreign object's holder is resolved through \
         tree 0's lessee and the membership census (§5.1.6)",
    ),
    (
        "SQUEEZEFS_MW_MEMBERS",
        "no roster: a writer is enrolled by joining (the membership lease is its \
         admission, §7.3)",
    ),
];

/// `Some(refusal)` when the plane is requested AND a retired posture knob
/// is set beside it — the ladder's rung 1, phrased in the registry's own
/// retired-spelling form so an operator reads ONE law. `None` on every
/// unarmed process (the knobs keep their shipped meaning there), so the
/// refusal fires only where the successor exists.
pub fn retired_knob_refusal() -> Option<String> {
    if !crate::meta_backend::kv::slot_lease::symmetric_meta_requested() {
        return None;
    }
    let mut lines = Vec::new();
    for (key, successor) in RETIRED_ON_ARMED {
        let Ok(raw) = std::env::var(key) else {
            continue;
        };
        let raw = raw.trim();
        if raw.is_empty() {
            continue;
        }
        // The BOOL knob at an OFF spelling (`0` / `false` / `no` / `off`)
        // asks for nothing: the knob law reads it as DISABLED, i.e. absent
        // — a fleet env that writes the shipped default out declares no
        // posture (review round 1, Issue 19). A malformed value never
        // reaches here (the registry gate refused the process first). The
        // enum / string knobs have no off spelling: `authority` is a role.
        if *key == "SQUEEZEFS_MULTI_WRITER" && !crate::env_knobs::bool_knob(key, false) {
            continue;
        }
        lines.push(format!(
            "{key}='{raw}' was RETIRED on a symmetric mount (forward-only — never a silent \
             alias): {successor}"
        ));
    }
    if lines.is_empty() {
        return None;
    }
    let mut s = String::from(
        "refusing to mount: SQUEEZEFS_SYMMETRIC_META=1 with retired posture knob(s) — the \
         symmetric join ladder replaces the multi-writer postures (design-symmetric-metadata \
         §6.1 / §7.3; PR 12)\n",
    );
    for l in &lines {
        s.push_str("  - ");
        s.push_str(l);
        s.push('\n');
    }
    s.push_str(
        "  (unset them — SQUEEZEFS_MULTI_WRITER at an OFF spelling (`0`/`false`/`no`/`off`) is \
         the absent knob and is admitted; on an UNARMED mount — SQUEEZEFS_SYMMETRIC_META unset \
         — they keep their shipped meaning until the PR-14 flip)",
    );
    Some(s)
}

/// The capability bits the ladder's rung 2 demands on EVERY volume: the
/// multi-writer class (7/9/10/11/13/14/15/16 — `multi_writer`'s
/// `REQUIRED_INCOMPAT`) plus the forest (17). `format --symmetric` stamps
/// all of them; `enable-symmetric` converts a multi-writer-class set.
pub fn required_bits() -> u64 {
    crate::multi_writer::REQUIRED_INCOMPAT
        | crate::meta_backend::kv::superblock::FEATURE_INCOMPAT_KV_SYMMETRIC_FOREST
}

/// Rung 2: every volume of the set carries [`required_bits`], else the
/// refusal names the volume, the bit and the act that stamps it.
pub fn check_bits(meta: &Arc<RoutedMetaBackend>) -> Result<()> {
    if meta.volumes.is_empty() {
        return Err(SqueezefsError::InvalidOperation(
            "symmetric join ladder rung 2 (bits) refuses: the metadata set has no volumes"
                .to_string(),
        ));
    }
    let want = required_bits();
    for vol in &meta.volumes {
        let have = vol.superblock().features_incompat;
        let missing = want & !have;
        if missing != 0 {
            let bit = missing.trailing_zeros();
            let remedy = if bit == 17 {
                "`squeezefs volume enable-symmetric <sqmeta-uri>` (offline) or `format --symmetric`"
            } else {
                "`squeezefs volume enable-multi-writer <sqmeta-uri>` (offline) or a default \
                 `format` (multi-writer-capable since the rung-10b flip)"
            };
            return Err(SqueezefsError::InvalidOperation(format!(
                "symmetric join ladder rung 2 (bits) refuses: metadata volume {} does not carry \
                 incompat bit {bit} (missing mask {missing:#x} of the required {want:#x}) — the \
                 plane presumes the whole multi-writer class plus the forest on EVERY volume. \
                 Stamp it with {remedy}",
                vol.device_path().display(),
            )));
        }
    }
    Ok(())
}

/// Whether the set this process opened is ARMED (the plane on at least one
/// volume) — the ladder's trigger. `false` on every unarmed and every
/// bit-17-absent mount, where the shipped postures run verbatim.
pub fn set_armed(meta: &RoutedMetaBackend) -> bool {
    meta.volumes.iter().any(|v| v.slot_lease_armed())
}

/// What the ladder armed on this mount (the role gauges' source).
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct JoinReport {
    /// The rungs the ladder REQUIRED and passed, named in the DESIGN's
    /// numbering (§7.3: declaration, bits, membership, registrant,
    /// join_appender, acquire_slots, planes) — a checklist, not a trace:
    /// rungs 5–6 run inside the routed OPEN, before rung 3 (the mount
    /// path) and before rungs 2 / 4 / 7 (`arm`). Every rung is required,
    /// so the published list is the full set or the report does not exist
    /// (a refused ladder installs none).
    pub rungs: Vec<&'static str>,
    /// The data namespaces registered under the WERO hold (0 on the
    /// detection-grade lab posture).
    pub data_namespaces_registered: usize,
    /// Detection-grade: `SQUEEZEFS_SYM_ALLOW_NON_PR=1` stood in for the
    /// device fence on the data namespaces.
    pub detection_grade: bool,
    /// The S8 listener every peer reaches this writer on.
    pub endpoint: String,
}

/// Whether `SQUEEZEFS_MEMBERSHIP_BIND` is EXPLICITLY `off` — the knob law's
/// distinction rung 3 turns on: `membership::resolve_bind` folds unset and
/// `off` into one word because the S6 plane's own default IS off, but the
/// ladder's default is `auto`, so here the two must be told apart.
fn membership_bind_explicitly_off() -> bool {
    std::env::var(MEMBERSHIP_BIND_ENV)
        .map(|v| v.trim().eq_ignore_ascii_case("off"))
        .unwrap_or(false)
}

/// The membership plane's bind knob (S6's; the ladder's rung 3 reads it).
pub const MEMBERSHIP_BIND_ENV: &str = "SQUEEZEFS_MEMBERSHIP_BIND";

/// Rung 3 (the membership shard): arm the S6 plane on an armed set when
/// the operator declared no bind — the death ledger's writer is the
/// owner's eviction (PR 10), so an armed set without membership would
/// record no death. The knob law (ENG-10 — explicit wins verbatim, never a
/// silent override): `SQUEEZEFS_MEMBERSHIP_BIND` UNSET is the ladder's
/// `auto`; an explicit `auto` / `addr:port` is the mount path's own arm
/// (armed before the ladder runs — this returns `Ok(None)`); an EXPLICIT
/// `off` is REFUSED here naming the knob — a writer that cannot be seen
/// cannot be evicted, and an operator who wrote `off` asked for exactly
/// the posture the plane cannot run under (review round 1, Issue 2: the
/// first build armed it at `auto` on `0.0.0.0:0` over the operator's word).
pub async fn arm_membership_shard(
    meta: &Arc<RoutedMetaBackend>,
    on_purge: Option<Arc<dyn Fn() + Send + Sync>>,
) -> Result<Option<crate::membership::MembershipArm>> {
    if crate::membership::membership_mode() != "off" {
        return Ok(None);
    }
    if membership_bind_explicitly_off() {
        return Err(SqueezefsError::InvalidOperation(format!(
            "symmetric join ladder rung 3 (membership) refuses: {MEMBERSHIP_BIND_ENV}=off was \
             DECLARED on an armed set — a writer that cannot be SEEN cannot be EVICTED, and the \
             S6 eviction is what records a death for the recovery driver (PR 10). The ladder \
             arms the shard at `auto` only when the knob is UNSET (explicit wins verbatim, \
             ENG-10); unset it, or give it `auto` / an addr:port"
        )));
    }
    let arm = crate::membership::arm_mount_membership_at(
        meta,
        false,
        on_purge,
        crate::membership::MembershipBind::Auto,
    )
    .await?;
    if arm.is_none() {
        return Err(SqueezefsError::InvalidOperation(
            "symmetric join ladder rung 3 (membership) refuses: the membership shard did not \
             arm — the volume set carries no `job:enroll` record, this plane's root of trust \
             (possession of volume access IS cluster membership, ruling D2). Enable the cluster \
             listener (SQUEEZEFS_JOB_WIRE_BIND) so the secret exists"
                .to_string(),
        ));
    }
    Ok(arm)
}

/// Rungs 4 and 7 — after the open joined the appender region and acquired
/// its slots (rungs 5–6) and the membership shard armed (rung 3): join the
/// data namespaces' WERO hold (or announce the detection-grade posture the
/// lab opt-in stands in with), then stand up the planes every writer
/// serves. `Ok(None)` on an unarmed set — the shipped posture exactly.
pub async fn arm(
    meta: &Arc<RoutedMetaBackend>,
    data_paths: &[PathBuf],
    quarantine: Option<Arc<dyn crate::data_grant::CustodyQuarantine>>,
    backend: Option<&Arc<crate::routing::BackendRouter>>,
) -> Result<Option<(crate::multi_writer::MultiWriterArm, JoinReport)>> {
    if !set_armed(meta) {
        return Ok(None);
    }
    let mut report = JoinReport {
        rungs: vec!["declaration", "bits"],
        ..JoinReport::default()
    };
    check_bits(meta)?;
    if crate::membership::membership_mode() == "off" {
        return Err(SqueezefsError::InvalidOperation(
            "symmetric join ladder rung 3 (membership) refuses: the membership plane is off — \
             a writer that cannot be SEEN cannot be EVICTED, and the S6 eviction is what records \
             a death for the recovery driver (PR 10). The ladder arms it at `auto` when \
             SQUEEZEFS_MEMBERSHIP_BIND is unset; an explicit `off` is refused"
                .to_string(),
        ));
    }
    report.rungs.push("membership");

    // Rung 4: the registrant. PR 3 took the METADATA half at the open
    // (`meta_wero`); the DATA half is S7's standing hold, and on a
    // substrate without reservations the SAME lab opt-in that admitted the
    // open stands in (KD-SYM-13: detection-grade, announced).
    let pr_capable = data_paths
        .iter()
        .all(|p| crate::meta_backend::reservation::resolve_for_mount(p).is_some());
    let allow_non_pr = crate::env_knobs::bool_knob("SQUEEZEFS_SYM_ALLOW_NON_PR", false);
    let wero = if pr_capable {
        let paths = data_paths.to_vec();
        let hold = squeezefs_ipc::sqz_blocking::run_blocking(move || {
            crate::data_custody::arm_data_plane(
                crate::data_custody::CustodyPosture::MultiWriter,
                &paths,
                true,
            )
        })
        .await
        .map_err(|e| {
            SqueezefsError::InvalidOperation(format!(
                "symmetric join ladder rung 4 (registrant) refuses: {e}"
            ))
        })?;
        report.data_namespaces_registered = data_paths.len();
        hold
    } else if allow_non_pr {
        report.detection_grade = true;
        log::warn!(
            "symmetric join ladder rung 4 (registrant): {} data namespace(s) advertise no NVMe \
             reservation support — DETECTION-GRADE under SQUEEZEFS_SYM_ALLOW_NON_PR=1 (the lab \
             opt-in; design-symmetric-metadata §5.8.1: a fenced writer's DMA is detected, never \
             rejected, on this substrate)",
            data_paths.len()
        );
        None
    } else {
        return Err(SqueezefsError::InvalidOperation(
            "symmetric join ladder rung 4 (registrant) refuses: a data namespace advertises no \
             NVMe reservation support, so this writer's DMA could only be DETECTED after a \
             fence, never rejected by the device (design-symmetric-metadata §5.8.1). Use a \
             PR-capable namespace, or opt into the detection-grade lab posture with \
             SQUEEZEFS_SYM_ALLOW_NON_PR=1"
                .to_string(),
        ));
    };
    report
        .rungs
        .extend(["registrant", "join_appender", "acquire_slots"]);

    // Rung 7: the planes. The bind is the operator's word or D2's posture;
    // `off` is not a posture for a writer that serves. EVERY refusal of the
    // prelude releases the rung-4 hold OFF the runtime (its ioctls are
    // blocking — the declared arm's law on each of its paths); a `?` here
    // would drop the hold at scope exit and run the release inline on the
    // `sqz-meta` lane (review round 1, Issue 11).
    let prelude: Result<(
        std::net::SocketAddr,
        String,
        Arc<crate::meta_ship::OwnerMap>,
    )> = async {
        let Some(bind) = crate::multi_writer::resolve_bind_public()? else {
            return Err(SqueezefsError::InvalidOperation(format!(
                "symmetric join ladder rung 7 (planes) refuses: {}=off — every writer of a \
                 symmetric set serves its slots' tokens, custody and shipped steps, so a \
                 writer with no listener is a misconfiguration, not a posture. Give it `auto` \
                 (ruling D2) or an addr:port",
                crate::multi_writer::MW_BIND_ENV
            )));
        };
        let node_id = crate::cowriter::node_member_id()?;
        let map = crate::multi_writer::derive_symmetric_ownership(meta, &node_id).await?;
        Ok((bind, node_id, map))
    }
    .await;
    let (bind, node_id, map) = match prelude {
        Ok(v) => v,
        Err(e) => {
            if let Some(hold) = wero {
                crate::data_custody::release_hold(hold).await;
            }
            return Err(e);
        }
    };
    let arm = crate::multi_writer::arm_authority_planes(
        meta,
        wero,
        bind,
        Vec::new(),
        quarantine,
        backend,
        map,
        node_id,
    )
    .await?;
    let Some(arm) = arm else {
        return Err(SqueezefsError::InvalidOperation(
            "symmetric join ladder rung 7 (planes) refuses: the authority planes did not arm"
                .to_string(),
        ));
    };
    report.rungs.push("planes");
    report.endpoint = arm.endpoint().to_string();
    // The binding's WRITER half (§5.1.6): this writer's listener into its
    // own claim-set entry on every volume it appends to — what
    // `resolve_holder_endpoint` reads for its appender id on any mount
    // (a reader's per-slot planes, a peer's shipped steps), no knob.
    crate::multi_writer::publish_symmetric_endpoint(meta, &report.endpoint).await;
    install_report(report.clone());
    log::warn!(
        "SYMMETRIC WRITER JOINED (design-symmetric-metadata §7.3): rungs {:?}; {} data \
         namespace(s) registered{}; serving on {}",
        report.rungs,
        report.data_namespaces_registered,
        if report.detection_grade {
            " (detection-grade)"
        } else {
            ""
        },
        report.endpoint
    );
    Ok(Some((arm, report)))
}

/// **The holder → endpoint binding, off DURABLE state** (§5.1.6 — "the
/// holder's identity → its endpoint from the membership census"): appender
/// `appender_id`'s directory page names its KD-MW-2 identity, the
/// identity is the member id every plane knows the node by
/// (`cowriter::node_member_id_of`), and the volume's durable claim set
/// carries that member's published listener — the endpoint the join
/// ladder's rung 7 writes into its own entry (`publish_owner_endpoint`).
/// `None` = no page, or the holder has not published (a joiner whose
/// ladder has not reached rung 7; a PR 4-era wire joiner). One directory
/// read + one claim-set read, no wire.
pub async fn resolve_holder_endpoint(
    vol: &crate::meta_backend::kv::backend::KvMetaBackend,
    appender_id: u32,
) -> Option<String> {
    let entries =
        crate::meta_backend::kv::appender::read_directory(vol.device_path(), vol.superblock())
            .await
            .ok()?;
    let page = entries
        .into_iter()
        .find(|e| e.appender_id == appender_id)
        .and_then(|e| e.page)?;
    let member =
        crate::cowriter::node_member_id_of(page.identity.node_token, page.identity.mount_slot);
    let set = crate::membership::ClaimSet::load(vol).await?;
    set.members
        .iter()
        .find(|m| crate::membership::member_id_matches(&m.identity.id, &member))
        .and_then(|m| m.identity.endpoint.clone())
        .filter(|e| !e.is_empty())
}

static REPORT: arc_swap::ArcSwapOption<JoinReport> = arc_swap::ArcSwapOption::const_empty();

fn install_report(report: JoinReport) {
    REPORT.store(Some(Arc::new(report)));
}

/// Forget the report (the leave / test teardown).
pub fn clear_report() {
    REPORT.store(None);
}

/// The ladder's report for the stats inode (`symmetric_join`), `None`
/// where no ladder ran.
pub fn report() -> Option<Arc<JoinReport>> {
    REPORT.load_full()
}
