#!/usr/bin/env python3
"""Offline v3 meta-volume forensics for FIND-VS-A.

Parses: superblock extents, both root-ledger slots, journal ring census
(per-page header lap/feo validity + chain walk from a given tail), and
classifies byte offsets of a searched name (ring / ledger / bitmap / heap).
Checksums use xxhash's xxh3_64 (pip module 'xxhash')."""
import struct
import sys

try:
    import xxhash

    def x3(b):
        return xxhash.xxh3_64_intdigest(b)
except ImportError:
    print("WARNING: no xxhash module; checksum validation skipped")
    x3 = None

PAGE = 4096
PHDR = 24
PDATA = PAGE - PHDR
EHDR = 20
MAGIC = 0x4B564A50


def read_sb(img):
    sb = img[:4096]
    assert sb[0:8] == b"METALV01", "bad magic"
    ver = struct.unpack_from("<I", sb, 8)[0]
    node_size = struct.unpack_from("<I", sb, 12)[0]
    rl_s, rl_l = struct.unpack_from("<QQ", sb, 32)
    j_s, j_l = struct.unpack_from("<QQ", sb, 48)
    ab_s, ab_l = struct.unpack_from("<QQ", sb, 64)
    h_s, h_l = struct.unpack_from("<QQ", sb, 80)
    return dict(
        ver=ver,
        node_size=node_size,
        root_ledger=(rl_s, rl_l),
        journal=(j_s, j_l),
        bitmap=(ab_s, ab_l),
        heap=(h_s, h_l),
    )


def classify(off, sb):
    for name in ("root_ledger", "journal", "bitmap", "heap"):
        s, l = sb[name]
        if s <= off < s + l:
            return name, off - s
    return "other", off


def ring_census(img, sb, tail=None):
    j_s, j_l = sb["journal"]
    pages = j_l // PAGE
    ring = img[j_s : j_s + j_l]
    laps = {}
    valid = 0
    invalid = []
    for k in range(pages):
        hdr = ring[k * PAGE : k * PAGE + PHDR]
        magic, lap, feo = struct.unpack_from("<IIH", hdr, 0)
        csum = struct.unpack_from("<Q", hdr, 16)[0]
        ok = magic == MAGIC and (x3 is None or x3(hdr[0:16]) == csum)
        if ok:
            valid += 1
            laps[lap] = laps.get(lap, 0) + 1
        else:
            invalid.append(k)
    print(
        f"  ring: pages={pages} valid_hdrs={valid} laps={dict(sorted(laps.items()))} "
        f"invalid_pages={len(invalid)}{invalid[:8] if invalid else ''}"
    )
    return pages


def chain_walk(img, sb, tail):
    """Replicate replay_scan_image chain-primary walk; return entries found,
    the head reached, and the first few failures."""
    j_s, j_l = sb["journal"]
    pages = j_l // PAGE
    L = pages * PDATA

    def rd(pos, n):
        # logical pos -> physical bytes (may span pages)
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

    chain_end = tail + L
    window_base = tail - (tail % PDATA)
    slots = pages + 1 if window_base < tail else pages
    entries = 0
    seqs = []
    pos = tail
    dropped = 0
    pending = 0
    next_slot = 0
    fails = []
    while True:
        if pos is not None and pos < chain_end:
            hdr = rd(pos, EHDR)
            seq, elen, csum = struct.unpack("<QIQ", hdr)
            ok = seq == pos and 0 < elen and EHDR + elen <= 128 * 1024
            if ok:
                payload = rd(pos + EHDR, elen)
                if x3 is not None:
                    calc = x3(hdr[0:12] + payload)
                    ok = calc == csum
            if ok:
                dropped += pending
                pending = 0
                entries += 1
                seqs.append(pos)
                pos = pos + EHDR + elen
            else:
                fails.append((pos, seq, elen))
                pending += 1
                next_slot = (pos - (pos % PDATA) - window_base) // PDATA + 1
                pos = None
        elif pos is not None:
            break
        else:
            found = None
            while next_slot < slots:
                pos_i = window_base + next_slot * PDATA
                next_slot += 1
                page = (pos_i // PDATA) % pages
                hdr = img[j_s + page * PAGE : j_s + page * PAGE + PHDR]
                magic, lap, feo = struct.unpack_from("<IIH", hdr, 0)
                csum = struct.unpack_from("<Q", hdr, 16)[0]
                ok = magic == MAGIC and (x3 is None or x3(hdr[0:16]) == csum)
                if ok and lap == pos_i // L:
                    if feo == 0xFFFF:
                        continue
                    if PHDR <= feo < PAGE:
                        found = pos_i + feo - PHDR
                        break
                    pending += 1
                else:
                    pending += 1
            if found is None:
                break
            pos = found
    print(
        f"  chain from tail={tail}: entries={entries} dropped_confirmed={dropped} "
        f"trailing_pending={pending} head={seqs[-1] if seqs else tail} "
        f"first_fails={fails[:4]}"
    )
    return entries, seqs


def parse_ledger(img, sb):
    rl_s, rl_l = sb["root_ledger"]
    # slots of unknown internal layout; dump first 96 bytes of each 4 KiB slot
    n = rl_l // 4096
    recs = []
    for k in range(min(n, 8)):
        raw = img[rl_s + k * 4096 : rl_s + k * 4096 + 96]
        recs.append(raw.hex())
    return recs


def main():
    imgpath = sys.argv[1]
    name = sys.argv[2].encode() if len(sys.argv) > 2 else None
    tail = int(sys.argv[3]) if len(sys.argv) > 3 else None
    img = open(imgpath, "rb").read()
    sb = read_sb(img)
    print(
        f"{imgpath}: ver={sb['ver']} node={sb['node_size']} journal={sb['journal']} "
        f"ledger={sb['root_ledger']} heap={sb['heap']}"
    )
    ring_census(img, sb)
    if name:
        off = -1
        hits = []
        while True:
            off = img.find(name, off + 1)
            if off < 0:
                break
            hits.append(classify(off, sb))
        print(f"  name {name.decode()}: {len(hits)} hit(s): {hits[:10]}")
    if tail is not None:
        chain_walk(img, sb, tail)


if __name__ == "__main__":
    main()

def parse_ledger_slots(img, sb):
    rl_s, rl_l = sb["root_ledger"]
    best = None
    out = []
    for k in range(rl_l // 4096):
        raw = img[rl_s + k * 4096 : rl_s + (k + 1) * 4096]
        magic, plen = struct.unpack_from("<II", raw, 0)
        seq = struct.unpack_from("<Q", raw, 8)[0]
        if magic != 0x4B56524C or 24 + plen > 4096:
            continue
        if x3 is not None:
            csum = struct.unpack_from("<Q", raw, 16)[0]
            calc = xxhash.xxh3_64(raw[:16] + b"\x00" * 8 + raw[24 : 24 + plen]).intdigest()
            if calc != csum:
                out.append((k, seq, "BADSUM"))
                continue
        tail, next_ino, abg, nsw = struct.unpack_from("<QQQQ", raw, 24)
        nroots = struct.unpack_from("<H", raw, 56)[0]
        out.append((k, seq, dict(tail=tail, next_ino=next_ino, nsw=nsw, nroots=nroots)))
        if best is None or seq > best[0]:
            best = (seq, tail)
    for o in out:
        print("   slot", o)
    return best

TREE_DENTRIES = 2

def parse_entry_payload(payload):
    """-> list of (tree_id, kind, seq, key, val)"""
    out = []
    i = 0
    while i < len(payload):
        tree_id = payload[i]; i += 1
        if i + 15 > len(payload):
            break
        klen, kind, seq, vlen = struct.unpack_from("<HBQI", payload, i)
        i += 15
        if i + klen + vlen > len(payload):
            break
        key = payload[i : i + klen]; i += klen
        val = payload[i : i + vlen]; i += vlen
        out.append((tree_id, kind, seq, key, val))
    return out


def ring_union_census(img, sb):
    """Brute-recover every physically-present entry from every page chain
    start; dedup by seq. Returns {seq: (elen, payload)}."""
    j_s, j_l = sb["journal"]
    pages = j_l // PAGE
    L = pages * PDATA

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

    found = {}
    for k in range(pages):
        hdr = img[j_s + k * PAGE : j_s + k * PAGE + PHDR]
        magic, lap, feo = struct.unpack_from("<IIH", hdr, 0)
        csum = struct.unpack_from("<Q", hdr, 16)[0]
        if magic != MAGIC or (x3 and x3(hdr[0:16]) != csum):
            continue
        if feo == 0xFFFF or not (PHDR <= feo < PAGE):
            continue
        pos = lap * L + k * PDATA + (feo - PHDR)
        while True:
            if pos in found:
                pos += EHDR + found[pos][0]
                continue
            h = rd(pos, EHDR)
            if len(h) < EHDR:
                break
            seq, elen, csum2 = struct.unpack("<QIQ", h)
            if seq != pos or elen == 0 or EHDR + elen > 128 * 1024:
                break
            payload = rd(pos + EHDR, elen)
            if x3 and x3(h[0:12] + payload) != csum2:
                break
            found[pos] = (elen, payload)
            pos += EHDR + elen
    return found


def dentry_names(found):
    """{name_bytes: [(seq, kind, parent_ino)]}"""
    names = {}
    for seq, (_l, payload) in found.items():
        for tree_id, kind, rseq, key, val in parse_entry_payload(payload):
            if tree_id != TREE_DENTRIES or len(val) < 10:
                continue
            child_ino, ftype, nlen = struct.unpack_from("<QBB", val, 0)
            name = val[10 : 10 + nlen]
            parent = struct.unpack_from(">Q", key, 0)[0] if len(key) >= 8 else -1
            names.setdefault(name, []).append((rseq, kind, parent, child_ino))
    return names

def heap_dentry_names(img, sb):
    """Scan every heap node extent's bset frames; return {name: [(rseq, node_addr, node_seq_at_write)]}."""
    h_s, h_l = sb["heap"]
    names = {}
    pos = h_s
    node_size = sb["node_size"]
    # node extents are node_size-aligned within the heap
    for base in range(h_s, h_s + h_l, node_size):
        hdr = img[base : base + 40]
        if len(hdr) < 40 or struct.unpack_from("<I", hdr, 0)[0] != 0x444E564B:  # "KVND"
            continue
        tree_id = hdr[6]
        level = hdr[7]
        node_addr, node_seq = struct.unpack_from("<QQ", hdr, 8)
        if node_addr != base:
            continue
        # walk bset frames from page 1
        off = base + 4096
        while off + 32 <= base + node_size:
            fh = img[off : off + 32]
            if struct.unpack_from("<I", fh, 0)[0] != 0x4653424B:  # "KBSF"
                break
            nsaw, padded, bset_len = struct.unpack_from("<QII", fh, 8)
            if padded == 0 or off + padded > base + node_size:
                break
            bset = img[off + 32 : off + 32 + bset_len]
            # bset image: parse records; bset has its own header — find records heuristically:
            # records: key_len u16 | kind u8 | seq u64 | val_len u32 | key | value
            # skip bset header: assume first record starts after a header; brute-scan for dentry values
            if tree_id == 2 and level == 0:
                i = 0
                while i + 15 <= len(bset):
                    klen, kind, rseq, vlen = struct.unpack_from("<HBQI", bset, i)
                    if klen == 16 and kind in (0, 1, 2) and vlen < 4096 and i + 15 + klen + vlen <= len(bset) and 0 < rseq < 1 << 40:
                        val = bset[i + 15 + klen : i + 15 + klen + vlen]
                        if vlen >= 10:
                            child_ino, ftype, nlen = struct.unpack_from("<QBB", val, 0)
                            if 10 + nlen <= vlen:
                                nm = val[10 : 10 + nlen]
                                if nm:
                                    names.setdefault(bytes(nm), []).append((rseq, node_addr, nsaw))
                        i += 15 + klen + vlen
                    else:
                        i += 1
            off += padded
    return names
