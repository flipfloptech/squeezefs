# Rung 10 — the `MutualTls` channel class, end to end

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
`cluster_wire_tls_key_updates`, `cluster_wire_tls_dial_refused`. Rails: the knob census, the fuzz census
(two new fuzz tests naming every new `parse*`/`decode*`), the literal rail (every DER/TLS constant
documented on its line; the allowlist rebased across the CLI line shifts, no new rows).

## 8. Owed

- The join ladder's console line for a refused channel (`verbError`: `HandshakeRefused → Refused`) — the
  symmetric plane's owner.
- A durable channel class in `set_config` if the owner wants the set, not the node, to declare it (0003 §O).
- Revocation / intermediates are out of scope by design (one leaf under one CA; rotation = re-issue).
