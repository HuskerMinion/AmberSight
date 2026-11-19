#!/usr/bin/env python3
"""Translate in parts (by people or agents) and merge.

  parts.py split <dir> <n>                 lang/en.json -> <dir>/en-part-1..n.json  ({key: english}),
                                           only messages not yet translated for any code are NOT filtered:
                                           pass --todo <code> to keep only missing/changed ones for that code
  parts.py check <code> <file>             <file> = {key: translation}; prints every problem, exit 1 if any
  parts.py merge <code> <file> [file...]   checked translations into lang/<code>.json (fingerprints added);
                                           failing ones are left out and listed
  parts.py rules <code>                    the translation rules and glossary for <code> (same as translate.py)
"""
import json, os, sys
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from icu import check_translation  # noqa: E402

ROOT = os.path.normpath(os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", ".."))
EN = os.path.join(ROOT, "lang", "en.json")


def en():
    return json.load(open(EN, encoding="utf-8"))["messages"]


def pack(code):
    p = os.path.join(ROOT, "lang", code + ".json")
    return json.load(open(p, encoding="utf-8"))["messages"] if os.path.exists(p) else {}


def main(a):
    if a[0] == "split":
        d, n = a[1], int(a[2])
        m = en()
        if "--todo" in a:
            have = pack(a[a.index("--todo") + 1])
            m = {k: v for k, v in m.items() if not (k in have and have[k][1] == v[1])}
        keys = sorted(m)
        os.makedirs(d, exist_ok=True)
        # contiguous parts of about equal word count (Help is long prose)
        words = [max(1, len(m[k][0].split())) for k in keys]
        target, chunks, cur, acc = sum(words) / n, [], [], 0
        for k, w in zip(keys, words):
            cur.append(k); acc += w
            if acc >= target * (len(chunks) + 1) and len(chunks) < n - 1:
                chunks.append(cur); cur = []
        chunks.append(cur)
        for i, ks in enumerate(chunks):
            part = {k: m[k][0] for k in ks}
            json.dump(part, open(os.path.join(d, "en-part-%d.json" % (i + 1)), "w", encoding="utf-8"), ensure_ascii=False, indent=1)
            print("part %d: %d messages, %d words" % (i + 1, len(part), sum(len(v.split()) for v in part.values())))
    elif a[0] == "check":
        code, f = a[1], a[2]
        m, got = en(), json.load(open(f, encoding="utf-8"))
        bad = 0
        for k, tr in got.items():
            why = "unknown key" if k not in m else check_translation(tr, m[k][0], code)
            if why:
                bad += 1
                print("%s: %s" % (k, why))
        print("%d checked, %d problems" % (len(got), bad))
        return 1 if bad else 0
    elif a[0] == "merge":
        code, files = a[1], a[2:]
        m, have = en(), pack(code)
        added = bad = 0
        for f in files:
            for k, tr in json.load(open(f, encoding="utf-8")).items():
                why = "unknown key" if k not in m else check_translation(tr, m[k][0], code)
                if why:
                    bad += 1
                    print("LEFT OUT %s: %s" % (k, why))
                    continue
                have[k] = [tr, m[k][1]]
                added += 1
        have = {k: v for k, v in have.items() if k in m}
        json.dump({"lang": code, "machine": True, "messages": have}, open(os.path.join(ROOT, "lang", code + ".json"), "w", encoding="utf-8"),
                  ensure_ascii=False, indent=0, sort_keys=True)
        missing = [k for k in m if k not in have or have[k][1] != m[k][1]]
        print("%s: %d merged, %d left out, %d of %d still missing" % (code, added, bad, len(missing), len(m)))
    elif a[0] == "rules":
        import translate  # noqa: E402
        print(translate.prompt_for(a[1]))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
