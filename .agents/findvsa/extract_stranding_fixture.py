#!/usr/bin/env python3
"""Derive kvparse.py-verified expectations + the stranding fixture from a
re-captured FIND-VS-A loss round (docs/design-smo-replay-currency.md §6 PR 1).

Input: a capture dir produced by recapture.sh (meta{1..4}.img page-cache-
coherent post-kill copies, tree.acked, missing.txt, verdict=LOSS).

For every image: superblock extents, both ledger slots (newest-valid =
the record a remount mounts), ring census, and the replay chain walk from
the mounted tail — all through kvparse.py's checksummed parsers. Then, for
each acked-lost name: locate its dentry Put (by the name embedded in the
dentry value) and its inode Put (by the child ino key) among the WINDOW
records, and find the covering higher-seq interior flip in the same tree —
the sub-mechanism (i) stranding signature (design §1: `C (T <= c < p_flip)`
replays via the pre-flip route; the flip then abandons that lineage).

Output:
  stdout                      expectations (kvparse output + per-name adjudication)
  <capture>/findvsa2_stranding_window.jsonl   the in-repo fixture subset:
      line 1: header {captured, image, mounted_seq, tail, loss_total, ...}
      rows:   {"row":"lost_put",...} / {"row":"flip",...}
"""
import json
import os
import struct
import sys
import importlib.util

HERE = os.path.dirname(os.path.abspath(__file__))
spec = importlib.util.spec_from_file_location("kvparse", os.path.join(HERE, "kvparse.py"))
kvparse = importlib.util.module_from_spec(spec)
# kvparse guards its CLI behind __name__ == "__main__"; import is side-effect free.
spec.loader.exec_module(kvparse)

PAGE, PHDR, PDATA, EHDR, MAGIC = kvparse.PAGE, kvparse.PHDR, kvparse.PDATA, kvparse.EHDR, kvparse.MAGIC
TREE_INODES, TREE_DENTRIES = 1, 2
KIND = {1: "Put", 2: "Delta", 3: "Delete"}


def untag(tag):
    return tag & 0x0F, tag >> 4


def parse_records(payload):
    """Entry payload -> [(tree, level, kind, seq, key, val)] (journal.rs wire)."""
    out, i = [], 0
    while i < len(payload):
        tag = payload[i]
        i += 1
        if i + 15 > len(payload):
            break
        klen, kind, seq, vlen = struct.unpack_from("<HBQI", payload, i)
        i += 15
        if i + klen + vlen > len(payload):
            break
        key = payload[i : i + klen]
        i += klen
        val = payload[i : i + vlen]
        i += vlen
        tree, level = untag(tag)
        out.append((tree, level, kind, seq, key, val))
    return out


def walk_window(img, sb, tail):
    """The kvparse.chain_walk logic, returning [(entry_pos, payload)] instead
    of printing — the replay window a remount of this image actually sees."""
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

    chain_end = tail + L
    window_base = tail - (tail % PDATA)
    slots = pages + 1 if window_base < tail else pages
    entries = []
    pos, next_slot = tail, 0
    while True:
        if pos is not None and pos < chain_end:
            hdr = rd(pos, EHDR)
            seq, elen, csum = struct.unpack("<QIQ", hdr)
            ok = seq == pos and 0 < elen and EHDR + elen <= 128 * 1024
            if ok:
                payload = rd(pos + EHDR, elen)
                if kvparse.x3 is not None:
                    ok = kvparse.x3(hdr[0:12] + payload) == csum
            if ok:
                entries.append((pos, payload))
                pos = pos + EHDR + elen
            else:
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
                ok = magic == MAGIC and (kvparse.x3 is None or kvparse.x3(hdr[0:16]) == csum)
                if ok and lap == pos_i // L:
                    if feo == 0xFFFF:
                        continue
                    if PHDR <= feo < PAGE:
                        found = pos_i + feo - PHDR
                        break
            if found is None:
                break
            pos = found
    return entries


def main():
    cap = sys.argv[1]
    missing = [
        os.path.basename(l.strip())
        for l in open(os.path.join(cap, "missing.txt"))
        if l.strip()
    ]
    print(f"# capture: {cap}")
    print(f"# acked-lost names: {len(missing)}")

    per_image = []
    for i in (1, 2, 3, 4):
        p = os.path.join(cap, f"meta{i}.img")
        img = open(p, "rb").read()
        sb = kvparse.read_sb(img)
        print(f"--- meta{i}.img ver={sb['ver']} node={sb['node_size']} journal={sb['journal']}")
        kvparse.ring_census(img, sb)
        print("  ledger slots (newest valid = the mounted record):")
        best = kvparse.parse_ledger_slots(img, sb)
        if best is None:
            print("  !! no valid ledger slot")
            continue
        mounted_seq, tail = best
        n_entries, seqs = kvparse.chain_walk(img, sb, tail)
        window = walk_window(img, sb, tail)
        assert len(window) == n_entries, "walk_window must agree with kvparse.chain_walk"
        recs = []
        for pos, payload in window:
            recs.extend(parse_records(payload))
        flips = [r for r in recs if r[1] > 0]
        print(
            f"  window: entries={len(window)} records={len(recs)} interior_flips={len(flips)} "
            f"mounted_seq={mounted_seq} tail={tail}"
        )
        per_image.append((i, p, img, sb, mounted_seq, tail, recs, flips))

    # Adjudicate each lost name against every volume's window. The dentry
    # value's child_ino is the GLOBAL ino (RoutedMetaBackend); the inode
    # record lives on volume (g-2) % nvols under the LOCAL ino key
    # (g-2)/nvols + 2 (meta_backend/mod.rs route_ino/make_global_ino).
    nvols = len(per_image)
    by_idx = {e[0]: e for e in per_image}
    fixture_rows = []
    flip_keys_used = {}
    found_names = 0
    strand_sig = 0
    for name in missing:
        nb = name.encode()
        hit = None
        for (idx, p, img, sb, mseq, tail, recs, flips) in per_image:
            for (tree, level, kind, seq, key, val) in recs:
                if tree != TREE_DENTRIES or level != 0 or kind != 1 or len(val) < 10:
                    continue
                child_ino, ftype, nlen = struct.unpack_from("<QBB", val, 0)
                if val[10 : 10 + nlen] == nb:
                    hit = (idx, seq, key, child_ino, flips, tail)
                    break
            if hit:
                break
        if not hit:
            print(f"  name={name}: dentry Put NOT in any window (below-tail class)")
            continue
        found_names += 1
        idx, dseq, dkey, g_ino, dflips, dtail = hit
        # covering flip: same tree, higher seq, smallest flip key >= put key
        dcover = [
            (fs, fk)
            for (ft, fl, fkind, fs, fk, fv) in dflips
            if ft == TREE_DENTRIES and fs > dseq and fk >= dkey and fkind == 1
        ]
        # the inode-record side (design §1: either tree's stranding loses
        # the name — the forensics' "dentry resolved, inode unreachable").
        if g_ino == 1 or nvols <= 1:
            ivol, local_ino = 1 if nvols <= 1 else 1, g_ino
        else:
            ivol = int((g_ino - 2) % nvols) + 1
            local_ino = (g_ino - 2) // nvols + 2
        ikey = struct.pack(">Q", local_ino)
        _, _, _, _, imseq, itail, irecs, iflips = by_idx[ivol]
        iput = [
            s
            for (t, l, kd, s, k, v) in irecs
            if t == TREE_INODES and l == 0 and kd == 1 and k == ikey
        ]
        icover = []
        if iput:
            iseq = max(iput)
            icover = [
                (fs, fk)
                for (ft, fl, fkind, fs, fk, fv) in iflips
                if ft == TREE_INODES and fs > iseq and fk >= ikey and fkind == 1
            ]
        stranded = bool(dcover) or bool(icover)
        if stranded:
            strand_sig += 1
        print(
            f"  name={name}: dvol=meta{idx} dentry_put_seq={dseq} "
            f"dentry_cover_flips={len(dcover)} ivol=meta{ivol} local_ino={local_ino} "
            f"inode_put={'seq=' + str(max(iput)) if iput else 'ABSENT'} "
            f"inode_cover_flips={len(icover)} STRANDING_SIG={'YES' if stranded else 'no'}"
        )
        if len(fixture_rows) < 512:
            fixture_rows.append(
                {
                    "row": "lost_put",
                    "name": name,
                    "dentry": {"img": idx, "seq": dseq, "key": dkey.hex(), "tail": dtail},
                    "inode": {
                        "img": ivol,
                        "local_ino": local_ino,
                        "seq": max(iput) if iput else None,
                        "key": ikey.hex(),
                        "tail": itail,
                    },
                    "global_ino": g_ino,
                }
            )
            for tree, cov, img_i in ((TREE_DENTRIES, dcover, idx), (TREE_INODES, icover, ivol)):
                for fs, fk in cov[:4]:
                    fid = (img_i, tree, fs, fk.hex())
                    if fid in flip_keys_used:
                        continue
                    flip_keys_used[fid] = True
                    fixture_rows.append(
                        {"row": "flip", "img": img_i, "tree": tree, "seq": fs, "key": fk.hex()}
                    )

    print(
        f"# adjudication: {found_names}/{len(missing)} lost names have their acked Put "
        f"IN the replay window; {strand_sig}/{found_names} carry the stranding "
        f"signature (a higher-seq covering flip in the same window)"
    )
    hdr = {
        "row": "header",
        "capture": os.path.basename(os.path.abspath(cap)),
        "loss_total": len(missing),
        "in_window": found_names,
        "stranding_sig": strand_sig,
        "images": [
            {"img": idx, "mounted_seq": mseq, "tail": tail, "window_records": len(recs)}
            for (idx, p, img, sb, mseq, tail, recs, flips) in per_image
        ],
    }
    out = os.path.join(cap, "findvsa2_stranding_window.jsonl")
    with open(out, "w") as f:
        f.write(json.dumps(hdr) + "\n")
        for r in fixture_rows:
            f.write(json.dumps(r) + "\n")
    print(f"# fixture written: {out} ({len(fixture_rows)} rows)")


if __name__ == "__main__":
    main()
