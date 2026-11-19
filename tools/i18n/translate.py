#!/usr/bin/env python3
"""Translate lang/en.json into lang/<code>.json with a local LLM. Only messages that are missing or
whose English changed (fingerprint) are sent. Every answer is checked (placeholders, plural forms,
HTML tags) and a failed one is sent back once with the reason; still failing, it stays English.

  LLM_URL=http://host:port tools/i18n/translate.py de [es ...]
"""
import json, os, sys, time, urllib.request
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from icu import check_translation, PLURALS, OPTIONAL  # noqa: E402

LLM = os.environ.get("LLM_URL", "").rstrip("/")
BATCH = int(os.environ.get("BATCH", "40"))
EFFORT = os.environ.get("EFFORT", "low")
HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.normpath(os.path.join(HERE, "..", ".."))

LANGS = {
    "de": ("German", "Use the informal 'du' consistently, as the Android app does."),
    "es": ("Spanish (neutral, for Spain and Latin America)", "Use informal 'tú' as the app does."),
    "fr": ("French", "Use the formal 'vous'."),
    "nl": ("Dutch", "Use informal 'je/jij'."),
    "it": ("Italian", "Use informal 'tu' as Italian software usually does."),
    "zh-CN": ("Simplified Chinese", "Use 您 sparingly; plain modern software Chinese."),
    "pl": ("Polish", "Address the user impersonally or with informal forms as Polish software does."),
    "pt-BR": ("Brazilian Portuguese", "Use 'você'."),
    "sv": ("Swedish", "Use informal 'du'."),
    "ru": ("Russian", "Use the formal 'вы' (lower case)."),
}

# Writing conventions per language, added to the prompt.
STYLE = {
    "de": "Nouns capitalised; prefer short compounds over long phrases; 'E-Mail', 'Push-Benachrichtigung'.",
    "fr": "Put a non-breaking space before : ; ! ? (write a normal space; the page handles it). Use « » only in sentences.",
    "nl": "Keep English tech words Dutch software uses (stream, snapshot) only if the glossary says so.",
    "it": "Use the infinitive for buttons and menu items (Salva, Annulla).",
    "zh-CN": "Use full-width punctuation (，。：；！？（）) in Chinese text, but keep ASCII around {placeholders}, numbers and units. No spaces between Chinese characters; one space between Chinese and Latin words is fine.",
    "pl": "Polish needs one, few, many and other plural forms; pick the right case after numbers.",
    "pt-BR": "Brazilian usage (tela, arquivo, celular), not European Portuguese.",
    "sv": "Short imperative buttons (Spara, Avbryt).",
    "ru": "Russian needs one, few, many and other plural forms; use «» quotes; 'ё' may be written as 'е'.",
}

GLOSSARY = os.environ.get("GLOSSARY", os.path.join(HERE, "glossary.json"))


def prompt_for(code):
    name, address = LANGS[code]
    gl = json.load(open(GLOSSARY, encoding="utf-8")) if os.path.exists(GLOSSARY) else {}
    extra = os.path.join(HERE, "glossary-extra.json")
    if os.path.exists(extra):
        for en, tr in json.load(open(extra, encoding="utf-8")).items():
            if not en.startswith("_"):
                gl.setdefault(en, {}).update({k: v for k, v in tr.items() if k not in gl.get(en, {})})
    terms = "\n".join("- %s → %s" % (en, tr[code]) for en, tr in gl.items() if code in tr)
    forms = sorted(PLURALS.get(code, {"other"}))
    return f"""You translate the web app of AmberSight, a self-hosted security camera recorder (NVR), from English into {name}.
Tone: plain, short, direct, like good software UI text. {address} {STYLE.get(code, "")}
Keep it about as short as the English; button and tab labels must stay short.

Rules for every message:
- Messages use ICU MessageFormat. Keep every placeholder like {{name}} exactly (same name, not translated).
- Plural: {{n, plural, one {{# camera}} other {{# cameras}}}} - translate inside each branch and give exactly the
  plural forms {name} needs: {', '.join(forms)}. Keep '#' (the number) and any '=0' branches.
- Select: {{kind, select, car {{...}} other {{...}}}} - keep the selector words (car, other) in English, translate the branches.
- HTML tags and entities (&nbsp; &larr; <b> </b> <a href=...> ...) must appear exactly as in the English, same order.
  Never add a < or a straight double quote "; for quotation marks use the language's typographic quotes („“ « » 「」).
- Do not translate: product and brand names (AmberSight, Reolink, ONVIF, RTSP, MQTT, HomeKit, Home Assistant,
  Telegram, ntfy, Pushover, NVIDIA, Intel, OpenVINO), file names, units (s, ms, MB, GB, fps, kbps), keyboard keys.
- A literal apostrophe is written as '' (two apostrophes) inside a message that has placeholders.
Glossary (use these words):
{terms}

You get JSON {{"items":[{{"key":..., "en":...}}]}}. Answer JSON only: {{"items":[{{"key":..., "text":...}}]}} with every key."""


SCHEMA = {"type": "object", "properties": {"items": {"type": "array", "items": {"type": "object",
          "properties": {"key": {"type": "string"}, "text": {"type": "string"}}, "required": ["key", "text"]}}},
          "required": ["items"]}


def ask(system, items):
    body = {"model": "m", "temperature": 0.2, "max_tokens": 16000,
            "messages": [{"role": "system", "content": system},
                         {"role": "user", "content": json.dumps({"items": items}, ensure_ascii=False)}],
            "chat_template_kwargs": {"reasoning_effort": EFFORT, "enable_thinking": os.environ.get("THINK", "0") == "1"},
            "response_format": {"type": "json_schema", "json_schema": {"name": "tr", "schema": SCHEMA}}}
    req = urllib.request.Request(LLM + "/v1/chat/completions", json.dumps(body).encode(), {"Content-Type": "application/json"})
    err = None
    for _ in range(3):
        try:
            with urllib.request.urlopen(req, timeout=1800) as r:
                j = json.load(r)
            return {x["key"]: x["text"] for x in json.loads(j["choices"][0]["message"]["content"])["items"]}
        except Exception as e:  # noqa: BLE001
            err = e
            time.sleep(5)
    raise RuntimeError("LLM failed: %s" % err)


def run(code):
    en = json.load(open(os.path.join(ROOT, "lang", "en.json"), encoding="utf-8"))["messages"]
    path = os.path.join(ROOT, "lang", code + ".json")
    have = json.load(open(path, encoding="utf-8"))["messages"] if os.path.exists(path) else {}
    todo = [k for k, (txt, fp) in en.items() if not (k in have and have[k][1] == fp)]
    system = prompt_for(code)
    failed, t0 = {}, time.time()
    print("%s: %d to translate" % (code, len(todo)), flush=True)

    def save():
        msgs = {k: v for k, v in have.items() if k in en}
        json.dump({"lang": code, "machine": True, "messages": msgs}, open(path, "w", encoding="utf-8"),
                  ensure_ascii=False, indent=0, sort_keys=True)

    for i in range(0, len(todo), BATCH):
        keys = todo[i:i + BATCH]
        got = ask(system, [{"key": k, "en": en[k][0]} for k in keys])
        retry = []
        for k in keys:
            why = check_translation(got.get(k, ""), en[k][0], code) if k in got else "missing from answer"
            if why:
                retry.append((k, why))
            else:
                have[k] = [got[k], en[k][1]]
        if retry:
            again = ask(system, [{"key": k, "en": en[k][0], "previous_problem": w} for k, w in retry])
            for k, w in retry:
                why = check_translation(again.get(k, ""), en[k][0], code) if k in again else "missing from answer"
                if why:
                    failed[k] = why
                else:
                    have[k] = [again[k], en[k][1]]
        save()
        print("  %s %d/%d done, %d failed, %.0fs" % (code, min(i + BATCH, len(todo)), len(todo), len(failed), time.time() - t0), flush=True)
    save()
    os.makedirs(os.path.join(HERE, "out"), exist_ok=True)
    json.dump(failed, open(os.path.join(HERE, "out", code + ".failed.json"), "w"), ensure_ascii=False, indent=1)
    print("%s: %d translated, %d left in English" % (code, len(todo) - len(failed), len(failed)), flush=True)


if __name__ == "__main__":
    if not LLM:
        sys.exit("set LLM_URL")
    for c in sys.argv[1:]:
        if c not in LANGS:
            sys.exit("unknown language %s (have %s)" % (c, " ".join(LANGS)))
        run(c)
