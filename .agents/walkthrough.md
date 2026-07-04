# Walkthrough: Transparent Block Data Compression & Client-Side Encryption

We have successfully implemented transparent data compression and client-side data encryption in SqueezeFS, following the `/tdd-development-workflow` principles. These properties are set permanently at format time, stored in Garnet metadata, and applied seamlessly across all write and read paths (inline, staged NVMe staging merges, and striped parallel blocks).

---

## Technical Architecture & Implementation

### 1. Compression & Encryption Engine
- **`src/crypto_compress.rs`**:
  - Implements `CryptoCompressState` containing the configured compression algorithm (`lz4`, `zstd`, or `none`) and encryption algorithm (`aes256gcm-rsa` or `chacha20-rsa`).
  - Supports automatic 2048-bit RSA key pair generation at format time if no key is provided, saving it to `squeezefs.key` locally and storing the private PEM securely in Garnet.
  - Symmetric data keys (32 bytes) and nonces (12 bytes) are randomly generated per block/payload using `SystemRandom`.
  - The symmetric data key is wrapped using the RSA public key (OAEP padding with SHA-256) and prepended to the block ciphertext along with the nonce.
  - Exposes unified handlers:
    - `process_write`: Compresses the data, then encrypts it (if encryption is enabled).
    - `process_read`: Decrypts the data, then decompresses it.

### 2. progressive Data Layout Integration
- **Inline Layout (Micro-Files < 64KB)**:
  - Writes to `inline_data:{file_path}` in Garnet are processed via `process_write` in `write_file`.
  - Reads from `inline_data` are decrypted and decompressed using `process_read` in `read_file` and `read_file_range`.
- **Staged Layout (Consolidated small files)**:
  - NVMe staged files are staged locally raw (unencrypted/uncompressed) to maximize write performance.
  - During batch consolidation (`flush_batch` in `src/cache/nvme.rs`), each file's data is individually compressed and encrypted using `process_write` before being merged into the packed block.
  - Range reads of merged staged files read the specific file's encrypted range from NVMe block storage, run `process_read` on it, and slice out the requested offset/size.
- **Striped Layout (Large files > 4MB)**:
  - Parallel block upload tasks in `write_striped` compress and encrypt the block data via `process_write` before writing block data to the backend.
  - Old blocks retrieved during RMW gap-filling are decrypted and decompressed using `process_read` before modifications are applied.
  - Blocks read in parallel from NVMe block storage are decrypted/decompressed via `process_read` before being stored in the local NVMe block cache or returned to FUSE.

### 3. CLI & Initialization
- **`src/main.rs` & `src/fuse_client.rs`**:
  - Format command extended with `--compression`, `--encrypt-algo`, and `--encrypt-key`.
  - Metadata is persisted in `squeezefs:format`. FUSE filesystem mount loads these settings on `init` and configures `DataRouter`'s `OnceCell` crypto container.

---

## Verification Results

### 1. Automated Tests
We added `test_compression_and_encryption_flow` in `tests/juicefs_alignment_tests.rs`:
- Formats a volume with `lz4` compression and `aes256gcm-rsa` encryption.
- Writes a compressible payload (exceeding inline size) and verifies it reads back with exact integrity.
- Asserts that raw inline data stored in Garnet is compressed/encrypted and doesn't leak any plaintext substrings.

### 2. Criterion Benchmarks
Added Criterion benchmarks in `benches/squeezefs_bench.rs` for `CryptoCompressState`:
- `process_write_none` (base baseline)
- `process_write_lz4` (LZ4 compression overhead)
- `process_write_zstd` (ZSTD compression overhead)
- `process_write_aes256gcm` (AES256-GCM encryption overhead)
- `process_write_both` (Combined compression & encryption)
- `process_read_both` (Combined decompression & decryption)

### 3. WSL Compilation & Test Verification
All **81 tests** compile and pass cleanly:

```bash
cargo test -- --test-threads=1
```

```
running 14 tests
test test_capacity_quota_enforcement ... ok
test test_cli_format_and_status ... ok
test test_compression_and_encryption_flow ... ok
...
test result: ok. 14 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 10.73s
```

All clippy lints (`-D warnings`) and cargo format checks pass successfully:
```bash
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt --check
```
