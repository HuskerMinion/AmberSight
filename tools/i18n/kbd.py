#!/usr/bin/env python3
"""Key names inside <kbd> as that language's keyboards and OS print them (letters, arrows, symbols unchanged).

  kbd.py <code> [<code> ...]     rewrites lang/<code>.json in place, prints what changed
Shortcuts are matched by key code in the app, so this changes only what the user reads."""
import json, os, re, sys
ROOT = os.path.normpath(os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", ".."))
KEYS = {  # English -> name on that language's keyboard / in its OS
    "de": {"Ctrl": "Strg", "Space": "Leertaste", "Enter": "Eingabe", "Esc": "Esc", "End": "Ende"},
    "fr": {"Ctrl": "Ctrl", "Space": "Espace", "Enter": "Entrée", "Esc": "Échap", "End": "Fin"},
    "es": {"Ctrl": "Ctrl", "Space": "Espacio", "Enter": "Intro", "Esc": "Esc", "End": "Fin"},
    "it": {"Ctrl": "Ctrl", "Space": "Spazio", "Enter": "Invio", "Esc": "Esc", "End": "Fine"},
    "nl": {"Space": "Spatie"},
    "pt-BR": {"Space": "Espaço"},
    "pl": {"Space": "Spacja"},
    "ru": {"Space": "Пробел"},
    "sv": {"Space": "Mellanslag"},
    "zh-CN": {"Space": "空格"},
}


def fix(code):
    p = os.path.join(ROOT, "lang", code + ".json")
    pack = json.load(open(p, encoding="utf-8"))
    names = KEYS.get(code, {})
    # what a translator may have written already for those keys, mapped back to English
    seen = {v.lower(): k for k, v in names.items()}
    seen.update({"strg": "Ctrl", "leertaste": "Space", "eingabe": "Enter", "échap": "Esc", "espace": "Space",
                 "entrée": "Enter", "espacio": "Space", "spazio": "Space", "invio": "Enter", "spatie": "Space",
                 "espaço": "Space", "spacja": "Space", "пробел": "Space", "mellanslag": "Space", "空格": "Space",
                 "ende": "End", "fin": "End", "fine": "End", "intro": "Enter", "entrar": "Enter"})
    en = json.load(open(os.path.join(ROOT, "lang", "en.json"), encoding="utf-8"))["messages"]
    n = 0
    for k, v in pack["messages"].items():
        if "<kbd>" not in v[0] or k not in en:
            continue

        def one(m):
            key = m.group(1)
            eng = key if key in ("Ctrl", "Space", "Enter", "Esc", "End") else seen.get(key.lower())
            return "<kbd>%s</kbd>" % names.get(eng, eng) if eng else m.group(0)
        new = re.sub(r"<kbd>(.*?)</kbd>", one, v[0])
        if new != v[0]:
            print(code, k, re.findall(r"<kbd>(.*?)</kbd>", v[0]), "->", re.findall(r"<kbd>(.*?)</kbd>", new))
            v[0] = new
            n += 1
    json.dump(pack, open(p, "w", encoding="utf-8"), ensure_ascii=False, indent=0, sort_keys=True)
    print("%s: %d messages changed" % (code, n))


if __name__ == "__main__":
    for c in sys.argv[1:]:
        fix(c)
