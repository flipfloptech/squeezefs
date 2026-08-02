//! VAL-3 + KW-1 — encryption key handling (`docs/design-key-handling.md`).
//!
//! THE DEFECT (pre-RC spec §3 VAL-3, verified): `--encrypt-key` is
//! documented as *"Path to RSA private key PEM file"*, but the value was
//! stored verbatim into `FormatConfig.encrypt_key`, persisted as the
//! `user.squeezefs.format_config` xattr on ino 1, and consumed as
//! `RsaPrivateKey::from_pkcs1_pem(pem)` — as PEM **content**. Nothing read
//! the file. So (a) as documented the feature did NOT function (a path
//! fails to parse, `private_key = None`, every write on a transformed
//! volume errors), and (b) the only working usage stored key material in
//! cleartext on the metadata volume it encrypts, having passed it on argv.
//! Compounding: the wrap primitive was `rsa 0.9.10`, RUSTSEC-2023-0071
//! with no fixed release in the 0.9 line (execution-plan ruling D3:
//! replace the primitive).
//!
//! THE CONTRACT PINNED HERE:
//!
//!  - **Key input**: the PEM/material is READ FROM THE PATH with
//!    `O_NOFOLLOW` and an fd-side mode check (group/other bits refuse
//!    loud), or from stdin via `-`. Never from argv.
//!  - **Key storage**: only a KDF salt + key id (`EncryptKeyRef`) is
//!    persisted; the legacy `encrypt_key` field never serializes again and
//!    redacts in `Debug`. The actual key resolves at mount from the flag,
//!    `SQUEEZEFS_ENCRYPT_KEY_FILE`, or `/etc/squeezefs/keys/<id>.key`.
//!  - **KW-1 / D3**: the wrap is HKDF-SHA-256 → AEAD (`ring`), versioned
//!    and AAD-bound to the key id; `rsa` is gone from the tree.
//!  - **Compatibility posture (stated, not implied)**: a pre-KW-1
//!    encrypted volume (no `encrypt_key_ref`) REFUSES to mount with an
//!    exact remedy. No new incompat bit is stamped this wave.
//!  - **VAL-7h**: key material is zeroized, and any process that touches
//!    it goes `PR_SET_DUMPABLE(0)` + `RLIMIT_CORE = 0`.
//!  - **The acceptance test the spec names**: `format --encrypt-key
//!    <path>` produces a mountable, writable encrypted volume, and no key
//!    material appears in the persisted format config.
//!
//! RED against dev `7d1ec2e1`: `squeezefs::keyfile` does not exist and
//! `CryptoCompressState::new` still takes a PEM string (compile failure —
//! the `crypto_scratch_pool_tests.rs` precedent), and the acceptance leg
//! fails on its round-trip assertions.

use squeezefs::crypto_compress::CryptoCompressState;
use squeezefs::keyfile::{
    self, EncryptKeyRef, KeyMaterial, VolumeKey, KEY_FILE_ENV, KEY_WRAP_SCHEME_V2,
    MIN_KEY_MATERIAL_BYTES,
};
use squeezefs::FormatConfig;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

/// Serializes the tests that mutate `SQUEEZEFS_ENCRYPT_KEY_FILE` or the
/// process-wide stashed material (the gate runs `--test-threads=1`, but
/// this file must be correct on its own).
static ENV_LOCK: Mutex<()> = Mutex::new(());

fn scratch(tag: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!("sqz-val3-{}-{}", tag, std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).expect("scratch dir");
    p
}

/// A 0600 key file holding `bytes` (the shipped posture).
fn key_file(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
    let p = dir.join(name);
    std::fs::write(&p, bytes).expect("write key file");
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600)).expect("chmod 600");
    p
}

/// 44 chars of base64-shaped material — what the docs tell operators to
/// generate (`head -c 32 /dev/urandom | base64`).
fn material_a() -> Vec<u8> {
    b"NQmQmLQ0y4nqRz9m2rQK4Zt8m0Yy5tJp1c7Hh4Vd0Yg=".to_vec()
}

fn material_b() -> Vec<u8> {
    b"ZZZZmLQ0y4nqRz9m2rQK4Zt8m0Yy5tJp1c7Hh4Vd0Yg=".to_vec()
}

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_squeezefs")
}

/// A `FormatConfig` shaped like the ones `format` writes.
fn cfg(encrypt_algo: &str, key_ref: Option<EncryptKeyRef>) -> FormatConfig {
    FormatConfig {
        name: "squeezefs".to_string(),
        block_size: 4 * 1024 * 1024,
        capacity: 1 << 30,
        inodes: 1000,
        compression: "none".to_string(),
        encrypt_algo: encrypt_algo.to_string(),
        encrypt_key: None,
        encrypt_key_ref: key_ref,
        mem_cache_size: None,
        disk_cache_size: None,
        disk_cache_paths: None,
        data_lv: None,
        data_volumes: None,
        read_cache_size: None,
        write_cache_size: None,
        read_mem_cache_size: None,
        write_mem_cache_size: None,
        dismount_wait: None,
        upload_delay: None,
        fuse_io_uring_sqpoll_idle_ms: None,
        meta_routing_width: None,
        meta_slot_runs: None,
        meta_volumes: None,
    }
}

// ---------------------------------------------------------------------------
// 1. Key input — from the path (O_NOFOLLOW + mode check) or stdin, never argv
// ---------------------------------------------------------------------------

#[test]
fn key_file_material_is_read_from_the_path() {
    let d = scratch("read");
    let p = key_file(&d, "k", &material_a());
    let m = keyfile::read_key_file(&p).expect("0600 regular file must be accepted");
    assert_eq!(m.as_bytes(), material_a().as_slice());
}

#[test]
fn key_file_trailing_whitespace_is_trimmed() {
    let d = scratch("trim");
    let plain = key_file(&d, "plain", &material_a());
    let mut with_nl = material_a();
    with_nl.push(b'\n');
    let echoed = key_file(&d, "echoed", &with_nl);
    assert_eq!(
        keyfile::read_key_file(&plain).unwrap().as_bytes(),
        keyfile::read_key_file(&echoed).unwrap().as_bytes(),
        "a key file written with `echo` must derive the same key as one without the newline"
    );
}

#[test]
fn key_file_refuses_group_or_other_permissions() {
    let d = scratch("modes");
    for mode in [0o640u32, 0o604, 0o660, 0o666, 0o644, 0o601] {
        let p = key_file(&d, &format!("k{mode:o}"), &material_a());
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(mode)).unwrap();
        let err = keyfile::read_key_file(&p)
            .expect_err(&format!("mode {mode:o} must be refused — group/other can read the key"));
        assert!(
            err.contains("chmod 600") && err.contains(&format!("{mode:o}")),
            "refusal must name the observed mode and the remedy, got: {err}"
        );
    }
}

#[test]
fn key_file_refuses_a_symlink() {
    let d = scratch("symlink");
    let real = key_file(&d, "real", &material_a());
    let link = d.join("link");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let err = keyfile::read_key_file(&link).expect_err("O_NOFOLLOW must refuse a symlink");
    assert!(
        err.to_lowercase().contains("symlink") || err.contains("ELOOP") || err.contains("link"),
        "refusal must say the path is a symlink, got: {err}"
    );
}

#[test]
fn key_file_refuses_a_non_regular_file() {
    let d = scratch("nonreg");
    let err = keyfile::read_key_file(&d).expect_err("a directory is not a key file");
    assert!(
        err.contains("regular file"),
        "refusal must name the class, got: {err}"
    );
}

#[test]
fn key_file_refuses_short_material() {
    let d = scratch("short");
    let p = key_file(&d, "k", b"hunter2");
    let err = keyfile::read_key_file(&p).expect_err("a passphrase-length secret must refuse");
    assert!(
        err.contains(&MIN_KEY_MATERIAL_BYTES.to_string()),
        "refusal must name the minimum, got: {err}"
    );
}

#[test]
fn key_material_reads_from_stdin_shaped_input() {
    let mut src = std::io::Cursor::new({
        let mut v = material_a();
        v.push(b'\n');
        v
    });
    let m = keyfile::read_key_reader(&mut src, "stdin").expect("stdin material");
    assert_eq!(m.as_bytes(), material_a().as_slice());
}

#[test]
fn key_material_never_appears_in_debug_output() {
    let d = scratch("debug");
    let p = key_file(&d, "k", &material_a());
    let m = keyfile::read_key_file(&p).unwrap();
    let shown = format!("{m:?}");
    assert!(
        !shown.contains("NQmQ") && shown.contains("redacted"),
        "KeyMaterial Debug must redact, got: {shown}"
    );

    let vk = keyfile::derive_volume_key(&m, &[7u8; 32]);
    let shown = format!("{vk:?}");
    assert!(
        shown.contains(&vk.key_id_hex()) && shown.contains("redacted"),
        "VolumeKey Debug must show the id and redact the key, got: {shown}"
    );

    let state = CryptoCompressState::new("none".to_string(), "aes256gcm".to_string(), Some(&vk));
    let shown = format!("{state:?}");
    assert!(
        shown.contains("redacted") && !shown.contains("NQmQ"),
        "CryptoCompressState Debug must redact key material, got: {shown}"
    );
}

// ---------------------------------------------------------------------------
// 2. Derivation and the durable key reference
// ---------------------------------------------------------------------------

fn derive(material: &[u8], salt: &[u8; 32]) -> VolumeKey {
    let m = KeyMaterial::from_bytes(material.to_vec()).expect("material");
    keyfile::derive_volume_key(&m, salt)
}

#[test]
fn derivation_is_deterministic_and_salt_bound() {
    let s1 = [1u8; 32];
    let s2 = [2u8; 32];
    assert_eq!(
        derive(&material_a(), &s1).key_id_hex(),
        derive(&material_a(), &s1).key_id_hex(),
        "same material + salt must derive the same key id"
    );
    assert_ne!(
        derive(&material_a(), &s1).key_id_hex(),
        derive(&material_a(), &s2).key_id_hex(),
        "the salt must bind the derivation"
    );
    assert_ne!(
        derive(&material_a(), &s1).key_id_hex(),
        derive(&material_b(), &s1).key_id_hex(),
        "different material must derive a different key id"
    );
}

#[test]
fn key_ref_carries_only_a_salt_and_an_id() {
    let salt = keyfile::new_kdf_salt();
    let vk = derive(&material_a(), &salt);
    let kr = keyfile::make_key_ref(&salt, &vk);
    assert_eq!(kr.scheme, KEY_WRAP_SCHEME_V2);
    assert_eq!(kr.kdf_salt.len(), 64, "32 bytes of salt, hex");
    assert_eq!(kr.key_id.len(), 16, "8 bytes of key id, hex");
    let json = serde_json::to_string(&kr).unwrap();
    assert!(
        !json.contains("NQmQ") && !json.contains("BEGIN"),
        "the key reference must carry no material: {json}"
    );
    // The reference is the whole persisted surface: three public fields.
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();
    let keys: Vec<&String> = v.as_object().unwrap().keys().collect();
    assert_eq!(keys.len(), 3, "unexpected fields in the key reference: {keys:?}");
}

#[test]
fn two_formats_of_the_same_key_file_get_different_salts() {
    assert_ne!(
        keyfile::new_kdf_salt(),
        keyfile::new_kdf_salt(),
        "each format must mint a fresh salt"
    );
}

// ---------------------------------------------------------------------------
// 3. The wrap primitive (KW-1 / D3)
// ---------------------------------------------------------------------------

fn state_for(vk: &VolumeKey, algo: &str) -> CryptoCompressState {
    CryptoCompressState::new("none".to_string(), algo.to_string(), Some(vk))
}

#[test]
fn wrap_unwrap_round_trips_under_both_record_algorithms() {
    for algo in ["aes256gcm", "chacha20"] {
        let vk = derive(&material_a(), &[9u8; 32]);
        let st = state_for(&vk, algo);
        let dk = [0x42u8; 32];
        let blob = st.wrap_session_key(&dk).expect("wrap");
        let back = st.unwrap_session_key(&blob).expect("unwrap");
        assert_eq!(&back[..], &dk[..], "{algo}: wrap/unwrap must round-trip");
    }
}

#[test]
fn wrap_blob_is_versioned_magic_and_compact() {
    let vk = derive(&material_a(), &[9u8; 32]);
    let st = state_for(&vk, "aes256gcm");
    let blob = st.wrap_session_key(&[3u8; 32]).unwrap();
    assert_eq!(&blob[..2], b"SK", "wrap blobs are self-describing");
    assert_eq!(blob[2], 2, "wrap format version 2");
    assert_eq!(
        blob.len(),
        3 + 12 + 32 + 16,
        "magic+version+nonce+ct+tag — far below the retired RSA wrap"
    );
}

#[test]
fn unwrap_refuses_a_foreign_key_a_tampered_blob_and_a_bad_version() {
    let vk = derive(&material_a(), &[9u8; 32]);
    let other = derive(&material_b(), &[9u8; 32]);
    let st = state_for(&vk, "aes256gcm");
    let st_other = state_for(&other, "aes256gcm");
    let blob = st.wrap_session_key(&[5u8; 32]).unwrap();

    assert!(
        st_other.unwrap_session_key(&blob).is_err(),
        "a blob from another volume key must not unwrap"
    );

    let mut tampered = blob.clone();
    let last = tampered.len() - 1;
    tampered[last] ^= 0x01;
    assert!(
        st.unwrap_session_key(&tampered).is_err(),
        "AEAD tag must reject a tampered blob"
    );

    let mut wrong_version = blob.clone();
    wrong_version[2] = 0x7f;
    let err = st
        .unwrap_session_key(&wrong_version)
        .expect_err("an unknown wrap version must refuse loud");
    assert!(
        format!("{err}").contains("version"),
        "refusal must name the version, got: {err}"
    );

    assert!(
        st.unwrap_session_key(&blob[..10]).is_err(),
        "a truncated blob must refuse, never panic"
    );
}

#[test]
fn an_encrypted_block_round_trips_and_a_foreign_key_cannot_read_it() {
    let vk = derive(&material_a(), &[11u8; 32]);
    let st = CryptoCompressState::new("lz4".to_string(), "aes256gcm".to_string(), Some(&vk));
    st.init_scratch_pool(4 * 1024 * 1024);
    let payload = bytes::Bytes::from(vec![0x5Au8; 256 * 1024]);
    let image = st.process_write(payload.clone()).expect("write transform");
    assert_ne!(&image[..], &payload[..], "the stored image must be transformed");
    let back = st.process_read(&image).expect("read transform");
    assert_eq!(&*back, payload.as_ref());

    let foreign = derive(&material_b(), &[11u8; 32]);
    let st2 = CryptoCompressState::new("lz4".to_string(), "aes256gcm".to_string(), Some(&foreign));
    assert!(
        st2.process_read(&image).is_err(),
        "a different key file must not decrypt the block"
    );
}

#[test]
fn an_encrypted_state_without_a_key_refuses_writes_loud() {
    let st = CryptoCompressState::new("none".to_string(), "aes256gcm".to_string(), None);
    let err = st
        .process_write(bytes::Bytes::from_static(b"payload"))
        .expect_err("no key configured must fail loud, never write plaintext");
    assert!(
        format!("{err}").to_lowercase().contains("key"),
        "refusal must name the missing key, got: {err}"
    );
}

// ---------------------------------------------------------------------------
// 4. The persisted format config carries no key material
// ---------------------------------------------------------------------------

#[test]
fn format_config_never_serializes_key_material() {
    let salt = keyfile::new_kdf_salt();
    let vk = derive(&material_a(), &salt);
    let mut c = cfg("aes256gcm", Some(keyfile::make_key_ref(&salt, &vk)));
    // Even a config that somehow carries the legacy field must not write it.
    c.encrypt_key = Some(keyfile::RedactedSecret::new(
        "-----BEGIN RSA PRIVATE KEY-----secret-----END RSA PRIVATE KEY-----".to_string(),
    ));
    let json = serde_json::to_string(&c).unwrap();
    assert!(
        !json.contains("BEGIN RSA") && !json.contains("secret"),
        "key material must never reach the volume: {json}"
    );
    assert!(
        !json.contains("\"encrypt_key\""),
        "the legacy field must never be written again: {json}"
    );
    assert!(json.contains(&vk.key_id_hex()), "the key id must persist");

    let shown = format!("{c:?}");
    assert!(
        !shown.contains("BEGIN RSA") && shown.contains("redacted"),
        "FormatConfig Debug must redact the legacy key field, got: {shown}"
    );
}

#[test]
fn a_legacy_config_still_deserializes_so_the_refusal_can_name_it() {
    let raw = r#"{"name":"squeezefs","block_size":4194304,"capacity":1073741824,"inodes":1000,
        "compression":"none","encrypt_algo":"aes256gcm-rsa",
        "encrypt_key":"-----BEGIN RSA PRIVATE KEY-----x-----END RSA PRIVATE KEY-----"}"#;
    let c: FormatConfig = serde_json::from_str(raw).expect("legacy configs must still parse");
    assert!(c.encrypt_key.is_some());
    assert!(c.encrypt_key_ref.is_none());
}

// ---------------------------------------------------------------------------
// 5. Compatibility posture — pre-KW-1 encrypted volumes refuse LOUD
// ---------------------------------------------------------------------------

#[test]
fn a_pre_kw1_encrypted_volume_refuses_with_an_exact_remedy() {
    let mut c = cfg("aes256gcm-rsa", None);
    c.encrypt_key = Some(keyfile::RedactedSecret::new("-----BEGIN RSA PRIVATE KEY-----x".into()));
    let msg = keyfile::legacy_encrypted_volume_refusal(&c)
        .expect("an RSA-era encrypted volume must refuse to mount");
    for needle in ["reformat", "squeezefs format", "--encrypt-key", "7d1ec2e1"] {
        assert!(
            msg.contains(needle),
            "the remedy must name {needle:?}; got:\n{msg}"
        );
    }
    assert!(
        msg.contains("cleartext"),
        "a config that carried the key must say the key is compromised:\n{msg}"
    );
    assert!(keyfile::mount_volume_key(&c).is_err());
}

#[test]
fn an_encrypted_volume_with_no_key_reference_refuses() {
    let c = cfg("chacha20-rsa", None);
    let msg = keyfile::legacy_encrypted_volume_refusal(&c)
        .expect("no key reference means pre-KW-1: refuse");
    assert!(msg.contains("reformat"), "{msg}");
}

#[test]
fn a_plaintext_volume_is_never_refused_by_the_key_posture() {
    let mut c = cfg("none", None);
    assert!(keyfile::legacy_encrypted_volume_refusal(&c).is_none());
    assert!(keyfile::mount_volume_key(&c).unwrap().is_none());
    // A stray legacy field on a plaintext volume encrypted nothing.
    c.encrypt_key = Some(keyfile::RedactedSecret::new("junk".into()));
    assert!(keyfile::legacy_encrypted_volume_refusal(&c).is_none());
}

#[test]
fn a_current_encrypted_volume_is_not_refused_by_the_legacy_screen() {
    let salt = keyfile::new_kdf_salt();
    let vk = derive(&material_a(), &salt);
    let c = cfg("aes256gcm", Some(keyfile::make_key_ref(&salt, &vk)));
    assert!(keyfile::legacy_encrypted_volume_refusal(&c).is_none());
}

// ---------------------------------------------------------------------------
// 6. Mount-time resolution (flag → env → /etc/squeezefs/keys → loud refusal)
// ---------------------------------------------------------------------------

#[test]
fn mount_resolves_the_key_from_the_env_path_and_verifies_the_id() {
    let _g = ENV_LOCK.lock().unwrap();
    keyfile::clear_key_material();
    let d = scratch("env-resolve");
    let p = key_file(&d, "k", &material_a());
    let salt = keyfile::new_kdf_salt();
    let vk = derive(&material_a(), &salt);
    let c = cfg("aes256gcm", Some(keyfile::make_key_ref(&salt, &vk)));

    std::env::set_var(KEY_FILE_ENV, &p);
    let resolved = keyfile::mount_volume_key(&c)
        .expect("the env path must resolve")
        .expect("an encrypted volume yields a key");
    assert_eq!(resolved.key_id_hex(), vk.key_id_hex());

    // Wrong key file for this volume: refuse naming BOTH ids, never
    // silently decrypt-fail on every read.
    let wrong = key_file(&d, "wrong", &material_b());
    std::env::set_var(KEY_FILE_ENV, &wrong);
    let err = keyfile::mount_volume_key(&c).expect_err("a wrong key file must refuse at mount");
    assert!(
        err.contains(&vk.key_id_hex()),
        "the refusal must name the expected id: {err}"
    );
    std::env::remove_var(KEY_FILE_ENV);
}

#[test]
fn the_stashed_flag_material_wins_over_the_env_path() {
    let _g = ENV_LOCK.lock().unwrap();
    let d = scratch("flag-wins");
    let salt = keyfile::new_kdf_salt();
    let vk = derive(&material_a(), &salt);
    let c = cfg("aes256gcm", Some(keyfile::make_key_ref(&salt, &vk)));
    let wrong = key_file(&d, "wrong", &material_b());
    std::env::set_var(KEY_FILE_ENV, &wrong);
    keyfile::stash_key_material(KeyMaterial::from_bytes(material_a()).unwrap());

    let resolved = keyfile::mount_volume_key(&c).unwrap().unwrap();
    assert_eq!(resolved.key_id_hex(), vk.key_id_hex());

    keyfile::clear_key_material();
    std::env::remove_var(KEY_FILE_ENV);
}

#[test]
fn an_encrypted_volume_with_no_key_source_refuses_naming_every_source() {
    let _g = ENV_LOCK.lock().unwrap();
    keyfile::clear_key_material();
    std::env::remove_var(KEY_FILE_ENV);
    let salt = keyfile::new_kdf_salt();
    let vk = derive(&material_a(), &salt);
    let c = cfg("aes256gcm", Some(keyfile::make_key_ref(&salt, &vk)));
    // The default path for a random key id cannot exist.
    let err = keyfile::mount_volume_key(&c).expect_err("no key source must refuse the mount");
    assert!(err.contains("--encrypt-key"), "{err}");
    assert!(err.contains(KEY_FILE_ENV), "{err}");
    assert!(
        err.contains(&keyfile::default_key_path(&vk.key_id_hex()).display().to_string()),
        "{err}"
    );
}

// ---------------------------------------------------------------------------
// 7. VAL-7h — a process holding key material is not dumpable
// ---------------------------------------------------------------------------

/// Runs `f` in a forked child and asserts it exits 0. `harden_process_memory`
/// is process-global and irreversible, so it must not run in the test
/// process itself.
fn in_child(f: impl FnOnce() -> bool) {
    let pid = unsafe { libc::fork() };
    assert!(pid >= 0, "fork");
    if pid == 0 {
        let ok = f();
        unsafe { libc::_exit(if ok { 0 } else { 1 }) };
    }
    let mut status = 0i32;
    unsafe { libc::waitpid(pid, &mut status, 0) };
    let code = if libc::WIFEXITED(status) {
        libc::WEXITSTATUS(status)
    } else {
        -1
    };
    assert_eq!(code, 0, "child assertions failed");
}

#[test]
fn handling_key_material_makes_the_process_undumpable_with_no_core() {
    let d = scratch("harden");
    let p = key_file(&d, "k", &material_a());
    in_child(move || {
        // Baseline: a fresh process IS dumpable.
        if unsafe { libc::prctl(libc::PR_GET_DUMPABLE) } != 1 {
            return false;
        }
        let _m = keyfile::read_key_file(&p).expect("read");
        if unsafe { libc::prctl(libc::PR_GET_DUMPABLE) } != 0 {
            return false;
        }
        let mut rl = libc::rlimit {
            rlim_cur: 1,
            rlim_max: 1,
        };
        if unsafe { libc::getrlimit(libc::RLIMIT_CORE, &mut rl) } != 0 {
            return false;
        }
        rl.rlim_cur == 0
    });
}

// ---------------------------------------------------------------------------
// 8. Acceptance (spec VAL-3): `format --encrypt-key <path>` produces a
//    mountable, writable encrypted volume, and the persisted config holds
//    no key material.
// ---------------------------------------------------------------------------

fn run(cmd: &mut Command, what: &str) -> std::process::Output {
    let out = cmd.output().unwrap_or_else(|e| panic!("spawn {what}: {e}"));
    assert!(
        out.status.success(),
        "{what} failed ({}):\nstdout:\n{}\nstderr:\n{}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    out
}

fn format_encrypted(base: &Path, key: &Path) -> (PathBuf, PathBuf, String) {
    let meta = base.join("meta.bin");
    let data = base.join("data.bin");
    std::fs::File::create(&meta)
        .unwrap()
        .set_len(256 * 1024 * 1024)
        .unwrap();
    std::fs::File::create(&data)
        .unwrap()
        .set_len(2 * 1024 * 1024 * 1024)
        .unwrap();
    let out = run(
        Command::new(bin())
            .arg("format")
            .arg(format!("sqmeta://{}", meta.display()))
            .arg(format!("sqdata://{}", data.display()))
            .arg("--force")
            .arg("--encrypt-algo")
            .arg("aes256gcm")
            .arg("--encrypt-key")
            .arg(key),
        "format --encrypt-key",
    );
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    (meta, data, stdout)
}

/// Read the format config back out of the volume the way mount does.
fn read_back_config(meta: &Path) -> (FormatConfig, Vec<u8>) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let vol = squeezefs::meta_backend::open_volume_probe(meta.to_str().unwrap())
            .await
            .expect("probe mount");
        let raw = vol
            .getxattr(
                vol.slot0_root_ino(),
                squeezefs::meta_backend::kv::builder::FORMAT_CONFIG_XATTR,
            )
            .await
            .expect("getxattr")
            .expect("format config present");
        let cfg: FormatConfig = serde_json::from_slice(&raw).expect("parse config");
        (cfg, raw)
    })
}

#[test]
fn format_with_a_key_path_persists_only_a_key_reference() {
    let base = scratch("acceptance-format");
    let key = key_file(&base, "volume.key", &material_a());
    let (meta, _data, stdout) = format_encrypted(&base, &key);
    let (cfg, raw) = read_back_config(&meta);

    let kr = cfg
        .encrypt_key_ref
        .as_ref()
        .expect("format must persist a key reference");
    assert_eq!(kr.scheme, KEY_WRAP_SCHEME_V2);
    assert!(cfg.encrypt_key.is_none(), "no key material on the volume");
    let stored = String::from_utf8_lossy(&raw);
    assert!(
        !stored.contains("NQmQ"),
        "the persisted config contains key material: {stored}"
    );
    assert!(
        stdout.contains(&kr.key_id),
        "format must print the key id operators need at mount:\n{stdout}"
    );

    // And the key file drives the wrap: derive from the file + the stored
    // salt and round-trip a block.
    let m = keyfile::read_key_file(&key).unwrap();
    let salt = keyfile::salt_from_hex(&kr.kdf_salt).expect("salt hex");
    let vk = keyfile::derive_volume_key(&m, &salt);
    assert_eq!(vk.key_id_hex(), kr.key_id);
    let st = CryptoCompressState::new(cfg.compression.clone(), cfg.encrypt_algo.clone(), Some(&vk));
    let payload = bytes::Bytes::from(vec![0xC3u8; 64 * 1024]);
    let image = st.process_write(payload.clone()).unwrap();
    assert_eq!(&*st.process_read(&image).unwrap(), payload.as_ref());
}

#[test]
fn format_refuses_an_encrypted_volume_with_no_key_and_a_key_with_no_algo() {
    let base = scratch("acceptance-refusals");
    let meta = base.join("meta.bin");
    std::fs::File::create(&meta)
        .unwrap()
        .set_len(256 * 1024 * 1024)
        .unwrap();
    let key = key_file(&base, "k", &material_a());

    let out = Command::new(bin())
        .arg("format")
        .arg(format!("sqmeta://{}", meta.display()))
        .arg("--force")
        .arg("--encrypt-algo")
        .arg("aes256gcm")
        .output()
        .unwrap();
    assert!(!out.status.success(), "encryption with no key must refuse");
    let err = String::from_utf8_lossy(&out.stderr).to_string()
        + &String::from_utf8_lossy(&out.stdout);
    assert!(err.contains("--encrypt-key"), "{err}");

    let out = Command::new(bin())
        .arg("format")
        .arg(format!("sqmeta://{}", meta.display()))
        .arg("--force")
        .arg("--encrypt-key")
        .arg(&key)
        .output()
        .unwrap();
    assert!(!out.status.success(), "a key with no algo must refuse");
}

fn fuse_available() -> bool {
    Path::new("/dev/fuse").exists() && which_fusermount().is_some()
}

fn which_fusermount() -> Option<PathBuf> {
    ["/usr/bin/fusermount3", "/bin/fusermount3", "/usr/local/bin/fusermount3"]
        .iter()
        .map(PathBuf::from)
        .find(|p| p.exists())
}

struct Mount {
    child: std::process::Child,
    mnt: PathBuf,
    log: PathBuf,
}

impl Drop for Mount {
    fn drop(&mut self) {
        let _ = Command::new("fusermount3").arg("-uz").arg(&self.mnt).status();
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Mount {
    fn unmount(&mut self) {
        for _ in 0..10 {
            if Command::new("fusermount3")
                .arg("-u")
                .arg(&self.mnt)
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(500));
        }
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline {
            if self.child.try_wait().expect("try_wait").is_some() {
                return;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        let _ = self.child.kill();
        panic!("mount daemon did not exit within 30s of unmount");
    }
}

fn spawn_mount(meta: &Path, mnt: &Path, log: &Path, key: Option<&Path>) -> Mount {
    std::fs::create_dir_all(mnt).unwrap();
    let logf = std::fs::File::create(log).unwrap();
    let mut cmd = Command::new(bin());
    cmd.arg("mount")
        .arg(format!("sqmeta://{}", meta.display()))
        .arg(mnt)
        .arg("--uid")
        .arg(unsafe { libc::getuid() }.to_string())
        .arg("--gid")
        .arg(unsafe { libc::getgid() }.to_string());
    if let Some(k) = key {
        cmd.arg("--encrypt-key").arg(k);
    }
    let child = cmd
        .stdout(Stdio::from(logf.try_clone().unwrap()))
        .stderr(Stdio::from(logf))
        .spawn()
        .expect("spawn squeezefs mount");
    Mount {
        child,
        mnt: mnt.to_path_buf(),
        log: log.to_path_buf(),
    }
}

fn wait_ready(m: &Mount, secs: u64) -> bool {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < deadline {
        if std::fs::read_to_string(m.mnt.join(".stats")).is_ok() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    false
}

/// THE acceptance test the spec names: `format --encrypt-key <path>` must
/// produce a **mountable, writable** encrypted volume — and mounting it
/// without the key file must fail loudly instead of silently serving
/// garbage.
#[test]
fn format_with_a_key_path_produces_a_mountable_writable_encrypted_volume() {
    if !fuse_available() {
        eprintln!("SKIP: /dev/fuse or fusermount3 unavailable");
        return;
    }
    let base = scratch("acceptance-mount");
    let key = key_file(&base, "volume.key", &material_a());
    let (meta, _data, _out) = format_encrypted(&base, &key);
    let mnt = base.join("mnt");
    let payload: Vec<u8> = (0..(512 * 1024)).map(|i| (i % 251) as u8).collect();

    {
        let mut m = spawn_mount(&meta, &mnt, &base.join("mount1.log"), Some(&key));
        assert!(
            wait_ready(&m, 90),
            "encrypted mount never became ready; log:\n{}",
            std::fs::read_to_string(&m.log).unwrap_or_default()
        );
        let f = mnt.join("secret.bin");
        {
            let mut fh = std::fs::File::create(&f).expect("create on encrypted volume");
            fh.write_all(&payload).expect("write");
            fh.sync_all().expect("fsync");
        }
        let mut back = Vec::new();
        let mut fh = std::fs::File::open(&f).unwrap();
        fh.seek(SeekFrom::Start(0)).unwrap();
        fh.read_to_end(&mut back).unwrap();
        assert_eq!(back, payload, "read-back through the encrypted path");
        m.unmount();
    }

    // Remount with the key: the data is still there (the wrap survives a
    // process boundary — the whole point of the key reference).
    {
        let mut m = spawn_mount(&meta, &mnt, &base.join("mount2.log"), Some(&key));
        assert!(wait_ready(&m, 90), "remount with the key must succeed");
        let mut back = Vec::new();
        std::fs::File::open(mnt.join("secret.bin"))
            .unwrap()
            .read_to_end(&mut back)
            .unwrap();
        assert_eq!(back, payload, "remount must read the encrypted data back");
        m.unmount();
    }

    // Without the key file: the mount must FAIL, loudly.
    {
        let m = spawn_mount(&meta, &mnt, &base.join("mount3.log"), None);
        assert!(
            !wait_ready(&m, 25),
            "an encrypted volume must not mount without its key"
        );
        let log = std::fs::read_to_string(&m.log).unwrap_or_default();
        assert!(
            log.contains("--encrypt-key") || log.contains(KEY_FILE_ENV),
            "the refusal must name the key sources; log:\n{log}"
        );
    }
}
