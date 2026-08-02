//! DLM S2 — durable `WriterClaim.term` + composed fencing tokens
//! (spec §6.7 decision 4, §6.9 stage S2, §6.11; incompat bit 7).
//!
//! S1 made the mint one process-global `grant_seq`. It is still **in
//! RAM**, so it restarts at 0 in every process: a record stamped 5
//! before a crash compares `5 < 0` against the fresh mount's read and is
//! ADOPTED as current (spec §6.11). S2 gives the mint a durable
//! high-order component — the writer term — so that a successor's every
//! grant strictly exceeds every grant any predecessor ever issued:
//!
//! ```text
//! token = (term << 40) | grant_seq        // 24 bits term, 40 bits seq
//! ```
//!
//! Contracts pinned here (the §6.11 remount repro itself lives in
//! `tests/fencing_remount_tests.rs`, which S2 un-ignores):
//!
//! 1. **Composition + bit budget** — the layout, the exhaustion
//!    constants, and the monotonicity the ~24-site census depends on
//!    (`<`, `==`, `.max()` all stay monotone-safe).
//! 2. **Durability** — the term is a field on the `writer_claim` record
//!    AND a never-deleted `writer_term` record beside it; it strictly
//!    increases on every claim acquisition, including across the CLEAN
//!    unmount that deletes the claim.
//! 3. **Compatibility** — a volume without incompat bit 7 stays at term
//!    0 (composed token ≡ the S1 token, claim bytes unchanged, no
//!    `writer_term` record); stamping the bit engages the machinery.
//! 4. **Publication** — a mount publishes its term to the process mint,
//!    so a fresh process's reads and grants dominate every prior era.
//!    This is what un-vacuums the offline verbs (`fsck`, `defrag`,
//!    `clone`, `config`), which read `get_fencing_token_ino` in a
//!    process whose map is empty — `0 < 0` today.
//! 5. **Exhaustion is loud** — a term-space rollover refuses the MOUNT;
//!    a grant-seq rollover refuses the MINT. Never a silent wrap into
//!    the neighbouring field.

use squeezefs::dlm::{
    self, compose_token, token_grant_seq, token_term, DlmClient, GRANT_SEQ_BITS, GRANT_SEQ_MAX,
    TERM_MAX,
};
use squeezefs::meta_backend::kv::backend::{
    KvMetaBackend, WriterClaim, WRITER_CLAIM_XATTR, WRITER_TERM_XATTR,
};
use squeezefs::meta_backend::kv::builder::{format_v3, FormatV3Options};
use squeezefs::meta_backend::kv::superblock::{
    classify_volume, set_durable_term_bit, write_superblock_v3, VolumeFormat,
    FEATURES_INCOMPAT_KNOWN, FEATURE_INCOMPAT_KV_DURABLE_TERM,
};
use squeezefs::meta_backend::Metadata;
use std::sync::OnceLock;
use std::time::Duration;
use tempfile::NamedTempFile;

const VOL_LEN: u64 = 64 * 1024 * 1024;

static SERIAL: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();

async fn serial() -> tokio::sync::MutexGuard<'static, ()> {
    SERIAL
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await
}

fn opts() -> FormatV3Options {
    FormatV3Options {
        node_size: 64 * 1024,
        journal_len_override: Some(1024 * 1024),
        force: false,
        full_wipe: false,
        format_config_xattr: None,
    }
}

async fn fresh_volume() -> NamedTempFile {
    let f = NamedTempFile::new().unwrap();
    f.as_file().set_len(VOL_LEN).unwrap();
    format_v3(f.path(), VOL_LEN, &opts()).await.unwrap();
    f
}

/// Strip incompat bit 7 from a freshly-formatted volume: the on-disk
/// shape of every volume formatted BEFORE S2 (the Phase-8 reformat
/// window has not run — bit 7 is never stamped at mount).
async fn unstamp_durable_term(path: &std::path::Path) {
    let VolumeFormat::V3(mut sb) = classify_volume(path).await.unwrap() else {
        panic!("expected v3");
    };
    assert_ne!(
        sb.features_incompat & FEATURE_INCOMPAT_KV_DURABLE_TERM,
        0,
        "a fresh format must carry bit 7 (the stamping path S2 ships)"
    );
    sb.features_incompat &= !FEATURE_INCOMPAT_KV_DURABLE_TERM;
    write_superblock_v3(path, &sb).await.unwrap();
}

async fn raw_ino1_xattr(be: &KvMetaBackend, name: &str) -> Option<Vec<u8>> {
    Metadata::getxattr(be, 1, name).await.unwrap()
}

/// 1. The composition (spec §6.7 decision 4): 24 bits of term over 40
/// bits of grant_seq, decodable both ways, with the exhaustion bounds
/// named as constants (a rollover must be a refusal, never a wrap).
#[test]
fn composed_token_layout_is_24_bit_term_over_40_bit_grant_seq() {
    assert_eq!(GRANT_SEQ_BITS, 40, "spec §6.7: (term << 40) | grant_seq");
    assert_eq!(GRANT_SEQ_MAX, (1u64 << 40) - 1, "40-bit grant space");
    assert_eq!(TERM_MAX, (1u64 << 24) - 1, "24-bit term space");

    let t = compose_token(3, 7);
    assert_eq!(t, (3u64 << 40) | 7);
    assert_eq!(token_term(t), 3);
    assert_eq!(token_grant_seq(t), 7);

    // The corner values round-trip without bleeding into each other.
    let corner = compose_token(TERM_MAX, GRANT_SEQ_MAX);
    assert_eq!(token_term(corner), TERM_MAX);
    assert_eq!(token_grant_seq(corner), GRANT_SEQ_MAX);
    assert_eq!(corner, u64::MAX, "the two fields tile the whole word");
    assert_eq!(token_term(0), 0);
    assert_eq!(token_grant_seq(0), 0);
}

/// 1b. Monotonicity — the property every census site rides: a later
/// term's FIRST grant beats a previous term's LAST grant, so `<` fences
/// reject stale eras, `==` coherence memos can never falsely match
/// across eras, and `.max()` folds stay order-safe.
#[test]
fn composed_tokens_stay_monotone_across_terms_and_grants() {
    for term in [0u64, 1, 2, 4095, TERM_MAX - 1] {
        assert!(
            compose_token(term, GRANT_SEQ_MAX) < compose_token(term + 1, 0),
            "term {term}'s last grant must be < term {}'s first",
            term + 1
        );
        assert!(compose_token(term, 1) < compose_token(term, 2));
        // Distinct eras are distinct VALUES (the `==` memo arm).
        assert_ne!(compose_token(term, 5), compose_token(term + 1, 5));
    }
    // Term 0 (an un-stamped volume) composes to the bare S1 token —
    // byte-identical fencing values for pre-S2 volumes.
    assert_eq!(compose_token(0, 42), 42);
}

/// 2a. The record: `term` is a durable field on `writer_claim`, and a
/// pre-S2 record (no `term` key) decodes as term 0 — the additive-JSON
/// compatibility law the `job_endpoint` field already established.
#[test]
fn writer_claim_term_round_trips_and_legacy_records_decode_as_term_zero() {
    let claim = WriterClaim {
        id: "w-1".to_string(),
        ts: 1_700_000_000,
        pid: 4242,
        boot: "boot-abc".to_string(),
        term: 9,
    };
    let decoded = WriterClaim::decode(&claim.encode()).expect("round-trip");
    assert_eq!(decoded, claim, "term must survive encode/decode");

    let legacy = br#"{"id":"w-0","ts":1700000000,"pid":7,"boot":"boot-abc"}"#;
    let decoded = WriterClaim::decode(legacy).expect("legacy record decodes");
    assert_eq!(decoded.term, 0, "a pre-S2 claim carries term 0");

    // ... and a term-0 claim encodes byte-identically to the pre-S2
    // record: an un-stamped volume's bytes never change.
    let zero = WriterClaim {
        id: "w-0".to_string(),
        ts: 1_700_000_000,
        pid: 7,
        boot: "boot-abc".to_string(),
        term: 0,
    };
    assert_eq!(
        String::from_utf8(zero.encode()).unwrap(),
        String::from_utf8(legacy.to_vec()).unwrap(),
        "term 0 must not change the on-disk claim bytes"
    );
}

/// 2b. The durability contract: every successful claim acquisition bumps
/// the term, and the bump SURVIVES the clean unmount that deletes the
/// claim (the `writer_term` record is never deleted — otherwise a
/// mount/unmount cycle would reset the fencing era and re-issue tokens a
/// crashed predecessor already used).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn durable_term_strictly_increases_on_every_claim_acquisition() {
    let _g = serial().await;
    let vol = fresh_volume().await;

    // Mount 1: a volume with no claim starts the era ladder at 1.
    let be = KvMetaBackend::open(vol.path()).await.expect("mount 1");
    assert_eq!(
        be.writer_term(),
        1,
        "first claim on a fresh volume = term 1"
    );
    let claim = be.read_writer_claim().await.expect("claim committed");
    assert_eq!(claim.term, 1, "the claim record carries the term");
    assert!(
        raw_ino1_xattr(&be, WRITER_TERM_XATTR).await.is_some(),
        "the durable term record rides beside the claim"
    );
    drop(be); // crash-equivalent: the claim (term 1) stays on the volume

    // Mount 2: the successor's term strictly exceeds the predecessor's.
    let be = KvMetaBackend::open(vol.path()).await.expect("mount 2");
    assert_eq!(be.writer_term(), 2, "successor term must strictly exceed");
    // Clean unmount DELETES the claim — the term must not go with it.
    be.shutdown().await.expect("clean unmount");
    drop(be);
    {
        let probe = KvMetaBackend::open_probe(vol.path()).await.unwrap();
        assert!(
            probe.read_writer_claim().await.is_none(),
            "premise: a clean unmount deletes the claim record"
        );
        assert!(
            raw_ino1_xattr(&probe, WRITER_TERM_XATTR).await.is_some(),
            "the durable term record survives claim deletion"
        );
    }

    // Mount 3: still strictly greater — the era ladder never resets.
    let be = KvMetaBackend::open(vol.path()).await.expect("mount 3");
    assert_eq!(
        be.writer_term(),
        3,
        "the term must survive the clean unmount that deleted the claim"
    );
    be.shutdown().await.unwrap();
}

/// 3. Compatibility (the Phase-8 sequencing rule: bit 7 is NEVER stamped
/// on an existing volume at mount): an un-stamped volume mounts exactly
/// as it does today — term 0, no `writer_term` record, claim bytes
/// unchanged, composed token ≡ the S1 token. Stamping the bit (the
/// reformat window's code path) engages the machinery.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unstamped_volume_stays_term_zero_until_the_bit_is_stamped() {
    let _g = serial().await;
    let vol = fresh_volume().await;
    unstamp_durable_term(vol.path()).await;

    let be = KvMetaBackend::open(vol.path()).await.expect("mount pre-S2");
    assert_eq!(be.writer_term(), 0, "an un-stamped volume has no term");
    let raw = raw_ino1_xattr(&be, WRITER_CLAIM_XATTR)
        .await
        .expect("claim committed");
    assert!(
        !String::from_utf8_lossy(&raw).contains("term"),
        "an un-stamped volume's claim bytes must not change: {}",
        String::from_utf8_lossy(&raw)
    );
    assert!(
        raw_ino1_xattr(&be, WRITER_TERM_XATTR).await.is_none(),
        "no durable term record on an un-stamped volume"
    );
    // Mounting must NOT stamp the bit (the batched reformat window owns
    // that decision).
    let VolumeFormat::V3(sb) = classify_volume(vol.path()).await.unwrap() else {
        panic!("expected v3");
    };
    assert_eq!(
        sb.features_incompat & FEATURE_INCOMPAT_KV_DURABLE_TERM,
        0,
        "mount must never stamp bit 7 on an existing volume"
    );
    drop(be);

    // The stamping path (the reformat window / a future upgrade verb).
    assert!(
        set_durable_term_bit(vol.path()).await.unwrap(),
        "stamping a fresh bit reports the write"
    );
    assert!(
        !set_durable_term_bit(vol.path()).await.unwrap(),
        "re-stamping is a no-op"
    );
    let be = KvMetaBackend::open(vol.path())
        .await
        .expect("mount post-stamp");
    assert_eq!(be.writer_term(), 1, "the stamp engages the era ladder");
    be.shutdown().await.unwrap();

    assert_ne!(
        FEATURES_INCOMPAT_KNOWN & FEATURE_INCOMPAT_KV_DURABLE_TERM,
        0,
        "this binary must understand bit 7"
    );
    assert_eq!(
        FEATURE_INCOMPAT_KV_DURABLE_TERM,
        1 << 7,
        "the execution plan assigns S2 bit 7"
    );
}

/// 4. Publication: the mount gate hands its durable term to the process
/// mint AFTER the barrier, so a fresh process reads and mints in the
/// CURRENT era. This is the §6.11 fix at the source — and the reason the
/// offline verbs stop being vacuous (their `get_fencing_token_ino` read
/// is no longer structurally 0 in a fresh process).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mount_publishes_the_term_so_every_prior_era_token_is_stale() {
    let _g = serial().await;
    let vol = fresh_volume().await;
    let be = KvMetaBackend::open(vol.path()).await.expect("mount");
    let term = be.writer_term();
    assert!(term >= 1);
    assert!(
        dlm::durable_term() >= term,
        "the mount must publish its term to the process mint ({} < {term})",
        dlm::durable_term()
    );
    let process_term = dlm::durable_term();
    assert_eq!(dlm::term_base(), compose_token(process_term, 0));

    let dlm_client = DlmClient::new().unwrap();
    let ino = 77_000_101u64;
    let read = dlm_client.get_fencing_token_ino(ino);
    assert_eq!(
        token_term(read),
        process_term,
        "a never-locked ino reads the CURRENT era's base, never 0"
    );

    let lease = dlm_client
        .acquire_lock(&format!("inode_{ino}"), None, Duration::from_secs(5))
        .await
        .expect("acquire");
    let tok = lease.fencing_token();
    assert_eq!(token_term(tok), process_term, "grants carry the era");
    assert!(token_grant_seq(tok) >= 1, "grant_seq keeps counting");

    // The §6.11 inversion is gone: EVERY token a predecessor era could
    // possibly have minted is stale against this era's reads.
    let prior_era_ceiling = compose_token(process_term - 1, GRANT_SEQ_MAX);
    assert!(
        prior_era_ceiling < read && prior_era_ceiling < tok,
        "a prior era's largest possible token ({prior_era_ceiling}) must be \
         stale against this era's read ({read}) and grant ({tok})"
    );
    lease.release().await.unwrap();
    be.shutdown().await.unwrap();
}

/// 5. The offline-verb arm of §6.11: `fsck` / `defrag` / `config` build
/// their data plane behind `open_routed_meta_set` — the guarded open
/// that takes the D0 claim — and then read fencing tokens from a DLM
/// whose map is empty. Today every such read is 0 and every
/// `save_metadata_to_backend` fence evaluates `0 < 0` (structurally
/// vacuous). With the term durable, the claim they hold IS the era: the
/// read is the current term's base and every pre-crash stamp is stale.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn offline_verb_open_reads_a_nonvacuous_fencing_generation() {
    let _g = serial().await;
    let vol = fresh_volume().await;
    let paths = vec![vol.path().display().to_string()];

    let routed = squeezefs::meta_backend::open_routed_meta_set(&paths)
        .await
        .expect("guarded offline open (the fsck/defrag/config path)");
    let term = routed.volumes[0].writer_term();
    assert!(term >= 1, "the offline verb's own claim carries an era");

    // The verb's fresh-process DLM: no entries, no history.
    let dlm_client = DlmClient::new().unwrap();
    let ino = 78_000_202u64;
    let current = dlm_client.get_fencing_token_ino(ino);
    assert!(
        current > 0,
        "the offline fence check must not evaluate `0 < 0` (spec §6.11)"
    );
    assert_eq!(token_term(current), dlm::durable_term());
    // A stamp from any earlier era is rejected by the SAME comparison the
    // ~24 census sites use (`presented < current`).
    let pre_crash_stamp = compose_token(dlm::durable_term() - 1, 5);
    assert!(
        pre_crash_stamp < current,
        "a pre-crash stamp must fence against an offline verb's read"
    );

    for v in &routed.volumes {
        v.shutdown().await.unwrap();
    }
}

/// 6a. Bit-budget exhaustion, term half: a volume whose era ladder has
/// consumed the 24-bit space refuses the MOUNT loudly. A term rollover
/// would alias a live era's tokens with a retired one's — never silent.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn term_space_exhaustion_refuses_the_mount_loudly() {
    let _g = serial().await;
    let vol = fresh_volume().await;

    // Plant the last legal era on the claim (the mount gate takes the
    // MAX of the claim's term and the durable term record).
    {
        let be = KvMetaBackend::open(vol.path()).await.expect("plant mount");
        let mut claim = be.read_writer_claim().await.expect("claim");
        claim.term = TERM_MAX;
        Metadata::setxattr(be.as_ref(), 1, WRITER_CLAIM_XATTR, &claim.encode())
            .await
            .unwrap();
        be.sync_device().await.unwrap();
        drop(be);
    }

    let err = KvMetaBackend::open(vol.path())
        .await
        .expect_err("an exhausted term space must refuse the mount");
    let msg = format!("{err}");
    assert!(
        msg.contains("term") && msg.to_lowercase().contains("exhaust"),
        "the refusal must name the exhausted term space: {msg}"
    );
}

/// 6b. Bit-budget exhaustion, grant half: past the 40-bit grant space
/// the MINT refuses loudly instead of carrying into the term field
/// (which would forge a future era). Uses the sanctioned mint seam and
/// restores the counter — the mint is process-global.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn grant_seq_exhaustion_refuses_the_mint_loudly() {
    let _g = serial().await;
    let dlm_client = DlmClient::new().unwrap();
    let saved = dlm::test_swap_grant_seq(GRANT_SEQ_MAX - 1);

    let last = dlm_client
        .acquire_lock("inode_79000001", None, Duration::from_secs(5))
        .await
        .expect("the last grant in the budget is served");
    assert_eq!(
        token_grant_seq(last.fencing_token()),
        GRANT_SEQ_MAX,
        "the final grant uses the last legal sequence number"
    );
    last.release().await.unwrap();

    let err = dlm_client
        .acquire_lock("inode_79000002", None, Duration::from_secs(5))
        .await
        .expect_err("past the budget the mint must refuse");
    let msg = format!("{err}");
    assert!(
        msg.contains("grant") && msg.to_lowercase().contains("exhaust"),
        "the refusal must name the exhausted grant space: {msg}"
    );

    dlm::test_swap_grant_seq(saved);
}
