# Rung 10 — the `MutualTls` channel class, end to end

> **The owner's direction of 2026-10-12 (after §1–§8 landed): the record layer on Linux is the KERNEL's
> (kTLS).** The handshake, the PKI, the OpenSSL interop and the exporter-bound session key stay; after
> the handshake the traffic keys go to the `tls` ULP and the socket is plain TCP to the daemon. **The
> venue kernel has no kTLS** (`# CONFIG_TLS is not set` in 7.2.6-sqz, §9.1), so per the direction this
> landing stops at the DESIGN (§9) plus the FAIL-LOUD path (§9.3): on Linux a `mutual_tls` mount and every
> TLS dial refuse by name, never a silent fall-back to the userspace record layer, which stays the
> handshake's transport and the portable path. §4–§6 below are the userspace record layer's record (the
> comparison column of §9.6) — the two-daemon matrix and the wire's verb rows over TLS ran green under it
> and are now the skip ledger's until a kernel with `CONFIG_TLS` arrives.

Charter §8 ("remote workers over TCP/TLS with HMAC enrollment … verify-read sampling below 100 % is
admissible only on authenticated ∧ confidential"), §1.2 (the TCP/TLS stack is the sanctioned non-uring
exception); `docs/porting/sym_wire_membership_recovery.md` §1.1 channel classes (CW:987–1008: `Plaintext`
authenticated-not-confidential, `MutualTls` CA-pinned authenticated AND confidential; CW:63–71, 1054–1078,
1146–1156 a CA-less TLS config REFUSED, never downgraded; CW:1010–1051 the ladder keys on
`authenticated()` ∧ `confidential()`, never on "transport == tls"); `ops_jobs_lifecycle.md` §7: the
verify-permille knob honoured only under mTLS. Decision 0003 §O; decision 0002 rows 486–488.

## 1. The engine decision (0003 §O, first row)

**Written in-tree over `std.crypto`'s primitives; nothing vendored.** The survey that decided it:

| Candidate | TLS 1.3 | Server | Client cert | Exporter | Verdict |
|---|---|---|---|---|---|
| `std.crypto.tls` (Zig 0.17) | yes | **no** | **no** (`CertificateRequest` → `TlsUnexpectedMessage`, `Client.zig:604/707/826`) | **no** | not viable as-is: the server, the client-auth arm and the exporter had to be written whichever way |
| BearSSL (vendored static) | **no** (1.2 only) | yes | yes | yes (RFC 5705) | fails the TLS 1.3 requirement |
| mbedTLS 3.6.4+ (vendored static) | yes | yes | yes | yes (`mbedtls_ssl_export_keying_material`, 2025-06) | viable; ~100 C files, Apache-2.0, a config whose job is to disable TLS 1.2 / RSA / tickets / 0-RTT / renegotiation / ALPN, translate-c bindings, a second allocator discipline |
| **in-tree, one profile** | yes | yes | yes | yes (§7.5) | **chosen**: `TLS_AES_128_GCM_SHA256` + X25519 + Ed25519, server and client, mutual authn under the pinned CA, KeyUpdate, close_notify; every primitive the one `std.crypto.tls.Client` already uses |

The "measure both before choosing if both are viable" rule did not bind: the std arm is not viable without
the write, and mbedTLS could only have been measured after the same integration work. What WAS measured is
the chosen engine against the plaintext wire and against an external oracle (§3, §4).

Module: `src/wire/tls.zig` (the engine, the identity, the posture, the record layer as a `session.Link`),
`src/wire/pki.zig` (the PKI). Deliberately absent: TLS 1.2, resumption/tickets (a foreign server's
`NewSessionTicket` is ignored), 0-RTT, HelloRetryRequest (one group, offered by every peer we accept),
renegotiation, ALPN, SNI, middlebox CCS (received ones dropped per RFC 8446 D.4).

## 2. The PKI's shape (0003 §O, third row)

Ed25519 everywhere, issued by the binary:

```
squeezefs key tls-init  [--name CA-NAME] [--force] <dir>          → <dir>/ca.pem (0644), <dir>/ca.key (0600)
squeezefs key tls-issue --node NAME [--out DIR] [--force] <ca-dir> → <out>/node.pem (0644), <out>/node.key (0600)
```

Minimal X.509 v3 DER (`pki.issue`, a backwards DER writer): v3, a 16 B serial (top bit clear), the Ed25519
AlgorithmIdentifier, `CN=` issuer/subject, UTCTime through 2049 / GeneralizedTime after (RFC 5280
§4.1.2.5), the Ed25519 SPKI, `basicConstraints` CA:TRUE + `keyUsage` keyCertSign|cRLSign on the CA,
`keyUsage` digitalSignature on a node. The CA ten years, a node one year, `notBefore` backdated five minutes
(`keys.NOT_BEFORE_SKEW_S`). PEM files; the key the 48 B PKCS#8 v1 form. The chain is exactly ONE leaf under
the pinned CA. On the venue: `openssl verify -CAfile ca.pem node.pem: OK`; `openssl x509 -text` reads every
field (Ed25519, the two extensions, the validity).

`std.crypto.Certificate.parse` indexes its buffer unchecked (a 0-byte input panics at `Certificate.zig:973`
under the fuzzer) — `pki.Screen` walks exactly the schema it will walk, bounds-checked, before it runs
(`fuzz: the PKI decoders`).

## 3. The wire (0003 §O, rows 4–7)

- **Posture:** `SQUEEZEFS_WIRE_CHANNEL = plaintext | mutual_tls` (switch), `SQUEEZEFS_WIRE_TLS_CA / _CERT /
  _KEY` (config, defaults `/etc/squeezefs/tls/{ca.pem,node.pem,node.key}`). `mount`, the admin lane and
  `job worker` install it once BEFORE any fork or dial; the identity crosses the daemon fork in memory. The
  CA and the certificate: `O_NOFOLLOW`, a regular file, owner ∈ {euid, root}, `mode & 0o022 == 0`; the key:
  `transform.linux_keys.readKeyFile`'s 0600 law. A CA-less or malformed `mutual_tls` refuses on the
  console naming the file and the law (`tls.writeInstallRefusal`).
- **Listener** (`sqz-wire-conn`): under `mutual_tls` the TLS handshake is the connection thread's first
  act; then the ladder with `binding` = `TLS-Exporter("squeezefs-cluster-wire/v1/binding", "", 32)`;
  `Framer.admitOn(key, .mutual_tls)`. Refusals counted by class and logged loud.
- **Dialer:** the same in the client role; a refusal is `cluster_wire_tls_dial_refused` + a warn naming it;
  the mux's halves (reader thread / writers under the lock) own the read / write keys respectively.
- **Class mismatches, both ends loud:** a TLS dialer at a plaintext listener reads the Challenge's frame
  magic where a record header should be (`PeerPlaintext`); a plaintext dialer at a TLS listener is SILENT
  (the ladder's coordinator speaks first, TLS's client does) — the listener answers at HALF the dial bound
  (`first_record_timeout_ms` = 5 s: the refusal must land before the dialer's own 10 s Challenge wait) with
  a plaintext `Refused` naming the class (`PeerSilent`, `cluster_wire_plaintext_refused`), and the dialer
  logs "refused before its challenge: this channel is MutualTls".
- **The frame path:** `Conn.link()` is a `session.Link`; `Session.sendSealed`'s one `writeAll` per frame
  becomes ⌈len / 2^14⌉ records, each one `sendto`; the receive side fills the session's reads from the
  decrypted window. No allocation per record: the `Conn` (one `gpa.create` per connection, at the
  handshake, as the `Session` already is) holds the 16 KiB ciphertext buffer, the plaintext window and the
  write record. The per-frame HMAC stays (defence in depth); `cluster_wire_mac_failures` is unchanged.
- **KeyUpdate:** sent at 2^23 records (half RFC 8446 §5.5's AES-GCM bound) or when the peer requested one,
  under the writer's own serialization; `cluster_wire_tls_key_updates`.

## 4. Tests

Portable (`zig build test` on macOS, 2095 pass):

- `pki.zig`: civil dates (epoch, a leap day, the 2049 boundary); CA + node issue → parse under
  `std.crypto.Certificate` → verify; the CA self-signed; a foreign CA of the SAME NAME `Untrusted`; expired /
  not-yet-valid; a tampered byte; a GeneralizedTime validity; the name law; PEM round trips (certificate,
  PKCS#8), the label mismatch, the v2 key form refused, trailing bytes refused; `fuzz: the PKI decoders`.
- `tls.zig` (over `ThreadPipe`, an `Io.Condition` byte pipe between two threads): a mutual handshake
  (both established, the exporter agrees and differs per label, the peer names are the CNs), records both
  ways, a 1 MiB + 80 B frame in 65 records, a KeyUpdate from the server, a peer-REQUESTED update honoured
  by the client's next write, close_notify = EOF; refusals — a client under a foreign CA refuses the server
  first (`Untrusted`), an expired leaf (`Expired`, the server's clock), a not-yet-valid leaf, a server under
  a foreign CA refused by the client, a plaintext frame at the server and a Challenge at the client
  (`PeerPlaintext`), garbage (`PeerNotTls`); the identity's own bad material refused at install; the
  posture; the handshake decoders and `fuzz: the TLS handshake decoders`.
- `session.zig`: two sessions from one secret and one nonce pair under two bindings hold different keys — a
  frame sealed on one channel is `MacFailed` on the other; `confidential()` true only on `mutual_tls`.
- `args.zig`: the two verbs parse; `ops/jobs/wire.zig`: the verify-permille ladder over `tls.Channel`.

Linux (`scripts/venue test`):

- **`tls e2e` — the OpenSSL 3.0.13 oracle:** our client against `openssl s_server -tls1_3 -Verify 1 -rev`
  under the PKI: `CONNECTION ESTABLISHED`, `Ciphersuite: TLS_AES_128_GCM_SHA256`, `Peer certificate: CN =
  node-b`, `Signature type: Ed25519`, `Verification: OK`, the `-rev` echo through the record layer.
  `openssl s_client -Verify … -keymatexport squeezefs-cluster-wire/v1/binding -keymatexportlen 32` against
  OUR server: admitted (`node-b`), the echo crosses, and OpenSSL's `Keying material:` equals our server's
  `binding()` byte for byte (the exporter oracle). An `s_client` under the foreign CA: `PeerCertificateUntrusted`
  on our side, `tlsv1 alert unknown ca` on OpenSSL's. (3.0's `s_server` prints no keying material in any
  body mode; the exporter oracle is the `s_client` row.)
- **`wire e2e over MutualTls`:** a listener + a `dialer.Client` under one CA — 1,000 verbs on one
  connection, `authenticated() ∧ confidential()`, 2 handshakes + 1 admission counted; a client under the
  foreign CA → `HandshakeRefused`, `tls_dial_refused` +1, the listener's `tls_refused_untrusted` +1 (the
  peer's `unknown_ca` alert is the mirror refusal, `PeerRefusedOurCertificate`); a Plaintext dialer →
  `Refused` by name after 5,009 ms, `plaintext_refused` +1; a MutualTls dialer at a Plaintext listener →
  `HandshakeRefused`, `tls_dial_refused` +1, and a plaintext client there still authenticated, not
  confidential.
- `tcp e2e` (the Plaintext rows) unchanged and green.

## 5. The two-daemon row (`smoke.zig`, "THE TWO-DAEMON MATRIX over MutualTls")

Through the binary: `key tls-init --name smoke-ca`, `tls-issue --node node-a`, `--node node-b`; a FOREIGN
CA of the same name with `node-c`. A mounts under `SQUEEZEFS_WIRE_CHANNEL=mutual_tls` with node-a's files;
B with node-b's.

```
smoke: mtls matrix: B joined over TLS — A: handshakes 1, sessions admitted 1; B: handshakes 2, wire verbs 3
smoke: mtls matrix: 200 MiB dd on B in 674 ms, sha from A equal; wire over TLS — A: frames received 51,
       bytes received 9128, grants 3; B: frames sent 51, bytes sent 9128, key updates 0;
       MAC failures 0, refusals 0 on both
smoke: mtls matrix: a daemon under a FOREIGN CA: mount exited with code 1; A counts tls_refused_untrusted 1
smoke: mtls matrix: C's log: warning(squeezefs): cluster wire: peer at 127.0.0.1:35489 refused at the TLS
       handshake: the peer's certificate is not under this set's CA
smoke: mtls matrix: a PLAINTEXT daemon: mount exited with code 1; A counts plaintext_refused 1
```

Asserted: A's and B's logs name the class and the node (`channel class mutual_tls — node 'node-a' under CA
'smoke-ca'`); `.stats` on both: `cluster_wire_channel_mutual_tls` 1, `cluster_wire_tls_handshakes ≥ 1`,
every refusal counter 0, `cluster_wire_mac_failures` 0, `cluster_wire_admissions_refused` 0, the
must-stay-0 set; A's token grant + recall on B's file (A's third read fresh); B's `mkdir` under A's root
(a cross-owner intent, 0 stuck); the 200 MiB `dd` on B — `block_grants ≥ 1` on A, the sha from A EQUAL to
B's (the Plaintext matrix's result); then the negative half: C (foreign CA) exits 1 with its log naming the
CA and A counting `tls_refused_untrusted`; D (`SQUEEZEFS_WIRE_CHANNEL=plaintext`) exits 1 with its log
carrying "this channel is MutualTls" and A counting `plaintext_refused`; B's clean leave; every file
visible from A after it.

A refused joiner's LAST console line is still the join ladder's generic `cannot open metadata set …:
device I/O failed (IO, terminal)` — the TLS reason is the warning before it. The ladder's `verbError` maps
`HandshakeRefused` to `Io` (`join_ladder.zig`, the symmetric plane's); a `Refused` arm there is the one-line
polish owed to that plane's owner.

## 6. Plaintext vs MutualTls on the venue

`tls e2e: SCOPING` (ReleaseSafe, loopback TCP, 512 MiB streams, one connection; the 9-vCPU aarch64 VM —
a scoping row, not the reference box):

| | plaintext | mutual_tls |
|---|---|---|
| connect + handshake (50 dials) | 10 µs | 509 µs |
| 64 KiB writes | 21.48 GB/s | 2.01 GB/s |
| 1 MiB writes | 9.98 GB/s | 1.99 GB/s |

The engine's record throughput is the AEAD's (`std.crypto`'s AES-128-GCM on this VM, one core); the wire
moves verbs and token pages — the 200 MiB `dd` across the TLS wire costs 51 frames / 9,128 B each way and
the dd's 674 ms is the data plane's, not the wire's. The handshake's 0.5 ms is a per-connection cost
against the member's six standing sessions; `connect+handshake` at the Debug profile was 4.7 ms (the
measurement that MUST be made in ReleaseSafe).

## 7. Knobs, counters, rails

Knobs: `SQUEEZEFS_WIRE_CHANNEL` (enum, switch), `SQUEEZEFS_WIRE_TLS_CA`, `SQUEEZEFS_WIRE_TLS_CERT`,
`SQUEEZEFS_WIRE_TLS_KEY` (config) — read by `wire/tls.zig`. Counters (`wire/stats.zig`, the `wire` family):
`cluster_wire_channel_mutual_tls` (gauge), `cluster_wire_tls_handshakes`, `cluster_wire_tls_refused_untrusted`,
`cluster_wire_tls_refused_expired`, `cluster_wire_tls_refused_protocol`, `cluster_wire_plaintext_refused`,
`cluster_wire_tls_key_updates`, `cluster_wire_tls_dial_refused`, `cluster_wire_ktls_refused` (§9.3). Rails:
the knob census, the fuzz census
(two new fuzz tests naming every new `parse*`/`decode*`), the literal rail (every DER/TLS constant
documented on its line; the allowlist rebased across the CLI line shifts, no new rows).

## 8. Owed

Superseded by §10 after the owner's kTLS direction.

## 9. The kernel record layer (kTLS): facts, design, the fail-loud path

### 9.1 The venue kernel's facts (7.2.6-sqz, 2026-10-12)

| Probe | Result |
|---|---|
| `zcat /proc/config.gz \| grep CONFIG_TLS` | **`# CONFIG_TLS is not set`**; no `CONFIG_TLS_DEVICE`, no `CONFIG_TLS_TOE` |
| `CONFIG_MODULES` | `# CONFIG_MODULES is not set` — a monolithic kernel: no `modprobe`, no `/lib/modules`, the ULP cannot be loaded later |
| `/proc/sys/net/ipv4/tcp_available_ulp` | empty (one newline) — no `tls` ULP registered |
| `/proc/kallsyms` `tls_*` | 15 symbols, ALL `net/handshake`'s upcall helpers (`tls_handshake_accept/done/cancel`, `tls_client_hello_x509`, `tls_alert_send`, `tls_get_record_type`, … — `CONFIG_NET_HANDSHAKE=y`); none of `tls_set_sw_offload` / `tls_sw_sendmsg` / `tls_sw_recvmsg` / `tls_set_device_offload` |
| `setsockopt(TCP_ULP, "tls")` on an established loopback pair (`tls.ktls.probeKernel`) | fails → `absent` |
| TLS 1.3 + rekey acceptance (6.14+ `TLS_TX`/`TLS_RX` re-setsockopt after a KeyUpdate) | **not testable here** — the kernel series (7.2) is past 6.14, so the API is expected present once `CONFIG_TLS=y`; the design below assumes it and the owed test row pins it |

**The venue change the kTLS record layer needs:** `CONFIG_TLS=y` (and `CONFIG_TLS_DEVICE=y` for NIC
offload) in the sqz kernel config. Everything below is designed against the upstream uapi
(`include/uapi/linux/tls.h`) and recorded so the data path lands as one commit when the kernel does.

### 9.2 The design: handshake in-tree, record layer = kTLS

1. **After the handshake** (`Conn.armRecordLayer`, both roles): `setsockopt(fd, SOL_TCP, TCP_ULP, "tls", 3)`
   on the ESTABLISHED socket (`linux.TCP.ULP` = 31), then `setsockopt(fd, SOL_TLS = 282, TLS_TX = 1,
   &tls12_crypto_info_aes_gcm_128)` with OUR application traffic keys and `TLS_RX = 2` with the PEER's:
   `{ version = TLS_1_3_VERSION (0x0304), cipher_type = TLS_CIPHER_AES_GCM_128 (51), salt = iv[0..4],
   iv = iv[4..12], key = key[16], rec_seq = 0 }` (`tls.ktls.cryptoInfo(Traffic)`, 40 B, comptime-pinned).
   The handshake's records (ClientHello … Finished) are the userspace layer's; nothing of the application
   epoch has been read into userspace when `TLS_RX` is installed (the record reader consumes exactly one
   record per read, so the socket's queue holds only application-epoch records and the kernel's `rec_seq`
   starts at 0 exactly). Optionally `TLS_RX_EXPECT_NO_PAD` (4): neither this engine nor OpenSSL pads.
   `confidential()` = both halves installed (the owner's law, `Conn.confidential`, already in code).
2. **From then on the socket is plain TCP to the daemon**: the session's `TcpLink` writes frames with
   `sendto` as the Plaintext class does (one record per ≤ 16 KiB; the kernel seals), and READS with
   `recvmsg` carrying a control buffer — never plain `recv`: a non-data record arriving without
   `msg_control` is `-EIO` (`tls_record_content_type` + `MSG_CTRUNC`). Each `recvmsg` returns bytes of ONE
   record type; the cmsg `SOL_TLS / TLS_GET_RECORD_TYPE (2)` carries it:
   - `application_data` (23): the frame bytes — the session's receive discipline unchanged;
   - `alert` (21): `close_notify` → `error.Eof` (orderly), any other → `error.TransportFailed`, both
     counted (`cluster_wire_tls_alerts_{close_notify,fatal}`, owed) and logged loud;
   - `handshake` (22): `KeyUpdate` (type 24) → derive the peer's next RX secret
     (`HKDF-Expand-Label(secret, "traffic upd", "", 32)`), re-`setsockopt(TLS_RX)` with the new key and
     `rec_seq = 0` (the 6.14 rekey), count `cluster_wire_tls_key_updates`; `update_requested` sets the
     writer's pending flag; `NewSessionTicket` (type 4) → ignored and counted
     (`cluster_wire_tls_tickets_ignored`, owed); anything else → `error.TransportFailed`;
   - any other type → `error.TransportFailed`.
   The owner's wording "`IORING_OP_RECVMSG`": the Plaintext wire's frame path is NOT on io_uring — it is
   the blocking-syscall path on the wire's OS threads by design (spec §1.1 "deliberately not io_uring",
   CW:100–102; charter §1.2's TCP/TLS exception; §9 thread-per-connection). The kTLS socket rides the SAME
   path, with `recvmsg(2)` where the Plaintext class uses `recvfrom(2)`; moving the wire's frames onto
   io_uring (`IORING_OP_RECVMSG`/`SEND_ZC`) is a separate decision about the Plaintext class too, named
   for the owner in the report, not taken here.
3. **Our KeyUpdate** (the 2^23-record ceiling, or the peer's request, honoured at the writer's next write
   under its own serialization): `sendmsg` of the 5 B KeyUpdate with cmsg `SOL_TLS / TLS_SET_RECORD_TYPE
   (1) = handshake` under the CURRENT key, then re-`setsockopt(TLS_TX)` with the next secret's key/iv and
   `rec_seq = 0`. The record count the ceiling judges is ⌈len / 2^14⌉ per write (the kernel splits as the
   userspace layer did).
4. **close_notify**: `sendmsg` of the 2 B alert with `TLS_SET_RECORD_TYPE = alert`, then the socket close
   (today's `linkClose`, over the kernel).
5. **The dead-on-arrival peek** stays on the raw socket; under kTLS the kernel presents the DECRYPTED
   queue to `MSG_PEEK`, so a queued close_notify is a control record the peek cannot see as EOF — the
   mux reader's `recvmsg` classifies it (the §O row stands).
6. **Tests owed with the data path** (each a `wire e2e` row gated on `tls.ktls.probeKernel() == .present`):
   the 1,000-verb row over kTLS (`authenticated() ∧ confidential()`, 2 handshakes + 1 admission); a
   `close_notify` from the peer closes the session loud and counted; a fatal alert closes loud and
   counted; a KeyUpdate from the peer re-keys RX and the stream continues (and a `TLS_RX`
   re-setsockopt is ACCEPTED — the 6.14 rekey pin); our own KeyUpdate at the ceiling re-keys TX; a
   `NewSessionTicket` from `openssl s_server` is ignored and counted; the OpenSSL oracle rows over kTLS;
   the two-daemon matrix (its contract and its userspace-layer output are §5).

### 9.3 The fail-loud path (this build)

- **At the posture install** (`tls.linux_files.installFromEnv`, every `mutual_tls` mount / admin lane /
  `job worker`, BEFORE the fork): `tls.ktls.probeKernel()` — a loopback TCP pair, one `TCP_ULP "tls"`
  setsockopt, everything closed. `absent` → `InstallError.KernelTlsMissing` and the console line:
  `SQUEEZEFS_WIRE_CHANNEL=mutual_tls needs kernel TLS and this kernel has no \`tls\` ULP
  (/proc/sys/net/ipv4/tcp_available_ulp: ''): build it with CONFIG_TLS=y (CONFIG_TLS_DEVICE=y for NIC
  offload) or load the tls module — the MutualTls record layer on Linux is the kernel's, never a userspace
  fall-back`; `present` → `KernelTlsRecordLayerOwed` ("this build's kTLS data path is owed … never a
  silent fall-back"). Either way the mount exits 1 and nothing is mounted (the smoke row).
- **At every connection** (defence in depth; `RecordLayer.ktls` requested by the listener and the dialer
  with the socket): after a GOOD handshake `armRecordLayer` attaches the ULP → `KtlsUnavailable` /
  `KtlsRecordLayerOwed`, a fatal `internal_error` alert to the peer, the refusal counted
  (`cluster_wire_ktls_refused`) and logged on BOTH ends, the session never admitted. Measured on the venue
  (`wire e2e over MutualTls, the kernel record layer's law`): one MutualTls dial → 2 counts (the dialer
  arms after its Finished and refuses first; the listener after reading that Finished), 0 admissions, 0
  verbs served; the two class-mismatch rows (a Plaintext dialer answered by name at 5,003 ms; a MutualTls
  dialer at a Plaintext listener) run BEFORE any record layer and still hold; then
  `skip[no_kernel_feature]` for the verb rows.
- **The userspace record layer** (`Conn.linkRead/linkWrite`) is reachable only with
  `RecordLayer.userspace`, which no Linux product path requests: it is the handshake's transport
  everywhere and the application path of the portable tests and the engine's OpenSSL oracle rows (which
  prove the handshake, the certificate verdicts and the exporter — all of which the kTLS design reuses).

### 9.4 The smoke row on this kernel

```
smoke: mtls: the PKI through the binary; openssl: …/pki/a/node.pem: OK  …/pki/b/node.pem: OK  1
smoke: mtls: mount under mutual_tls: exited with code 1; squeezefs mount: cluster wire:
       SQUEEZEFS_WIRE_CHANNEL=mutual_tls needs kernel TLS and this kernel has no `tls` ULP
       (/proc/sys/net/ipv4/tcp_available_ulp: ''): build it with CONFIG_TLS=y (CONFIG_TLS_DEVICE=y for NIC
       offload) or load the tls module — the MutualTls record layer on Linux is the kernel's, never a
       userspace fall-back
skip[no_kernel_feature] cli/smoke.zig: the two-daemon matrix over kernel TLS needs CONFIG_TLS (the tls
       ULP): this kernel has none; the fail-loud rows ran
```

Asserted: the five PKI verbs through the binary (a second `tls-init` without `--force` refuses "never
overwritten"); OpenSSL verifies node-a and node-b under the CA and FAILS node-c (the foreign CA of the same
name); a `mutual_tls` mount refuses on the console naming the ULP and the remedy, nothing mounted; a CA-less
`mutual_tls` refuses naming the CA ("never downgraded"); node-b's key under node-a's certificate refuses
("does not match"); the same set under `plaintext` serves and unmounts clean (`cluster_wire_channel_mutual_tls`
0). The two-daemon matrix (§5) is the skip ledger's.

### 9.5 zcrx and kTLS (hardware-owed row, beside `rung-08b1-zcrx-lane.md`'s)

`RECV_ZC` delivers the NIC's raw TCP payload pages into the zcrx area; SOFTWARE kTLS (`tls_sw_*`) decrypts
in the socket's receive path, which zcrx bypasses — the two cannot share a socket. Only NIC TLS RX offload
(`CONFIG_TLS_DEVICE=y`, `ethtool -K <nic> tls-hw-rx-offload on`, a NIC that decrypts in line) delivers
plaintext payload pages zcrx can hand up. The cluster wire's sockets are never zcrx sockets today (zcrx is
the NVMe/TCP READ lane to an nvmet target, charter §6.3; nvme-tcp's own TLS is PSK-based kTLS and not this
wire), so there is no product site to refuse at; the rule binds the day a `RECV_ZC` path touches a
`MutualTls` socket: **refuse (counted) unless the device reports TLS RX offload for that flow** — a
hardware-owed row like the zcrx lane's ifq/steering rows.

### 9.6 Measurements: Plaintext vs MutualTls

| | plaintext | mutual_tls, userspace record layer (§6) | mutual_tls, kTLS |
|---|---|---|---|
| connect + handshake | 10 µs | 509 µs (the handshake is in-tree either way) | = userspace (the handshake is the same code) — to measure |
| 64 KiB writes | 21.48 GB/s | 2.01 GB/s | owed: no `CONFIG_TLS` on 7.2.6-sqz |
| 1 MiB writes | 9.98 GB/s | 1.99 GB/s | owed |
| daemon CPU per GB | — | — (not measured before the direction) | owed — the point of kTLS: the AEAD on the kernel's softirq/`sendmsg` path, `SEND_ZC` where the frame body is pinned |

The kTLS column's harness is written into the design: the scoping row gains `CLOCK_PROCESS_CPUTIME_ID`
around the transfer (CPU-s per GB) and runs the three columns when the probe reads `present`.

## 10. Owed (revised)

- **The kernel:** `CONFIG_TLS=y` (+ `CONFIG_TLS_DEVICE=y`) in the sqz kernel series — the venue change.
- **The kTLS data path** (§9.2) and its tests (§9.2.6), one commit against that kernel; the measurement
  column (§9.6); the two-daemon matrix re-armed (§5's contract).
- The join ladder's console line for a refused channel (`verbError`: `HandshakeRefused → Refused`) — the
  symmetric plane's owner.
- A durable channel class in `set_config` if the owner wants the set, not the node, to declare it (0003 §O).
- The wire's frame path on io_uring (`IORING_OP_RECVMSG` / `SEND_ZC`) is a decision about the Plaintext
  class as much as the TLS one (spec §1.1 keeps the wire off io_uring by design) — the owner's call.
- Revocation / intermediates are out of scope by design (one leaf under one CA; rotation = re-issue).
