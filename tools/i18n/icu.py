"""Shared helpers for the i18n tools: find t() calls, fingerprint and check ICU messages.
Mirrors static/js/i18n.js (fingerprint, markup check) so a pack that passes here passes in the browser."""
import re

T_CALL = re.compile(r"""\bt\(\s*'([a-z][a-z0-9_]*(?:\.[a-z0-9_]+)+)'\s*,\s*('(?:[^'\\]|\\.)*'|"(?:[^"\\]|\\.)*")""")
MARKUP = re.compile(r"</?[a-zA-Z][^<>]*>")  # tags only; entities are harmless text

# Plural categories each language needs (CLDR, as Intl.PluralRules reports them for integers).
PLURALS = {"en": {"one", "other"}, "de": {"one", "other"}, "es": {"one", "many", "other"},
           "fr": {"one", "many", "other"}, "nl": {"one", "other"}, "it": {"one", "many", "other"},
           "zh-CN": {"other"}, "pl": {"one", "few", "many", "other"}, "pt-BR": {"one", "many", "other"},
           "sv": {"one", "other"}, "ru": {"one", "few", "many", "other"}}
# "many" in es/fr/it/pt is only for 1,000,000-style numbers; accept it missing (falls to other).
OPTIONAL = {"many"}


_ESC = {"n": "\n", "t": "\t", "r": "\r", "b": "\b", "f": "\f", "v": "\v", "0": "\0"}


def js_unquote(lit):
    """The string a JavaScript '...' or "..." literal stands for (escapes as JS reads them)."""
    body, out, i = lit[1:-1], [], 0
    while i < len(body):
        c = body[i]
        if c != "\\" or i + 1 == len(body):
            out.append(c); i += 1; continue
        n = body[i + 1]
        if n == "u" and body[i + 2:i + 3] == "{":
            j = body.index("}", i)
            out.append(chr(int(body[i + 3:j], 16))); i = j + 1
        elif n == "u":
            out.append(chr(int(body[i + 2:i + 6], 16))); i += 6
        elif n == "x":
            out.append(chr(int(body[i + 2:i + 4], 16))); i += 4
        elif n == "\n":
            i += 2  # line continuation
        else:
            out.append(_ESC.get(n, n)); i += 2
    s = "".join(out)
    # surrogate pairs written as two \u escapes
    return s.encode("utf-16", "surrogatepass").decode("utf-16")


def fingerprint(s):
    h = 0x811c9dc5
    for ch in s.encode("utf-16-le").decode("utf-16-le"):
        for unit in _units(ch):
            h ^= unit
            h = (h * 0x01000193) & 0xffffffff
    return "%08x" % h


def _units(ch):
    o = ord(ch)
    if o < 0x10000:
        return [o]
    o -= 0x10000
    return [0xD800 + (o >> 10), 0xDC00 + (o & 0x3FF)]


def markup(s):
    return MARKUP.findall(s)


def safe_against(tr, en):
    """Same tags as the English, no other '<', no more '"' than the English (see static/js/i18n.js)."""
    rest = MARKUP.sub("", tr)
    return markup(tr) == markup(en) and "<" not in rest and rest.count('"') <= MARKUP.sub("", en).count('"')


def _close(s, j):
    d = 0
    while j < len(s):
        if s[j] == "{":
            d += 1
        elif s[j] == "}":
            if not d:
                return j
            d -= 1
        j += 1
    return -1


def args(s):
    """Top-level and nested arguments: list of (name, kind, {selector: body})."""
    out, i = [], 0
    s2 = re.sub(r"'[{}#][^']*'", lambda m: "_" * len(m.group(0)), s)  # quoted literals out of the way
    while i < len(s2):
        if s2[i] != "{":
            i += 1
            continue
        end = _close(s2, i + 1)
        if end < 0:
            raise ValueError("unbalanced braces")
        inner = s2[i + 1:end]
        parts = inner.split(",", 2)
        name = parts[0].strip()
        if not re.fullmatch(r"[A-Za-z_][A-Za-z0-9_]*", name):
            raise ValueError("bad argument {%s}" % inner[:20])
        if len(parts) < 3:
            out.append((name, None, {}))
        else:
            kind, rest = parts[1].strip(), parts[2]
            if kind not in ("plural", "select"):
                raise ValueError("unknown kind %s" % kind)
            opts, j = {}, 0
            while j < len(rest):
                m = re.match(r"\s*(=?[A-Za-z0-9_]+)\s*\{", rest[j:])
                if not m:
                    if rest[j:].strip():
                        raise ValueError("bad options in {%s}" % name)
                    break
                b = j + m.end()
                e = _close(rest, b)
                if e < 0:
                    raise ValueError("unbalanced option")
                opts[m.group(1)] = rest[b:e]
                j = e + 1
            if "other" not in opts:
                raise ValueError("{%s, %s} lacks other" % (name, kind))
            out.append((name, kind, opts))
            for body in opts.values():
                out.extend(args(body))
        i = end + 1
    return out


def check_translation(tr, en, lang):
    """None if tr can stand in for en, else the reason."""
    if not tr.strip():
        return "empty"
    if not safe_against(tr, en):
        return "HTML tags differ from the English (must be exactly: %s), or a stray < or extra \" (use typographic quotes)" % (" ".join(markup(en)) or "none")
    try:
        ea, ta = args(en), args(tr)
    except ValueError as e:
        return "English: %s" % e if "English" in str(e) else "broken ICU syntax: %s" % e
    en_names = {(n, k) for n, k, _ in ea}
    tr_names = {(n, k) for n, k, _ in ta}
    if {n for n, _ in en_names} != {n for n, _ in tr_names}:
        return "placeholders must be exactly %s" % sorted("{%s}" % n for n, _ in en_names)
    # A value the English uses only to pick a branch (plural count, select choice) is not shown raw.
    shown_en = {n for n, k, _ in ea if k is None}
    for n, k, _ in ta:
        if k is None and n not in shown_en:
            return "{%s} is shown as it is, but the English only uses it as a plural or select" % n
    need = PLURALS.get(lang, {"other"})
    for n, k, opts in ta:
        if k == "plural":
            missing = need - OPTIONAL - set(opts)
            if missing:
                return "{%s, plural} needs forms %s for this language" % (n, sorted(need))
        if k == "select":
            ek = next((o for nn, kk, o in ea if nn == n and kk == "select"), {})
            if set(ek) - set(opts):
                return "{%s, select} must keep the selectors %s" % (n, sorted(ek))
    return None
