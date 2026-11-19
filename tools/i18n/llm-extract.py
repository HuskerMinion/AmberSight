#!/usr/bin/env python3
"""Turn a web app file's user-visible English into t('key', 'English', {vars}) calls, using a local LLM.

The model sees numbered lines and answers only with one-line edits; this script checks every edit
before applying it, then the whole file must still parse (node --check) or that chunk is undone.

  LLM_URL=http://host:port tools/i18n/llm-extract.py static/js/settings.js [more files]

Writes a report per file to tools/i18n/out/<file>.extract.json (rejected edits and why).
"""
import json, os, re, subprocess, sys, time, urllib.request

LLM = os.environ.get("LLM_URL", "").rstrip("/")
CHUNK = int(os.environ.get("CHUNK", "70"))
EFFORT = os.environ.get("EFFORT", "medium")
HERE = os.path.dirname(os.path.abspath(__file__))
OUT = os.path.join(HERE, "out")

RULES = r"""You prepare a JavaScript web app (a home security camera recorder, "AmberSight") for translation.
Find English text that a USER SEES (labels, buttons, headings, hints, help text, toasts, dialog text,
placeholders, title= and aria-label= text, error messages shown on screen) and wrap it in t().

The function:  t('KEY', 'English text', { vars })   returns the text in the user's language.
- KEY: '<PREFIX>.' + a short snake_case name for the meaning, e.g. '<PREFIX>.save_failed'. Unique per meaning.
- The English must be EXACTLY the original words (same spelling, punctuation, capitals, HTML tags, entities).
- Values that are inserted go in as {name} with a vars object: 'Saved ' + n + ' cameras'  becomes
  t('<PREFIX>.saved_n', 'Saved {n, plural, one {# camera} other {# cameras}}', { n })
  'Camera ' + esc(c.name) + ' is offline'  becomes  t('<PREFIX>.cam_offline', 'Camera {name} is offline', { name: esc(c.name) })
  Keep esc() and other calls exactly as they were, inside the vars object.
- Join a sentence that is split across + pieces ON ONE LINE into one message with {vars}. Do not join across lines.
- If English plural is made by hand (n === 1 ? 'camera' : 'cameras', or n + ' camera' + (n === 1 ? '' : 's')),
  turn it into ICU plural as above.
- HTML tags inside a sentence may stay inside the message: '<b>Note:</b> restart needed'.
  But do not wrap pure markup, and keep long HTML templates as they are, wrapping only the text pieces.
- A literal apostrophe inside the message: keep JavaScript quoting valid. Prefer the original quote style;
  if the original was "Don't", write t('<PREFIX>.dont', "Don't").  In the message an ICU literal { or } must be quoted '{'.

NEVER wrap or change:
- keys, ids, CSS classes, data-* values, element/attribute names, URLs, routes like '#settings/x', file names
- values compared in code or sent to the server ('on', 'off', 'person', mode names used in === or switch)
- console.log/warn/error, comments, regex, API field names, CSS, keyboard keys, units already in a fmt helper
- camera brand or product names on their own (Reolink, ONVIF, RTSP, MQTT, HomeKit), single symbols
- text that is already inside t(...)
If a line has nothing to translate, give no edit for it. Do not touch import lines.

Answer JSON only: {"edits":[{"line":N,"old":"exact text copied from that line","new":"replacement"}]}
"old" must be an exact substring of line N (copy it character for character, as short as possible but unique on the line).
"new" replaces exactly that substring. One edit per line at most; if a line has several pieces, make "old" span them.
"""

SCHEMA = {"type": "object", "properties": {"edits": {"type": "array", "items": {"type": "object",
          "properties": {"line": {"type": "integer"}, "old": {"type": "string"}, "new": {"type": "string"}},
          "required": ["line", "old", "new"]}}}, "required": ["edits"]}


def ask(prompt):
    body = {"model": "m", "temperature": 0.1, "max_tokens": 12000,
            "messages": [{"role": "system", "content": RULES.replace("<PREFIX>", PREFIX)},
                         {"role": "user", "content": prompt}],
            "chat_template_kwargs": {"reasoning_effort": EFFORT},
            "response_format": {"type": "json_schema", "json_schema": {"name": "edits", "schema": SCHEMA}}}
    req = urllib.request.Request(LLM + "/v1/chat/completions", json.dumps(body).encode(), {"Content-Type": "application/json"})
    for attempt in range(3):
        try:
            with urllib.request.urlopen(req, timeout=1800) as r:
                j = json.load(r)
            txt = j["choices"][0]["message"]["content"]
            return json.loads(txt).get("edits", []), j.get("usage", {})
        except Exception as e:  # noqa: BLE001 - retried, then reported
            err = e
            time.sleep(5)
    raise RuntimeError("LLM failed: %s" % err)


# ---------------------------------------------------------------- checking one edit

T_CALL = re.compile(r"""\bt\(\s*'([a-z][a-z0-9_]*(?:\.[a-z0-9_]+)+)'\s*,\s*('(?:[^'\\]|\\.)*'|"(?:[^"\\]|\\.)*"|`(?:[^`\\$]|\\.)*`)""")
WORD = re.compile(r"[A-Za-z]+(?:'[A-Za-z]+)*")


def js_unquote(lit):
    q, body = lit[0], lit[1:-1]
    return re.sub(r"\\(.)", lambda m: {"n": "\n", "t": "\t"}.get(m.group(1), m.group(1)), body)


def icu_vars(msg):
    # names of {name} / {name, plural|select, ...}, skipping quoted '{' literals
    m2 = re.sub(r"'[{}#][^']*'", "", msg)
    return set(re.findall(r"\{\s*([A-Za-z_][A-Za-z0-9_]*)\s*[,}]", m2))


def check(old_line, old, new, keys):
    if old not in old_line:
        return "old text not on the line"
    if old_line.count(old) != 1:
        return "old text not unique on the line"
    calls = T_CALL.findall(new)
    if not calls:
        return "no t('key', 'English') call in new"
    if new.count("t(") > len(calls) + new.count("t(t") + 3:
        pass
    old_words = set(WORD.findall(re.sub(r"\\(.)", r"\1", old)))
    for key, lit in calls:
        if not key.startswith(PREFIX + "."):
            return "key %s lacks prefix %s." % (key, PREFIX)
        en = js_unquote(lit)
        if lit[0] == "`":
            return "template literal as message"
        # every English word must come from the original (ICU plural/select words may be new forms)
        plain = re.sub(r"\{[^{}]*,\s*(plural|select)\s*,", " ", en)
        plain = re.sub(r"<[^>]+>|&[a-z#0-9]+;|\{[A-Za-z_][A-Za-z0-9_]*\}", " ", plain)
        extra = [w for w in WORD.findall(plain) if w not in old_words and w.rstrip("s") not in old_words
                 and w + "s" not in old_words and w.lower() not in ("one", "other", "zero", "few", "many")]
        if extra:
            return "English has words not in the original: %s" % ", ".join(extra[:5])
        for v in icu_vars(en):
            after = new[new.index(lit) + len(lit):]
            if not re.search(r"\{[^}]*\b%s\b" % re.escape(v), after.split("t(")[0] if "t(" in after else after):
                return "value {%s} not passed in vars" % v
        if key in keys and keys[key] != en:
            return "key %s already used for different English" % key
    return None


def literal_at(line, i, n):
    """If line[i:i+n] lies inside one string literal, return (quote, start, end) of that literal
    (start = index of the opening quote, end = index of the closing one), else None."""
    q, st, j = None, -1, 0
    while j < len(line):
        c = line[j]
        if q:
            if c == "\\":
                j += 2
                continue
            if c == q:
                if st < i and i + n <= j:
                    return q, st, j
                q = None
        elif c in "'\"`":
            q, st = c, j
        elif c == "/" and line[j + 1:j + 2] == "/":
            break
        j += 1
    return None


def fit_edit(line, old, new):
    """Make the model's edit valid JS when its old text sits inside a string literal."""
    i = line.find(old)
    lit = literal_at(line, i, len(old))
    if not lit:
        return old, new
    q, st, en = lit
    if q == "`":
        return old, new
    if st + 1 == i and i + len(old) == en:      # the whole string: replace it with its quotes
        return line[st:en + 1], new
    pre, post = line[st + 1:i], line[i + len(old):en]
    rep = (q + pre + q + " + " if pre else "") + new + (" + " + q + post + q if post else "")
    return line[st:en + 1], rep


def node_ok_lines(path, lines):
    tmp = path + ".i18n-tmp.mjs"
    open(tmp, "w", encoding="utf-8").writelines(add_import(lines))
    r = subprocess.run(["node", "--check", tmp], capture_output=True, text=True)
    os.unlink(tmp)
    err = [l for l in r.stderr.splitlines() if "Error" in l][:1]
    return r.returncode == 0, (err[0][:160] if err else "")


def add_import(lines):
    src = "".join(lines)
    if re.search(r"^import\s*\{[^}]*\bt\b[^}]*\}\s*from\s*'\./i18n\.js'", src, re.M):
        return lines
    for i, l in enumerate(lines):
        if l.startswith("import "):
            return lines[:i] + ["import { t } from './i18n.js';\n"] + lines[i:]
    # no imports: after the header comment block
    i = 0
    while i < len(lines) and (lines[i].startswith("//") or not lines[i].strip()):
        i += 1
    return lines[:i] + ["import { t } from './i18n.js';\n"] + lines[i:]


def run(path):
    global PREFIX
    base = os.path.basename(path)[:-3]
    PREFIX = re.sub(r"[^a-z0-9]", "_", base.lower())
    lines = open(path, encoding="utf-8").readlines()
    keys = {}
    for k, lit in T_CALL.findall("".join(lines)):
        keys[k] = js_unquote(lit)
    report = {"file": path, "applied": 0, "rejected": [], "chunks_undone": [], "tokens": 0}
    head = "".join(lines[:12])
    t0 = time.time()
    for start in range(0, len(lines), CHUNK):
        end = min(len(lines), start + CHUNK)
        body = "".join("%d| %s" % (i + 1, lines[i]) for i in range(start, end))
        prompt = ("File %s (top of file for context):\n%s\n----\nLines %d-%d to process:\n%s"
                  % (base + ".js", head, start + 1, end, body))
        edits, usage = ask(prompt)
        report["tokens"] += usage.get("completion_tokens", 0)
        for e in edits:
            n = e.get("line", 0) - 1
            if not (start <= n < end):
                report["rejected"].append({**e, "why": "line outside chunk"})
                continue
            old = e["old"]
            if old not in lines[n]:
                # the model often drops the code's escaping: account\'s -> account's
                for alt in (old.replace("'", "\\'"), old.replace('"', '\\"')):
                    if alt in lines[n]:
                        old = alt
                        break
            why = check(lines[n], old, e["new"], keys)
            if not why:
                before = lines[n]
                o2, n2 = fit_edit(lines[n], old, e["new"])
                lines[n] = lines[n].replace(o2, n2, 1)
                ok, err = node_ok_lines(path, lines)
                if not ok:
                    lines[n] = before
                    why = "file no longer parses: %s" % err
            if why:
                report["rejected"].append({**e, "why": why})
                continue
            for k, lit in T_CALL.findall(e["new"]):
                keys[k] = js_unquote(lit)
            report["applied"] += 1
        print("  %s %d-%d: %d edits, %d applied so far, %.0fs" % (base, start + 1, end, len(edits), report["applied"], time.time() - t0), flush=True)
    if report["applied"]:
        lines = add_import(lines)
    open(path, "w", encoding="utf-8").writelines(lines)
    os.makedirs(OUT, exist_ok=True)
    json.dump(report, open(os.path.join(OUT, base + ".extract.json"), "w"), indent=1)
    print("%s: %d applied, %d rejected, %d chunks undone, %ds" % (path, report["applied"], len(report["rejected"]), len(report["chunks_undone"]), time.time() - t0), flush=True)


if __name__ == "__main__":
    if not LLM:
        sys.exit("set LLM_URL")
    for p in sys.argv[1:]:
        run(p)
