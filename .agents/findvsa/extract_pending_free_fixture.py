#!/usr/bin/env python3
"""Option-A (pending-free coverage) fixture extractor — Branch 3, PR 1(c).

docs/design-smo-replay-currency.md §1 sub-mechanism (ii) / §2-A: the
pending-free gates compared checkpoint GENERATION only, certifying
durability of the freeing checkpoint RECORD — never coverage of the
freeing swap/flips. Post-C'/PR-3 dev no longer produces REFUSED storm
rounds at observable rate (the C' two-phase replay closed the walks that
DETECTED the reuse; 8/8 recapture rounds CLEAN, 2026-07-16), so this
extractor pins the mechanism's PRECONDITION from real captured post-kill
bytes instead: in-window `Freed` records whose historical generation tag
the OLD mount gate would have RELEASED AT LOAD (`retire_tag <=
mounted_seq`) while their freeing entries sit at-or-past the mounted
tail — i.e. extents the §2-A mount gate must PARK, quantified per image,
plus the fallback-record root references that make releasing them the
reuse-vs-fallback §4.7 law violation.

Usage: extract_pending_free_fixture.py <capture_dir with meta{1..4}.img>
Writes JSONL rows to stdout (header first) — commit the output as
tests/fixtures/findvsa3_pending_free_window.jsonl; the in-repo test
tests/kv_smo_crash_completeness_tests.rs::
recaptured_window_pins_generation_gate_release re-asserts the pins.
"""
import contextlib
import json
import struct
import sys
import os

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import kvparse  # noqa: E402  (checksummed parsers)

PAGE = 4096
PHDR = 24
PDATA = PAGE - PHDR
EHDR = 20
LEDGER_MAGIC = 0x4B56524C  # "KVRL"
TREE_ALLOC = 4


def parse_ledger_records(img, sb):
    """All valid slots, newest-first: (seq, tail, roots=[(tree,addr,nseq)])."""
    rl_s, rl_l = sb["root_ledger"]
    recs = []
    for k in range(rl_l // PAGE):
        raw = img[rl_s + k * PAGE : rl_s + (k + 1) * PAGE]
        magic, plen = struct.unpack_from("<II", raw, 0)
        if magic != LEDGER_MAGIC or plen < 34 or 24 + plen > PAGE:
            continue
        seq = struct.unpack_from("<Q", raw, 8)[0]
        stored = struct.unpack_from("<Q", raw, 16)[0]
        if kvparse.x3 is not None:
            calc = kvparse.x3(raw[0:16] + b"\0" * 8 + raw[24 : 24 + plen])
            if calc != stored:
                continue
        pay = raw[24 : 24 + plen]
        tail, next_ino, gen, wm = struct.unpack_from("<QQQQ", pay, 0)
        n_roots = struct.unpack_from("<H", pay, 32)[0]
        if 34 + n_roots * 17 != plen:
            continue
        roots = []
        for i in range(n_roots):
            t = pay[34 + i * 17]
            addr, nseq = struct.unpack_from("<QQ", pay, 34 + i * 17 + 1)
            roots.append((t, addr, nseq))
        recs.append(dict(seq=seq, tail=tail, roots=roots))
    recs.sort(key=lambda r: -r["seq"])
    return recs


def ring_reader(img, sb):
    j_s, j_l = sb["journal"]
    pages = j_l // PAGE

    def rd(pos, n):
        out = bytearray()
        p = pos
        while n > 0:
            page = (p // PDATA) % pages
            in_page = p % PDATA
            take = min(PDATA - in_page, n)
            phys = j_s + page * PAGE + PHDR + in_page
            out += img[phys : phys + take]
            p += take
            n -= take
        return bytes(out)

    return rd


def decode_records(payload):
    """Entry payload -> [(tag, key, kind, seq, value)] (§4.4 wire format)."""
    out = []
    pos = 0
    while pos < len(payload):
        tag = payload[pos]
        pos += 1
        key_len = struct.unpack_from("<H", payload, pos)[0]
        kind = payload[pos + 2]
        seq = struct.unpack_from("<Q", payload, pos + 3)[0]
        val_len = struct.unpack_from("<I", payload, pos + 11)[0]
        pos += 15
        key = payload[pos : pos + key_len]
        val = payload[pos + key_len : pos + key_len + val_len]
        pos += key_len + val_len
        out.append((tag, key, kind, seq, val))
    return out


def main():
    cap = sys.argv[1]
    rows = []
    totals = dict(
        row="header",
        images=0,
        in_window_frees=0,
        generation_gate_released=0,
        coverage_gate_parked=0,
        fallback_root_refs=0,
        mounted_root_refs=0,
    )
    for i in (1, 2, 3, 4):
        path = os.path.join(cap, f"meta{i}.img")
        if not os.path.exists(path):
            continue
        img = open(path, "rb").read()
        sb = kvparse.read_sb(img)
        ledgers = parse_ledger_records(img, sb)
        assert ledgers, f"{path}: no valid ledger record"
        mounted = ledgers[0]
        fallback = ledgers[1] if len(ledgers) > 1 else None
        rd = ring_reader(img, sb)
        # kvparse's walk prints its census to stdout — keep the JSONL
        # stream clean by routing the diagnostics to stderr.
        with contextlib.redirect_stdout(sys.stderr):
            _, entry_seqs = kvparse.chain_walk(img, sb, mounted["tail"])
        totals["images"] += 1

        node_size = sb["node_size"]
        heap_s, _ = sb["heap"]

        def roots_ref(rec, extent):
            return any(
                (addr - heap_s) // node_size == extent for (_, addr, _) in rec["roots"]
            )

        # Per-key LWW fold of the window's allocator records (the K1 fold
        # shape the mount replay applies).
        finals = {}
        for es in entry_seqs:
            hdr = rd(es, EHDR)
            _seq, elen, _csum = struct.unpack("<QIQ", hdr)
            for tag, key, kind, rseq, val in decode_records(rd(es + EHDR, elen)):
                if (tag & 0x0F) != TREE_ALLOC:
                    continue
                extent = struct.unpack(">Q", key)[0]
                finals[extent] = (rseq, es, val)
        for extent, (rseq, es, val) in sorted(finals.items()):
            if not val or val[0] != 2:
                continue  # alloc final (or malformed — entry checksummed)
            retire_tag = struct.unpack_from("<Q", val, 1)[0]
            if retire_tag == 0:
                continue  # never-referenced sentinel: immediate reuse is sound
            gen_released = retire_tag <= mounted["seq"]
            in_window = rseq >= mounted["tail"]
            fb_ref = fallback is not None and roots_ref(fallback, extent)
            m_ref = roots_ref(mounted, extent)
            totals["in_window_frees"] += 1
            totals["generation_gate_released"] += int(gen_released)
            totals["coverage_gate_parked"] += int(in_window)
            totals["fallback_root_refs"] += int(fb_ref)
            totals["mounted_root_refs"] += int(m_ref)
            rows.append(
                dict(
                    row="free",
                    img=i,
                    extent=extent,
                    rec_seq=rseq,
                    entry_seq=es,
                    retire_tag=retire_tag,
                    mounted_seq=mounted["seq"],
                    mounted_tail=mounted["tail"],
                    generation_gate_released=gen_released,
                    fallback_root_ref=fb_ref,
                    mounted_root_ref=m_ref,
                )
            )
    print(json.dumps(totals))
    for r in rows:
        print(json.dumps(r))


if __name__ == "__main__":
    main()
