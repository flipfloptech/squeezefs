# Third-Party Notices

SqueezeFS is licensed under the Business Source License 1.1 (see
[LICENSE](LICENSE)). The repository contains the third-party components
recorded below; each remains governed by its own original license, which is
unaffected by the repository's BUSL 1.1. This file satisfies the attribution
and notice-retention obligations of those licenses.

## Fork: `crates/fuse3/` — derived from `fuse3` (MIT License)

[`crates/fuse3/`](crates/fuse3/) is a first-party maintained fork, derived
from upstream [`fuse3`](https://crates.io/crates/fuse3) crate v0.7.3 ("FUSE
user-space library async version implementation") by Sherlock Holo,
upstream repository <https://github.com/Sherlock-Holo/fuse3>, wired in via
`[patch.crates-io]` in [Cargo.toml](Cargo.toml). The fork has substantially
diverged from upstream and is maintained as first-party code, but it
inherits its provenance obligations: the derived portions remain under the
original upstream **MIT License**, the upstream license file is retained
in-tree at [`crates/fuse3/LICENSE`](crates/fuse3/LICENSE), and it is
reproduced here as its terms require:

```text
MIT License

Copyright (c) 2020 Sherlock Holo

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```

## Derived portions: JuiceFS — Apache License 2.0

The following source files contain portions derived from
[JuiceFS](https://github.com/juicedata/juicefs) (Copyright 2026 Juicedata,
Inc.) and carry retained **Apache License 2.0** headers in-file (deliberate
provenance — the NVMe-oF module rebuild's binding decision was that
retained/derived code keeps its existing Apache-2.0 header):

- `src/nvme_dev.rs`
- `src/nvmeof/initiator.rs`
- `src/nvmeof/nocow.rs`

These portions are licensed under the Apache License, Version 2.0; a copy of
the license is available at <https://www.apache.org/licenses/LICENSE-2.0>.
Per Section 4 of that license, the original copyright and license notices are
retained verbatim at the top of each listed file and must be preserved in
redistributions of those files or derivative works of them.

## Crate dependency license inventory

Audit of all resolved, non-workspace crate dependencies (`cargo metadata
--format-version 1` against `Cargo.lock`, 2026-07-18): **298 packages, zero
copyleft** — no GPL, AGPL, or license requiring copyleft obligations when
statically linked. All dependencies are available under permissive terms
(MIT / Apache-2.0 / BSD / ISC / Zlib / Unicode / Unlicense / Boost) or
file-level MPL-2.0.

| License expression | Crates |
|---|---|
| MIT OR Apache-2.0 (incl. `MIT/Apache-2.0`, `Apache-2.0 OR MIT`) | 205 |
| MIT | 48 |
| Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT | 7 |
| Apache-2.0 | 6 |
| Apache-2.0 OR ISC OR MIT | 6 |
| Unlicense OR MIT (incl. `Unlicense/MIT`) | 7 |
| ISC | 3 |
| MIT OR Apache-2.0 OR Zlib (and Zlib OR Apache-2.0 OR MIT) | 3 |
| MPL-2.0 (`colored`, `webpki-roots`) | 2 |
| MIT OR Apache-2.0 OR LGPL-2.1-or-later (`r-efi`) | 2 |
| BSD-2-Clause OR Apache-2.0 OR MIT | 2 |
| BSD-3-Clause (`subtle`) | 1 |
| BSL-1.0 — Boost Software License (`xxhash-rust`) | 1 |
| Apache-2.0 OR BSL-1.0 (`ryu`) | 1 |
| Apache-2.0 AND ISC (`ring`) | 1 |
| (MIT OR Apache-2.0) AND Apache-2.0 (`moka`) | 1 |
| (MIT OR Apache-2.0) AND Unicode-3.0 (`unicode-ident`) | 1 |
| Apache-2.0 / MIT | 1 |

Notes:

- `r-efi` is offered under a **disjunctive** choice `MIT OR Apache-2.0 OR
  LGPL-2.1-or-later`; SqueezeFS elects the MIT/Apache-2.0 terms. No LGPL
  obligation attaches.
- `BSL-1.0` is the **Boost Software License 1.0** (permissive) — not the
  Business Source License.
- The two MPL-2.0 crates (`colored`, `webpki-roots`) are used unmodified;
  MPL-2.0's obligations are file-level (source availability for modified MPL
  files only) and impose no copyleft on this repository.
