#!/usr/bin/env python3
"""Draft translations with a translation model (Tencent Hy-MT2) behind llama-server, one message per call.

  LLM_URL=http://127.0.0.1:18501 tools/i18n/hymt.py <code> <out.json> [--keys keys.txt] [--workers 8]

Writes {key: translation} for every message that passes the checker (as parts.py check), and
<out>.failed.json {key: reason} for the rest (those go to a person or a bigger model). Resumable:
keys already in <out.json> are skipped. Same rules as translate.py (address form, style, glossary)."""
import json, os, re, sys, time, threading, urllib.request
from concurrent.futures import ThreadPoolExecutor
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from icu import check_translation, PLURALS, OPTIONAL  # noqa: E402
import translate as T  # noqa: E402

LLMS = [u.strip().rstrip("/") for u in os.environ.get("LLM_URL", "http://127.0.0.1:18501").split(",")]
_rr = __import__("itertools").count()
TEMP = float(os.environ.get("TEMP", "0.1"))
BUTTONS = {"fr": "infinitive (Enregistrer, Annuler)", "it": "the form Italian software uses (Salva, Annulla)",
           "pt-BR": "infinitive (Salvar, Cancelar)", "nl": "infinitive (Opslaan, Annuleren)",
           "ru": "infinitive (Сохранить, Отменить)", "pl": "imperative as Polish software does (Zapisz, Anuluj)",
           "zh-CN": "short verb (保存, 取消)"}
NAMES = {"pt-BR": "Brazilian Portuguese", "zh-CN": "Simplified Chinese"}
WHERE = [("help.", "a paragraph of the in-app Help"), ("settings.", "the Settings pages"),
         ("events.", "the Events page"), ("index.", "the page header and menus")]


def glossary(code):
    gl = json.load(open(T.GLOSSARY, encoding="utf-8")) if os.path.exists(T.GLOSSARY) else {}
    extra = os.path.join(T.HERE, "glossary-extra.json")
    if os.path.exists(extra):
        for en, tr in json.load(open(extra, encoding="utf-8")).items():
            if not en.startswith("_"):
                gl.setdefault(en, {}).update({k: v for k, v in tr.items() if k not in gl.get(en, {})})
    return {en: tr[code] for en, tr in gl.items() if code in tr}


def terms_for(text, gl):
    """Glossary entries whose English appears in the message. "arm / disarm" -> two terms when the translation
    has the same number of parts; "(tab)" entries only for short labels; other notes are hints, not words."""
    short = len(text.split()) <= 4
    out = []
    for en, tr in gl.items():
        base, note = re.match(r"(.*?)\s*(\(.*\))?$", en).groups()
        if note == "(tab)" and not short:
            continue
        if note in ("(tab)",) or not note:
            pass
        ens = [x.strip() for x in re.split(r"\s*/\s*|,\s*(?:and\s+)?|\s+and\s+", base) if x.strip()]
        trs = [x.strip() for x in re.split(r"\s*/\s*|,\s*|\s+(?:und|y|et|e|en|i|и|和|och|oraz)\s+", tr) if x.strip()]
        if len(ens) != len(trs) and "/" in tr:
            continue  # "stream (main / sub)": the pair would leak into the text
        pairs = list(zip(ens, trs)) if len(ens) == len(trs) else [(base, tr)]
        for e, t in pairs:
            if re.search(r"(?<![A-Za-z])%s(?:s|ed|ing)?(?![A-Za-z])" % re.escape(e), text, re.I):
                out.append((e, t))
    # "armed" already covers "arm"
    return [(e, t) for e, t in out if not any(o != e and o.startswith(e) and re.search(r"(?<![A-Za-z])%s" % re.escape(o), text, re.I) for o, _ in out)]


NEAR = {}


def neighbours(key, en):
    """English of up to 6 messages next to this one in the same file (gives short labels their context)."""
    if not NEAR:
        ks = sorted(en)
        for i, k in enumerate(ks):
            NEAR[k] = [ks[j] for j in range(max(0, i - 3), min(len(ks), i + 4)) if j != i and ks[j].split(".")[0] == k.split(".")[0]]
    return [en[k][0][:80] for k in NEAR.get(key, [])]


def prompt(code, key, text, gl, problem=None, en=None):
    name = NAMES.get(code) or T.LANGS[code][0].split(" (")[0]
    where = next((w for p, w in WHERE if key.startswith(p)), "the user interface")
    tasks = ["This text is from %s of AmberSight, a security camera recorder (NVR) web app." % where,
             T.LANGS[code][1]]
    quoted = bool(re.search(r"[\"“”‘’«»]", text))
    style = T.STYLE.get(code, "")
    if not quoted:  # a hint about quote marks makes the model add them
        style = " ".join(x for x in re.split(r"(?<=[.;])\s+", style) if not re.search(r"quote|«|„", x))
    if style:
        tasks.append(style)
    words = len(text.split())
    if words <= 4:
        tasks.append("It is a short UI label (button, tab, column or menu item): keep it short."
                     + (" Buttons and actions use the %s." % BUTTONS[code] if code in BUTTONS else ""))
    if text[:1].islower():
        tasks.append("It starts in lower case because it continues a sentence; keep the first letter lower case.")
    if text[:1].islower() or (words <= 8 and not re.search(r"[.!?:)]$", text.strip())):
        tasks.append("It may be only part of a sentence; translate exactly this part, do not complete or explain it.")
    tasks.append("Be concise: about as long as the source; do not add words, quotes or politeness formulas.")
    if "{" in text:
        tasks.append("Keep every {...} placeholder exactly as written (same name, untranslated).")
    if "<" in text or "&" in text:
        tasks.append("Keep the HTML tags and entities exactly as in the source.")
    if ", plural" in text:
        tasks.append("The text uses ICU plural syntax: keep {n, plural, ...} and '#', translate inside each "
                     "branch, and give exactly these branches: %s." % ", ".join(sorted(PLURALS.get(code, {"other"}))))
    if ", select" in text:
        tasks.append("The text uses ICU select syntax: keep the selector words in English, translate the branches.")
    if re.search(r"[\"“”‘’]", text):
        tasks.append("For quotation marks use the language's typographic quotes, never a straight double quote.")
    if re.search(r"AmberSight|Reolink|ONVIF|RTSP|MQTT|HomeKit|Home Assistant|\.\w{2,4}\b|\b(ms|MB|GB|fps)\b", text):
        tasks.append("Do not translate product names, file names or units.")
    ts = terms_for(text, gl)
    if ts:
        tasks.append("Use these translations: " + "; ".join("“%s” translates to “%s”" % t for t in ts) + ".")
    if problem:
        tasks.append("A previous translation was rejected: %s. Fix that." % problem)
    tasks.append("Translate the [Source Text] into %s. Output ONLY the translation." % name)
    if os.environ.get("V2") == "1" and en is not None:
        near = neighbours(key, en)
        bg = "Text id in the app: %s. Nearby texts on the same screen: %s" % (key, " | ".join(near) if near else "none")
        return ("[Background Information]\n%s\n\nRules:\n%s\n\nPlease translate the following text into %s, taking the "
                "provided background information into consideration. Output ONLY the translation.\n\n[Source Text]\n%s"
                % (bg, "\n".join("- " + t for t in tasks[:-1]), NAMES.get(code) or T.LANGS[code][0].split(" (")[0], text))
    return "[Source Text]\n%s\n\n[Translation Tasks]\n%s" % (
        text, "\n".join("%d. %s" % (i + 1, t) for i, t in enumerate(tasks)))


def tidy(tr, src):
    """Quote marks the model wrapped around a message whose source has none."""
    if not re.search(r"[\"“”‘’«»„]", src):
        tr = re.sub(r"\s*«\s*»\s*", "", tr).strip()
        tr = re.sub(r"^[«“„]\s*(.*?)\s*[»”“]$", r"\1", tr)
    return tr


def untranslated(tr, src):
    """The model sometimes hands a short fragment back in English."""
    w = re.findall(r"[A-Za-z]{4,}", src)
    if not w or re.search(r"AmberSight|Reolink|ONVIF|RTSP|MQTT|HomeKit|Home Assistant", src):
        return False
    if tr.strip().lower() == src.strip().lower():
        return True
    first = re.match(r"\W*([A-Za-z]+)", tr)
    return bool(src[:1].islower() and first and first.group(1).lower() == w[0].lower() and len(w[0]) > 3)


def ask(p):
    body = {"model": "m", "temperature": TEMP, "top_p": 1.0, "max_tokens": 4096,
            "messages": [{"role": "user", "content": p}]}
    url = LLMS[next(_rr) % len(LLMS)]  # several servers (one per GPU): round robin
    req = urllib.request.Request(url + "/v1/chat/completions", json.dumps(body).encode(),
                                 {"Content-Type": "application/json"})
    for i in range(3):
        try:
            with urllib.request.urlopen(req, timeout=600) as r:
                j = json.load(r)
            return j["choices"][0]["message"]["content"].strip(), j.get("usage", {}).get("completion_tokens", 0)
        except Exception:  # noqa: BLE001
            time.sleep(5 * (i + 1))
    return "", 0


def main(a):
    code, out = a[0], a[1]
    workers = int(a[a.index("--workers") + 1]) if "--workers" in a else 8
    en = json.load(open(os.path.join(T.ROOT, "lang", "en.json"), encoding="utf-8"))["messages"]
    keys = sorted(en)
    if "--keys" in a:
        keys = [k.strip() for k in open(a[a.index("--keys") + 1]) if k.strip() in en]
    done = json.load(open(out, encoding="utf-8")) if os.path.exists(out) else {}
    failp = out[:-5] + ".failed.json"
    failed = {}
    todo = [k for k in keys if k not in done]
    gl, lock, t0 = glossary(code), threading.Lock(), time.time()
    stats = {"n": 0, "tok": 0}
    print("%s: %d to do (%d already done)" % (code, len(todo), len(done)), flush=True)

    def one(k):
        src = en[k][0]
        tr, n = ask(prompt(code, k, src, gl, en=en))
        tr = tidy(tr, src)
        why = check_translation(tr, src, code) if tr else "no answer"
        if not why and re.search(r"\[(Source|Translation|Background|Texte|Text)", tr):
            why = "it contains template text in brackets; output only the translation"
        if not why and untranslated(tr, src):
            why = "it was left in English; translate every word"
            tr2, n2 = ask(prompt(code, k, src, gl, why, en=en))
            tr2 = tidy(tr2, src)
            n += n2
            why2 = check_translation(tr2, src, code) if tr2 else "no answer"
            tr, why = (tr2, None) if not why2 else (tr, why2)
        with lock:
            stats["n"] += 1; stats["tok"] += n
            if why:
                failed[k] = why
            else:
                done[k] = tr
            if stats["n"] % 100 == 0 or stats["n"] == len(todo):
                json.dump(done, open(out, "w", encoding="utf-8"), ensure_ascii=False, indent=1, sort_keys=True)
                json.dump(failed, open(failp, "w", encoding="utf-8"), ensure_ascii=False, indent=1)
                dt = time.time() - t0
                print("  %s %d/%d, %d failed, %.0fs, %.1f tok/s" % (code, stats["n"], len(todo), len(failed), dt,
                                                                   stats["tok"] / dt), flush=True)

    with ThreadPoolExecutor(workers) as ex:
        list(ex.map(one, todo))
    json.dump(done, open(out, "w", encoding="utf-8"), ensure_ascii=False, indent=1, sort_keys=True)
    json.dump(failed, open(failp, "w", encoding="utf-8"), ensure_ascii=False, indent=1)
    print("%s: %d done, %d failed, %.0fs" % (code, len(done), len(failed), time.time() - t0), flush=True)


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
