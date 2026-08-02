# Encryption key handling (VAL-3 + KW-1) — Rev 1

**Items:** pre-RC spec §3 **VAL-3** (P0, [V]) · §3 **VAL-7h** (key hygiene) · §10 **ENG-2** (`rsa`
advisory) · execution-plan ruling **D3** (replace the primitive) · manifest §3 row **KW-1**.

**Status:** implemented on `fix/val3-key-handling`. The on-disk wrap format is NEW but stamps
**no incompat bit** this wave — bit assignment rides the Phase-8 reformat window (execution plan
§2 Phase 8, one-window rule).

---

## 1. The defect this replaces

`--encrypt-key` was documented as *"Path to RSA private key PEM file"*. The value was stored
verbatim into `FormatConfig.encrypt_key`, persisted as the `user.squeezefs.format_config` xattr on
ino 1, and consumed as `RsaPrivateKey::from_pkcs1_pem(pem)` — as PEM **content**. Nothing read the
file. Two consequences, both verified:

1. **As documented the feature did not function.** A path makes `from_pkcs1_pem("/etc/key.pem")`
   fail, leaving `private_key = None`; every `resolve_encrypt_key()` on a transformed volume then
   errors and all writes fail. Such a volume never accepted a single write.
2. **The only working usage stored key material in cleartext on the metadata volume it encrypts**,
   and passed it on `argv` (`/proc/<pid>/cmdline` for the duration of `format`). At-rest
   encryption provided no protection against the threat it exists for.

Compounding, the wrap primitive was `rsa 0.9.10` — RUSTSEC-2023-0071 (Marvin timing attack), the
one audit finding with **no patched release in the 0.9 line** (`docs/rc-manifest.md` §5), which is
why D3 rules replacement rather than adjudication.

## 2. Chosen primitive: HKDF-SHA-256 → AEAD key wrap (`ring`)

**Scheme id:** `hkdf-sha256/aead-kw/v2` (`keyfile::KEY_WRAP_SCHEME_V2`).

```
material  = the key file's bytes (whitespace-trimmed), ≥ 32 B, never on argv
PRK       = HKDF-Extract(salt = per-volume random 32 B, ikm = material)      [RFC 5869]
KEK       = HKDF-Expand(PRK, info = "squeezefs:key-wrap:v2:kek",    32 B)
key_id    = HKDF-Expand(PRK, info = "squeezefs:key-wrap:v2:key-id",  8 B)
wrap blob = "SK" ‖ 0x02 ‖ nonce(12) ‖ AEAD_seal(KEK, nonce, aad = key_id, data key(32))  = 63 B
```

The AEAD is the volume's **record** algorithm (AES-256-GCM or ChaCha20-Poly1305) — one primitive
per volume, not two. The per-block session-key structure is unchanged: a random 32-byte data key
per mount session, its wrap blob carried in every block header, unwrap memoized by blob bytes.

**Why this and not X25519/HPKE or AES-KW (RFC 3394):**

- **`ring` 0.17 is already a direct dependency** and already provides the record AEAD, HKDF, and
  the CSPRNG. Wrap and record cipher share one audited primitive set, so the review surface
  *shrinks*; no crate enters the tree, and `rsa` (plus its `num-bigint-dig` bignum stack) leaves.
  The task's own preference — "a well-reviewed crate already in the tree's dependency graph" — is
  satisfied exactly.
- **The Marvin class is structurally absent.** No variable-time bignum modular exponentiation
  exists anywhere in the design; AES-GCM and ChaCha20-Poly1305 in `ring` are constant-time
  (including the table-free AES fallback where AES-NI is absent — *portable by default*).
- **Asymmetry buys nothing today.** The mount that writes also reads and would hold the private
  half regardless, so an X25519/HPKE ECDH would add ~40 B per block header and an agreement per
  cold unwrap for a property no shipped code consumes. The wrap blob is versioned (`0x02`) and
  self-describing, so a public-wrap posture (write-only mounts holding only a public key) remains
  a strictly additive `0x03`.
- **AES-KW (RFC 3394) is not in `ring`** and is unauthenticated over associated data; the AEAD
  wrap binds the blob to `key_id` as AAD, so a blob lifted from another volume fails the tag check
  instead of silently unwrapping to a wrong key.

Header cost: 63 B vs 256 B (RSA-2048) / 512 B (RSA-4096) per block. The conservative geometry
constants (`WRAPPED_KEY_LEN_FALLBACK`, `TRANSFORM_BLOCK_HEADROOM`) are deliberately **left at
their RSA-4096 values** — shrinking them changes the format-time `block_size` clamp, i.e. on-disk
geometry, which belongs to the Phase-8 window.

## 3. Key input (never argv)

`--encrypt-key <path>` reads the file; `--encrypt-key -` reads stdin. The value on argv is a
*path*, never material.

`keyfile::read_key_file` opens with `O_NOFOLLOW | O_CLOEXEC` and validates on the **fd** (no
TOCTOU): regular file, `st_uid == geteuid()` (or euid 0), `mode & 0o077 == 0` (any group/other
bit refuses, the ssh posture), size ≤ 64 KiB, material ≥ 32 B after trimming ASCII whitespace.
Every refusal names the file, the observed mode, and the `chmod 600` remedy.

The file must hold **key material, not a passphrase**: HKDF has no work factor. `head -c 32
/dev/urandom | base64 > key` is the documented generator.

## 4. Key storage: only a salt and an id

`FormatConfig.encrypt_key_ref: Option<EncryptKeyRef>` = `{ scheme, kdf_salt (64 hex), key_id
(16 hex) }`. None of it is secret: the salt is public by RFC 5869 construction, and `key_id` is an
8-byte HKDF output that identifies which key file a volume expects.

`FormatConfig.encrypt_key` survives **only** as a legacy-detection field: type
`RedactedSecret` (redacting `Debug`, zeroized on drop), `#[serde(default, skip_serializing)]` — a
current binary can read a pre-KW-1 config to refuse it, and can never write key material back.

**Mount-time resolution order** (KMS-style indirection; file provider shipped):

1. `--encrypt-key <path|->` on `squeezefs mount` (read pre-fork, so `-` works for daemon mounts);
2. `SQUEEZEFS_ENCRYPT_KEY_FILE=<path>` (a path in the environment, never material);
3. `/etc/squeezefs/keys/<key_id>.key`;
4. otherwise the mount **fails loudly**, naming all three.

The derived `key_id` is compared against the config's; a mismatch refuses naming both ids, so a
wrong key file is a loud refusal at mount rather than tag failures on every read.

## 5. Process hygiene (VAL-7h)

`keyfile::harden_process_memory()` runs (idempotently, logged once) before any key material is
read — `format --encrypt-key …` and every encrypted mount, and nowhere else:

- `prctl(PR_SET_DUMPABLE, 0)` — no core dump, no same-uid `ptrace`, `/proc/<pid>/mem` closed;
- `setrlimit(RLIMIT_CORE, 0, 0)` — belt and braces for the `core_pattern`-piped case.

Everything that holds material is `Zeroizing` (`KeyMaterial`, `VolumeKey.kek`, the session data
key in `CryptoCompressState`), and every key-bearing type has a redacting `Debug`.

## 6. Compatibility posture for existing encrypted volumes — REFUSE LOUD

**Stated explicitly, and pinned by `tests/encrypt_key_handling_tests.rs`:**

An encrypted volume (`encrypt_algo != none`) whose config carries **no `encrypt_key_ref`** is a
pre-KW-1 (RSA-wrap) volume and **refuses to mount**, naming the exact remedy: copy the data off
with a binary at or before `7d1ec2e1`, `format --force` with a current binary, copy back. Where
the legacy `encrypt_key` field is present the message additionally states that the key material
was stored in cleartext on the volume and must be considered compromised.

This is the only posture under which `rsa` can leave the tree: keeping a *reader* keeps the crate,
and the advisory with it (D3's whole point). The cost is bounded by the defect itself — volumes
formatted the **documented** way (`--encrypt-key <path>`) never accepted a write, so they carry no
data to lose; only the undocumented PEM-content usage holds data, and that usage stored its key in
cleartext beside the ciphertext. Unencrypted volumes are entirely unaffected (`encrypt_algo =
none` never consults a key), which is every volume the project's own rigs and benchmarks format.

**No incompat bit is stamped.** The wrap version lives in the block header (`0x02`) and the scheme
string in the key ref; the format-level bit for KW-1 is assigned in the Phase-8 reformat window
alongside the DUR-6 sharded indirect map and S2 bit 7.

## 7. What the reformat window still owes

1. The KW-1 **incompat bit** itself (manifest §3 row), stamped at format on encrypted volumes so a
   pre-KW-1 binary refuses a KW-1 volume from the superblock rather than from the wrap header.
2. Shrinking `WRAPPED_KEY_LEN_FALLBACK` 512 → 63 and `TRANSFORM_BLOCK_HEADROOM` 4096 → 1024 (or
   less): ~450 B of per-block chunk headroom and a smaller scratch-pool buffer, both blocked here
   only because they move the format-time `block_size` clamp.
3. Retiring the deprecated `aes256gcm-rsa` / `chacha20-rsa` algorithm spellings (accepted as
   aliases today so the refusal path can parse a legacy config; canonical names are `aes256gcm`
   and `chacha20`).
4. Field validation of an encrypted volume at scale — the item ships with cargo-level and
   CLI-level acceptance only (no cluster access this wave, per the D7 parallel-agent law).
