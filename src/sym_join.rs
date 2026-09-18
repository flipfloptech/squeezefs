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
//! owner, a data WERO join, a membership shard). The MANY-writer posture —
//! N RW daemons on one set, N unbounded by design: every non-manager RW
//! daemon is a wire joiner whose `JoinAppender` / `AcquireSlots` travel to
//! a manager in another process and whose backend commits into a region
//! it does not manage, with its own ring, page writes, checkpoint task and
//! listener; "the second daemon" is only its first pin — is PR 12b (the
//! design's row 12b, three weeks); it is not in this rung (its rungs 5–6
//! over the wire refuse at the D0 gate, which names the posture), and the
//! note names it as the one piece of row 12 left standing.

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
    // The binding's WRITER half, the other direction (PR 6's owed "the
    // endpoint binding + the wire initiator's lock take"): the step shipper
    // a cross-owner op's foreign steps and travelling guards ride — the S8
    // client router over this set under the cluster secret — and, for
    // every appender the directory names Live, the endpoint its own ladder
    // published, bound into the slot holder table `step_home` reads. A
    // holder that joins AFTER this ladder is bound by PR 12b's wire
    // `JoinAppender` (the joiner's endpoint word at the verb), not by a
    // re-read here.
    install_step_shipper(meta, &report.endpoint).await;
    bind_live_appender_endpoints(meta).await;
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

/// **The ladder's rungs 4 and 7 on a JOINED appender** (PR 12b — the
/// many-writer posture): after `open_routed_meta_set_joined` walked rungs
/// 5–6 over the wire and the mount path joined the manager's membership
/// shard as a WRITER MEMBER (rung 3 — `membership::arm_joined_member`),
/// this stands up the same planes `arm` does on the manager — the data
/// WERO join (or the announced detection-grade posture), the custody
/// owner, the publish / meta / token services on ONE listener (never the
/// manager verbs: `arm_authority_planes` leaves them off a joiner) — then
/// PUBLISHES the listener through the manager (`PublishEndpoint` → the
/// joiner's claim-set entry on every volume + the manager's holder
/// table), installs the step shipper and binds every Live appender's
/// published endpoint. `mount_posture` reads `writer`; `manager_lease`
/// reads `peer:<manager>`.
pub async fn arm_joined(
    meta: &Arc<RoutedMetaBackend>,
    data_paths: &[PathBuf],
    quarantine: Option<Arc<dyn crate::data_grant::CustodyQuarantine>>,
    backend: Option<&Arc<crate::routing::BackendRouter>>,
) -> Result<(crate::multi_writer::MultiWriterArm, JoinReport)> {
    let mut report = JoinReport {
        rungs: vec!["declaration", "bits"],
        ..JoinReport::default()
    };
    check_bits(meta)?;
    if crate::membership::membership_mode() != "member" {
        return Err(SqueezefsError::InvalidOperation(format!(
            "symmetric join ladder rung 3 (membership) refuses on a joined appender: this mount \
             is no MEMBER of the manager's shard (membership_mode = {}) — a writer that cannot \
             be SEEN cannot be EVICTED, and the S6 eviction is what records its death for the \
             recovery driver (PR 10)",
            crate::membership::membership_mode()
        )));
    }
    report.rungs.push("membership");
    let pr_capable = data_paths
        .iter()
        .all(|p| crate::meta_backend::reservation::resolve_for_mount(p).is_some());
    let allow_non_pr = crate::env_knobs::bool_knob("SQUEEZEFS_SYM_ALLOW_NON_PR", false);
    // Rung 4's DATA half — NEVER an acquire (the manager holds; §5.8.1):
    // the door decided co-located-or-remote per volume off the manager's
    // flock and claim (one manager process per set, so the words agree)
    // and minted the set's one registrant key; a co-located joiner ADOPTS
    // the manager's standing data holds (KD-SYM-22 — the holder key
    // cross-checked against the claim set's enrolled writer keys), a
    // remote one REGISTERS under them with the same key its metadata
    // registration carries. The first build ran `arm_data_plane`'s
    // MultiWriter arm here — the manager's acquire — which under a
    // spec-strict target unregistered the manager's holder key through
    // the register ladder's own-stale proof and took the fence.
    let (colocated, registrant_key) = meta
        .volumes
        .iter()
        .filter_map(|v| v.joined_wire())
        .fold((true, 0u64), |(c, k), w| {
            (c && w.colocated, if k == 0 { w.registrant_key } else { k })
        });
    let wero = if pr_capable {
        let enrolled: Vec<u64> = match meta.volumes.first() {
            Some(first) => crate::membership::ClaimSet::load(first)
                .await
                .map(|set| set.registrant_keys())
                .unwrap_or_default(),
            None => Vec::new(),
        };
        let paths = data_paths.to_vec();
        let joined = squeezefs_ipc::sqz_blocking::run_blocking(move || {
            crate::data_custody::join_wero_as_appender(
                &paths,
                colocated,
                &enrolled,
                (registrant_key != 0).then_some(registrant_key),
            )
        })
        .await
        .map_err(|e| {
            SqueezefsError::InvalidOperation(format!(
                "symmetric join ladder rung 4 (registrant) refuses on a joined appender ({}): {e}",
                if colocated {
                    "co-located — adopting the manager's standing data holds"
                } else {
                    "remote — registering under the manager's standing data holds"
                }
            ))
        })?;
        report.data_namespaces_registered = data_paths.len();
        Some(joined.hold().clone())
    } else if allow_non_pr {
        report.detection_grade = true;
        log::warn!(
            "symmetric join ladder rung 4 (registrant), joined appender: {} data namespace(s) \
             advertise no NVMe reservation support — DETECTION-GRADE under \
             SQUEEZEFS_SYM_ALLOW_NON_PR=1 (KD-SYM-13)",
            data_paths.len()
        );
        None
    } else {
        return Err(SqueezefsError::InvalidOperation(
            "symmetric join ladder rung 4 (registrant) refuses on a joined appender: a data \
             namespace advertises no NVMe reservation support (design-symmetric-metadata \
             §5.8.1). Use a PR-capable namespace, or opt into the detection-grade lab posture \
             with SQUEEZEFS_SYM_ALLOW_NON_PR=1"
                .to_string(),
        ));
    };
    report
        .rungs
        .extend(["registrant", "join_appender", "acquire_slots"]);
    let prelude: Result<(
        std::net::SocketAddr,
        String,
        Arc<crate::meta_ship::OwnerMap>,
    )> = async {
        let Some(bind) = crate::multi_writer::resolve_bind_public()? else {
            return Err(SqueezefsError::InvalidOperation(format!(
                "symmetric join ladder rung 7 (planes) refuses on a joined appender: {}=off — \
                 every writer of a symmetric set serves its slots' tokens, custody and shipped \
                 steps. Give it `auto` (ruling D2) or an addr:port",
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
            "symmetric join ladder rung 7 (planes) refuses on a joined appender: the planes did \
             not arm"
                .to_string(),
        ));
    };
    report.rungs.push("planes");
    report.endpoint = arm.endpoint().to_string();
    // The binding's JOINER half (§5.1.6): the listener travels to the
    // manager, which writes it into this mount's claim-set entry and binds
    // it in its own holder table; every other Live appender's published
    // endpoint is bound here off the same durable state. The key is OURS
    // alone (0 when rung 4 adopted — the manager's key is never published
    // as a joiner's).
    let pr_key = crate::data_custody::own_registered_key().unwrap_or(0);
    for vol in &meta.volumes {
        if let Err(e) = vol.joined_publish_endpoint(&report.endpoint, pr_key).await {
            log::warn!(
                "symmetric join ladder rung 7 (joined appender): publishing the listener {} on \
                 {} failed ({e}) — peers resolve this holder as unbound until the next publish",
                report.endpoint,
                vol.device_path().display()
            );
        }
    }
    install_step_shipper(meta, &report.endpoint).await;
    bind_live_appender_endpoints(meta).await;
    install_report(report.clone());
    log::warn!(
        "SYMMETRIC WRITER JOINED as a NON-MANAGER appender (design-symmetric-metadata §7.3, PR \
         12b): rungs {:?}; {} data namespace(s) registered{}; serving on {}",
        report.rungs,
        report.data_namespaces_registered,
        if report.detection_grade {
            " (detection-grade)"
        } else {
            ""
        },
        report.endpoint
    );
    Ok((arm, report))
}

/// **The holder → endpoint binding, off DURABLE state** (§5.1.6 — "the
/// holder's identity → its endpoint from the membership census"): appender
/// `appender_id`'s directory page names its KD-MW-2 identity, the
/// identity is the member id every plane knows the node by
/// (`cowriter::node_member_id_of`), and the volume's durable claim set
/// carries that member's published listener — the endpoint the join
/// ladder's rung 7 writes into its own entry (`publish_owner_endpoint`).
/// `None` = no page, or the holder has not published (a joiner whose
/// ladder has not reached rung 7; a PR 4-era wire joiner). The one-shot
/// form — one directory read + one claim-set read, no wire — for a
/// single holder (the reader's per-slot binding); a caller with the
/// directory and the claim set in hand resolves through
/// [`resolve_holder_endpoint_from`] without re-reading either.
pub async fn resolve_holder_endpoint(
    vol: &crate::meta_backend::kv::backend::KvMetaBackend,
    appender_id: u32,
) -> Option<String> {
    let entries =
        crate::meta_backend::kv::appender::read_directory(vol.device_path(), vol.superblock())
            .await
            .ok()?;
    let page = entries
        .iter()
        .find(|e| e.appender_id == appender_id)
        .and_then(|e| e.page.as_ref())?;
    let set = crate::membership::ClaimSet::load(vol).await?;
    resolve_holder_endpoint_from(page, &set)
}

/// **A joined appender's holder venue for data volume `vol_tag`** (PR 12b
/// review round 1, Issue 2 — the DATA plane follows a manager failover):
/// the venue starts at the manager the join dialed and, after a transport
/// failure, RE-RESOLVES the allocation-lease holder's listener off durable
/// state — the set's volume 0 projection refreshed from the successor's
/// ledger record (`refresh_control_projection`), its `alloc_lease:{vol_tag}`
/// record's `holder_appender_id` on its `home_vol` (PR 8's gate: a
/// successor re-holds the lease at `term + 1` under its own page), and
/// that appender's published endpoint (`resolve_holder_endpoint`); an
/// unleased volume resolves to volume 0's manager (appender 0). The set is
/// held weakly — a venue outliving its set answers nothing and keeps the
/// last endpoint.
pub fn joined_holder_venue(
    routed: &Arc<crate::meta_backend::RoutedMetaBackend>,
    vol_tag: u64,
    initial_endpoint: String,
) -> Arc<crate::meta_backend::kv::alloc_lease::HolderVenue> {
    let weak = Arc::downgrade(routed);
    let resolver: crate::meta_backend::kv::alloc_lease::HolderVenueResolver = Arc::new(move || {
        let weak = weak.clone();
        Box::pin(async move {
            let routed = weak.upgrade()?;
            let (_, vol0) = crate::meta_backend::kv::backend::recovery::vol0_of(&routed)?;
            if let Err(e) = vol0.refresh_control_projection().await {
                log::debug!(
                    "data volume {vol_tag:#018x}: projection refresh before the holder \
                         venue's re-resolve failed ({e}) — reading the projection in hand"
                );
            }
            let (home, holder) = match vol0.alloc_lease_record(vol_tag).await {
                Ok(Some(rec)) => (
                    routed
                        .volumes
                        .get(usize::from(rec.home_vol))
                        .unwrap_or(vol0),
                    rec.holder_appender_id,
                ),
                Ok(None) => (vol0, 0),
                Err(e) => {
                    log::warn!(
                        "data volume {vol_tag:#018x}: its allocation-lease record is \
                             unreadable ({e}) — resolving the manager's venue"
                    );
                    (vol0, 0)
                }
            };
            resolve_holder_endpoint(home, holder).await
        })
    });
    crate::meta_backend::kv::alloc_lease::HolderVenue::resolved(initial_endpoint, resolver)
}

/// [`resolve_holder_endpoint`] over an already-read page and claim set —
/// the pure step both resolvers share: the page's KD-MW-2 identity → the
/// member id → that member's published listener (non-empty).
pub fn resolve_holder_endpoint_from(
    page: &crate::meta_backend::kv::appender::AppenderPage,
    set: &crate::membership::ClaimSet,
) -> Option<String> {
    let member =
        crate::cowriter::node_member_id_of(page.identity.node_token, page.identity.mount_slot);
    set.members
        .iter()
        .find(|m| crate::membership::member_id_matches(&m.identity.id, &member))
        .and_then(|m| m.identity.endpoint.clone())
        .filter(|e| !e.is_empty())
}

/// Install the cross-owner step shipper for this writer (rung 7): the S8
/// client router under the set's cluster secret, keyed by this node's
/// member id so a served step's scope names the initiator (`serve_guards_for`
/// compares `router.peer_id()` with the scope's client).
async fn install_step_shipper(meta: &Arc<RoutedMetaBackend>, endpoint: &str) {
    let Some(first) = meta.volumes.first() else {
        return;
    };
    let Some(secret) = crate::membership::cluster_secret(first).await else {
        log::warn!(
            "symmetric join ladder rung 7: no cluster secret on the set — the step shipper is \
             not installed (a foreign step is the un-shippable class the roll-forward cadence \
             retries); the S8 listener on {endpoint} still serves"
        );
        return;
    };
    let Ok(node_id) = crate::cowriter::node_member_id() else {
        return;
    };
    crate::meta_backend::crossvol_tx::install_xv_shipper(crate::meta_ship::MetaShipRouter::new(
        Arc::clone(meta),
        &node_id,
        secret,
    ));
}

/// Bind every Live appender's PUBLISHED endpoint into each volume's slot
/// holder table (rung 7's census binding, the writer side): the ONE table
/// `crossvol_tx::step_home` and `data_grant`'s slot-holder custody read.
/// Per volume the directory is read ONCE and the claim set ONCE — every
/// Live appender resolves off those two reads
/// ([`resolve_holder_endpoint_from`]), so a join costs two device reads
/// per volume whatever N is (review round 2, Issue 22: the first build
/// re-read both per appender). An appender whose ladder has not reached
/// its publish is left unbound (`StepHome::Unreachable` — the retryable
/// class), never guessed. Returns the bindings made.
pub async fn bind_live_appender_endpoints(meta: &Arc<RoutedMetaBackend>) -> usize {
    let mut bound = 0;
    for vol in &meta.volumes {
        let Some(plane) = vol.slot_leases() else {
            continue;
        };
        let own = vol.own_appender_id();
        let Ok(entries) =
            crate::meta_backend::kv::appender::read_directory(vol.device_path(), vol.superblock())
                .await
        else {
            continue;
        };
        let live: Vec<_> = entries
            .iter()
            .filter(|e| e.appender_id != own)
            .filter_map(|e| {
                e.page
                    .as_ref()
                    .filter(|p| p.state == crate::meta_backend::kv::appender::AppenderState::Live)
                    .map(|p| (e.appender_id, p))
            })
            .collect();
        if live.is_empty() {
            continue;
        }
        let Some(set) = crate::membership::ClaimSet::load(vol).await else {
            continue;
        };
        for (appender_id, page) in live {
            if let Some(endpoint) = resolve_holder_endpoint_from(page, &set) {
                plane.holders.set_endpoint(appender_id, &endpoint);
                bound += 1;
            }
        }
    }
    bound
}

/// Holder endpoints bound ON DEMAND (`sym_holder_binds_on_demand`): a
/// holder the rung-7 census did not know — an appender that joined AFTER
/// this mount's ladder ran — resolved off durable state at its first
/// foreign act. 0 on a solo mount and on every mount that joined last.
static HOLDER_BINDS_ON_DEMAND: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The on-demand holder bindings so far (`sym_holder_binds_on_demand`).
pub fn holder_binds_on_demand() -> u64 {
    HOLDER_BINDS_ON_DEMAND.load(std::sync::atomic::Ordering::Relaxed)
}

/// **The holder → endpoint binding ON DEMAND** (PR 12b, N ≥ 3): the slot
/// holder table knows every appender that was Live when this mount's
/// rung 7 ran ([`bind_live_appender_endpoints`]) and, on the manager,
/// every joiner whose `PublishEndpoint` it served — but an appender that
/// joined AFTER this mount's ladder is unknown to every OTHER joiner. Its
/// first foreign act here (a read of its slot, a shipped step, a custody
/// acquire, a travelling guard) resolves the endpoint off the SAME
/// durable state the census read — the page's identity, the claim set's
/// published listener ([`resolve_holder_endpoint`]; on a joiner the claim
/// set is read through the manager's tokens, so it is fresh) — and binds
/// it, once. A holder whose ladder has not published stays unbound (the
/// caller's retryable class). Returns the endpoint, bound or already
/// known.
pub async fn bind_holder_endpoint_on_demand(
    vol: &crate::meta_backend::kv::backend::KvMetaBackend,
    holder: u32,
) -> Option<Arc<str>> {
    let plane = vol.slot_leases()?;
    if let Some(e) = plane.holders.endpoint(holder) {
        return Some(e);
    }
    // A JOINED appender asks the manager (`ResolveEndpoint` — its table
    // is exact where this mount's claim-set projection is its open's);
    // the manager and a reader resolve off the durable state they read
    // fresh.
    let endpoint = if vol.is_joined_appender() {
        match vol.joined_resolve_endpoint(holder).await {
            Ok(e) => e?,
            Err(e) => {
                log::warn!(
                    "meta volume {}: ResolveEndpoint for appender {holder} failed ({e}) — the \
                     holder stays unbound for this act",
                    vol.device_path().display()
                );
                return None;
            }
        }
    } else {
        resolve_holder_endpoint(vol, holder).await?
    };
    plane.holders.set_endpoint(holder, &endpoint);
    HOLDER_BINDS_ON_DEMAND.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    log::info!(
        "meta volume {}: appender {holder}'s endpoint {endpoint} bound ON DEMAND (it joined after \
         this mount's ladder ran)",
        vol.device_path().display()
    );
    plane.holders.endpoint(holder)
}

/// **Re-resolve a holder whose bound endpoint went DEAD** (PR 12b, N ≥ 3):
/// a dead holder's SUCCESSOR at the same identity rejoins its region
/// (same appender id) and publishes a NEW listener, while every peer's
/// table still names the old one — the peer's dials fail for ever. On a
/// dial failure at `stale` the caller asks here: the endpoint is resolved
/// FRESH (past the table — a joiner asks the manager, the manager and a
/// reader read the durable state the successor's publish wrote) and, when
/// it MOVED, bound in place and returned; the same address (the holder is
/// simply down) answers `None` and the caller keeps its retry class.
/// Counted with the on-demand bindings.
pub async fn rebind_holder_endpoint_if_moved(
    vol: &crate::meta_backend::kv::backend::KvMetaBackend,
    holder: u32,
    stale: &str,
) -> Option<Arc<str>> {
    let plane = vol.slot_leases()?;
    let fresh = if vol.is_joined_appender() {
        vol.joined_resolve_endpoint(holder).await.ok().flatten()?
    } else {
        resolve_holder_endpoint(vol, holder).await?
    };
    if fresh == stale {
        return None;
    }
    plane.holders.set_endpoint(holder, &fresh);
    HOLDER_BINDS_ON_DEMAND.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    log::info!(
        "meta volume {}: appender {holder} MOVED its listener {stale} → {fresh} (a successor at \
         the same identity) — re-bound",
        vol.device_path().display()
    );
    plane.holders.endpoint(holder)
}

static REPORT: arc_swap::ArcSwapOption<JoinReport> = arc_swap::ArcSwapOption::const_empty();

fn install_report(report: JoinReport) {
    REPORT.store(Some(Arc::new(report)));
}

/// Forget the report and the step shipper (the leave / test teardown) —
/// the planes the ladder stood up leave together.
pub fn clear_report() {
    REPORT.store(None);
    crate::meta_backend::crossvol_tx::uninstall_xv_shipper();
}

/// The ladder's report for the stats inode (`symmetric_join`), `None`
/// where no ladder ran.
pub fn report() -> Option<Arc<JoinReport>> {
    REPORT.load_full()
}
