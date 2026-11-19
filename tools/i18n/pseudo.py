#!/usr/bin/env python3
"""Make lang/en-XA.json: every message about 40% longer with accented letters, markup and ICU kept,
so layout breaks and untranslated text (plain English) show up before a real language does."""
import json, os, re, sys
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from icu import MARKUP  # noqa: E402
ROOT = os.path.normpath(os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", ".."))
ACC = str.maketrans("aceinousyACEINOUSY", "àçéîñöûšýÀÇÉÎÑÖÛŠÝ")
from icu import args  # noqa: E402


def skip_for(s):
    """What stays as it is in message s: markup, entities, ICU syntax and the message's own argument names.
    A word followed by "{" is a select/plural selector unless what follows is one of the arguments
    ("active {time}" is text then an argument; "person {person}" is a selector and its branch)."""
    try:
        names = sorted({n for n, _, _ in args(s)}, key=len, reverse=True)
    except ValueError:
        names = []
    arg = "(?:" + "|".join(map(re.escape, names)) + ")" if names else "(?!)"
    return re.compile(r"(" + MARKUP.pattern + r"|&(?:[a-zA-Z]+|#\d+|#x[0-9a-fA-F]+);|\{\s*" + arg + r"\s*,\s*(?:plural|select)\s*,|\{\s*" + arg +
                      r"\s*\}|=?[A-Za-z0-9_]+\s*\{(?!\s*" + arg + r"\s*[,}])|[{}#']|\b(?:one|few|many|other)\b)")


def pseudo(s):
    out = []
    SKIP = skip_for(s)
    for i, part in enumerate(SKIP.split(s)):
        if part is None:
            continue
        if SKIP.fullmatch(part):
            out.append(part)
        else:
            out.append(part.translate(ACC))
    t = "".join(out)
    return "[" + t + " " + "·" * max(1, len(s) * 2 // 5 // 3) + "]"

en = json.load(open(os.path.join(ROOT, "lang", "en.json"), encoding="utf-8"))["messages"]
pk = {k: [pseudo(v), fp] for k, (v, fp) in en.items()}
json.dump({"lang": "en-XA", "machine": True, "messages": pk}, open(os.path.join(ROOT, "lang", "en-XA.json"), "w", encoding="utf-8"), ensure_ascii=False, indent=0, sort_keys=True)
print(len(pk), "pseudo messages")
