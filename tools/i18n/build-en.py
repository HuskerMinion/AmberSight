#!/usr/bin/env python3
"""Collect every t('key', 'English') in static/js into lang/en.json (the source for translation).
Fails if one key is used for two different English texts, or a message's ICU syntax is broken."""
import glob, json, os, sys
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from icu import T_CALL, js_unquote, args, fingerprint  # noqa: E402

ROOT = os.path.normpath(os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", ".."))
msgs, where, bad = {}, {}, []
for path in sorted(glob.glob(os.path.join(ROOT, "static", "js", "*.js"))):
    rel = os.path.relpath(path, ROOT)
    for n, line in enumerate(open(path, encoding="utf-8"), 1):
        for key, lit in T_CALL.findall(line):
            en = js_unquote(lit)
            if key in msgs and msgs[key] != en:
                bad.append("%s:%d key %s also used at %s for different English" % (rel, n, key, where[key]))
                continue
            try:
                args(en)
            except ValueError as e:
                bad.append("%s:%d %s: %s" % (rel, n, key, e))
            msgs[key] = en
            where.setdefault(key, "%s:%d" % (rel, n))
# Static page text: data-i18n="key" (element text) and data-i18n-attr="attr:key,..." (attribute values).
import html as _h  # noqa: E402
from html.parser import HTMLParser  # noqa: E402


class _Pages(HTMLParser):
    def __init__(self, rel):
        super().__init__(convert_charrefs=False)
        self.rel, self.stack = rel, []

    def handle_starttag(self, tag, attrs):
        a = dict(attrs)
        for pair in (a.get("data-i18n-attr") or "").split(","):
            if ":" in pair:
                attr, key = (x.strip() for x in pair.split(":", 1))
                if a.get(attr) is not None:
                    add(key, _h.unescape(a[attr]), "%s:%d" % (self.rel, self.getpos()[0]))
        self.stack.append([tag, a.get("data-i18n"), "", self.getpos()[0]])

    def handle_data(self, d):
        for s in self.stack:
            s[2] += d

    def handle_entityref(self, n):
        self.handle_data(_h.unescape("&%s;" % n))

    def handle_charref(self, n):
        self.handle_data(_h.unescape("&#%s;" % n))

    def handle_endtag(self, tag):
        while self.stack:
            t, key, text, line = self.stack.pop()
            if key:
                add(key, text, "%s:%d" % (self.rel, line))
            if t == tag:
                break


def add(key, en, at):
    if key in msgs and msgs[key] != en:
        bad.append("%s key %s also used at %s for different English" % (at, key, where[key]))
        return
    msgs[key] = en
    where.setdefault(key, at)


for page in ("index.html", "display.html"):
    pth = os.path.join(ROOT, "static", page)
    if os.path.exists(pth):
        _Pages(os.path.relpath(pth, ROOT)).feed(open(pth, encoding="utf-8").read())

out = {"lang": "en", "messages": {k: [v, fingerprint(v)] for k, v in sorted(msgs.items())}}
os.makedirs(os.path.join(ROOT, "lang"), exist_ok=True)
json.dump(out, open(os.path.join(ROOT, "lang", "en.json"), "w", encoding="utf-8"), ensure_ascii=False, indent=0, sort_keys=True)
print("%d messages, %d words" % (len(msgs), sum(len(v.split()) for v in msgs.values())))
for b in bad:
    print("PROBLEM", b)
sys.exit(1 if bad else 0)
