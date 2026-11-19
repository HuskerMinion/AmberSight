#!/usr/bin/env python3
"""find-untranslated.py - lists string literals in static/js/*.js that look like
user-visible English but are not inside t(). Read-only.

  python3 tools/i18n/find-untranslated.py [files...]    (default: static/js/*.js)

Output: "file:line: text", one per line, then a count per file. Exit 1 if any
are found, so it can serve as the "no new English-only text" test.

Ignore file: tools/i18n/untranslated-ok.txt, one "file:text" per line
(file is the basename, e.g. "cameras.js:Sign in"; '#' starts a comment line).

What counts as user-visible English: 2+ words made of letters, or one
capitalised word ("Revoke"). HTML in a literal is read for its text and for
title / placeholder / aria-label / alt values; other attributes are skipped.
Skipped: comments, console.*, import/export-from lines, regex literals, URLs and
routes, CSS-looking text, selectors, and literals compared with ===, !==, ==, !=
or `case`, or passed as the first argument of DOM/storage/event calls.
"""
import glob
import os
import re
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.abspath(os.path.join(HERE, '..', '..'))
OK_FILE = os.path.join(HERE, 'untranslated-ok.txt')

# Calls whose string arguments are never shown to people.
QUIET_CALLS = {
    'querySelector', 'querySelectorAll', 'closest', 'matches', 'getElementById', 'getElementsByClassName',
    'getElementsByTagName', 'addEventListener', 'removeEventListener', 'getAttribute', 'setAttribute',
    'removeAttribute', 'hasAttribute', 'toggleAttribute', 'add', 'remove', 'toggle', 'contains', 'replace',
    'getItem', 'setItem', 'removeItem', 'emit', 'on', 'off', 'once', 'fetch', 'api', 'import', 'createElement',
    'createElementNS', 'require', 'matchMedia', 'postMessage', 'includes', 'indexOf', 'startsWith', 'endsWith',
    'split', 'test', 'exec', 'match', 'padStart', 'padEnd', 'RegExp', 'Error', 'TypeError', 'cssText',
    'setProperty', 'getPropertyValue', 'removeProperty', 'dispatchEvent', 'CustomEvent', 'Event', 'URL',
    'Intl', 'DateTimeFormat', 'NumberFormat', 'PluralRules', 'toLocaleString', 'toLocaleDateString',
    'toLocaleTimeString', 'register', 'get', 'set', 'has', 'delete', 'resolve', 'open', 'sendBeacon',
    'Worker', 'WebSocket', 'EventSource', 'Audio', 'Image', 'Date', 'parse', 'cookie', 'mark', 'log',
    'warn', 'info', 'debug', 'trace', 'assert', 'goTab', 'showTab', 'bus', 'hash', 'pathname',
}
CMP_BEFORE = {'===', '!==', '==', '!='}

ATTR_WORDS = {'checked', 'disabled', 'selected', 'hidden', 'readonly', 'required', 'open', 'autofocus', 'multiple'}
ATTR_SHOWN = {'title', 'placeholder', 'aria-label', 'alt', 'label'}
TAG_RE = re.compile(r'<[^>]*>')
ENT_RE = re.compile(r'&(?:#\d+|#x[0-9a-fA-F]+|[a-zA-Z]+);')
ATTR_RE = re.compile(r'([a-zA-Z][\w:-]*)\s*=\s*(?:"([^"]*)"|\'([^\']*)\')')
WORD_RE = re.compile(r"[A-Za-z][A-Za-z'’]+")
IDENT_RE = re.compile(r'^[A-Za-z_$][\w$]*$')
CSSY_RE = re.compile(r'^\s*[-\w]+\s*:\s*[^ ]|;\s*[-\w]+\s*:|^[.#\[]|^\d|^\s*[-+*/%]|^[a-z-]+\([^)]*\)$')
URLY_RE = re.compile(r'^(https?:|wss?:|data:|blob:|mailto:|tel:|#|/|\./|\.\./|\?|&)|^\S+\.(js|css|png|svg|jpg|mp4|json|html|webm|ico)$')


def lex(src):
    """Yield (kind, text, line, prev_tok, next_tok_placeholder_index).

    Tokens: ('str', value, line), ('tok', text, line) for identifiers/punctuation.
    Comments and regex literals are dropped. Template literals yield their text
    pieces as 'str' and their ${...} code as ordinary tokens.
    """
    i, n, line = 0, len(src), 1
    out = []
    tmpl = []          # stack of brace depths for ${ ... } inside templates
    depth = 0
    last = ''          # last significant token text, for regex detection

    def regex_allowed():
        if not last:
            return True
        if last in (')', ']', '}'):
            return False
        if re.match(r'^[\w$]+$', last) and last not in ('return', 'typeof', 'case', 'in', 'of', 'delete', 'void', 'throw', 'new', 'else', 'do', 'yield', 'await'):
            return False
        return True

    def read_template(i, line):
        buf = []
        start = line
        while i < n:
            c = src[i]
            if c == '\\':
                buf.append(src[i + 1] if i + 1 < n else '')
                if i + 1 < n and src[i + 1] == '\n':
                    line += 1
                i += 2
                continue
            if c == '`':
                out.append(('str', ''.join(buf), start))
                return i + 1, line, False
            if c == '$' and i + 1 < n and src[i + 1] == '{':
                out.append(('str', ''.join(buf), start))
                out.append(('tok', '${', line))
                return i + 2, line, True
            if c == '\n':
                line += 1
            buf.append(c)
            i += 1
        return i, line, False

    while i < n:
        c = src[i]
        if c == '\n':
            line += 1
            i += 1
            continue
        if c in ' \t\r':
            i += 1
            continue
        if src.startswith('//', i):
            j = src.find('\n', i)
            i = n if j < 0 else j
            continue
        if src.startswith('/*', i):
            j = src.find('*/', i + 2)
            j = n if j < 0 else j + 2
            line += src.count('\n', i, j)
            i = j
            continue
        if c in '\'"':
            q, j, buf, start = c, i + 1, [], line
            while j < n and src[j] != q and src[j] != '\n':
                if src[j] == '\\' and j + 1 < n:
                    nx = src[j + 1]
                    buf.append({'n': ' ', 't': ' ', "'": "'", '"': '"', '\\': '\\'}.get(nx, nx))
                    if nx == '\n':
                        line += 1
                    j += 2
                    continue
                buf.append(src[j])
                j += 1
            out.append(('str', ''.join(buf), start))
            last = 'STR'
            i = j + 1
            continue
        if c == '`':
            i, line, expr = read_template(i + 1, line)
            if expr:
                tmpl.append(depth)
                depth += 1
                last = '${'
            else:
                last = 'STR'
            continue
        if c == '/' and regex_allowed():
            j, in_cls = i + 1, False
            while j < n and src[j] != '\n':
                if src[j] == '\\':
                    j += 2
                    continue
                if src[j] == '[':
                    in_cls = True
                elif src[j] == ']':
                    in_cls = False
                elif src[j] == '/' and not in_cls:
                    break
                j += 1
            i = j + 1
            while i < n and src[i].isalpha():
                i += 1
            last = 'REGEX'
            out.append(('tok', 'REGEX', line))
            continue
        m = re.match(r'[A-Za-z_$][\w$]*|\d[\w.]*|===|!==|==|!=|=>|\?\.|\.\.\.|[^\s]', src[i:i + 40])
        tok = m.group(0) if m else c
        i += len(tok)
        if tok == '{':
            depth += 1
        elif tok == '}':
            depth -= 1
            if tmpl and depth == tmpl[-1]:
                tmpl.pop()
                out.append(('tok', '}', line))
                i, line, expr = read_template(i, line)
                if expr:
                    tmpl.append(depth)
                    depth += 1
                last = 'STR'
                continue
        out.append(('tok', tok, line))
        last = tok
    return out


def candidates(s):
    """Texts in a literal that a person could read."""
    if '<' in s and '>' in s:
        found = []
        for tag in re.findall(r'<[^>]*>', s):
            for k, d, sq in ATTR_RE.findall(tag):
                if k.lower() in ATTR_SHOWN:
                    found.append(d or sq)
        for piece in TAG_RE.split(s):
            found.append(piece)
        return found
    return [s]


def looks_english(text):
    t = ENT_RE.sub(' ', text).strip()
    if not t or len(t) < 3:
        return False
    if URLY_RE.search(t) or CSSY_RE.search(t):
        return False
    if re.search(r'[{}<>=]|\$\{|\\', t):
        return False
    toks = t.split()
    if len(toks) == 1:
        # one word: only a capitalised plain word ("Revoke", "Save:")
        return bool(re.fullmatch(r"[A-Z][a-z]{2,}[.:!?]?", t))
    if all(w.startswith('data-') or w in ATTR_WORDS for w in toks):
        return False
    letter = [w for w in toks if re.search(r'[A-Za-z]{2}', w)]
    if len(letter) < 2:
        return False
    # CSS values ("transform .18s ease", "px) scale(") are not prose
    if any(re.fullmatch(r'[-+]?\.?\d+(\.\d+)?(s|ms|px|em|rem|%|deg|vh|vw)?', w) and re.search(r'[a-z]', t) and re.search(r'\d(s|ms|px|em|rem|%|deg)\b', w) for w in toks):
        return False
    if re.search(r'\w\(|\)\s*\w', t) and not re.search(r'[A-Z].*[a-z]{3}', t):
        return False
    return True


def scan(path):
    src = open(path, encoding='utf-8').read()
    toks = lex(src)
    hits = []
    # Paren stack: each entry is the name of the callee ('' for a plain group).
    stack = []
    # Line of the import/export-from statement: skip strings after `from` or `import`.
    for idx, (kind, val, line) in enumerate(toks):
        if kind == 'tok':
            if val == '(':
                prev = toks[idx - 1][1] if idx else ''
                prevk = toks[idx - 1][0] if idx else 'tok'
                stack.append(prev if prevk == 'tok' and IDENT_RE.match(prev or '') else '')
            elif val == ')':
                if stack:
                    stack.pop()
            continue
        # string literal
        if not val.strip():
            continue
        prev = toks[idx - 1][1] if idx else ''
        prev2 = toks[idx - 2][1] if idx > 1 else ''
        nxt = toks[idx + 1][1] if idx + 1 < len(toks) else ''
        if 'i18n-ok' in src.split('\n')[line - 1]:
            continue
        # inside t(...): any callee on the stack named t
        if 't' in stack:
            continue
        if prev in CMP_BEFORE or nxt in CMP_BEFORE or prev == 'case':
            continue
        if prev in ('from', 'import') or (prev == '(' and prev2 == 'import'):
            continue
        # inside console.*(...)
        if any(c in ('log', 'warn', 'error', 'info', 'debug', 'trace', 'assert') for c in stack) and _console(toks, idx):
            continue
        # direct argument of a quiet call
        if stack and stack[-1] in QUIET_CALLS and prev in ('(', ','):
            continue
        if prev in ('=', '+=') and prev2 in ('className', 'class'):
            continue
        # object key  { 'a b': ... }
        if nxt == ':' and prev in ('{', ','):
            continue
        # computed member / index: x['some thing']
        if prev == '[' and nxt == ']':
            continue
        for cand in candidates(val):
            if looks_english(cand):
                hits.append((line, ' '.join(ENT_RE.sub(' ', cand).split())))
    return hits


def _console(toks, idx):
    """True if the innermost open call before idx is console.<x>(."""
    depth = 0
    for j in range(idx - 1, -1, -1):
        k, v, _ = toks[j]
        if k != 'tok':
            continue
        if v == ')':
            depth += 1
        elif v == '(':
            if depth == 0:
                return j >= 2 and toks[j - 2][1] == 'console'
            depth -= 1
    return False


def load_ok():
    ok = set()
    if os.path.exists(OK_FILE):
        for ln in open(OK_FILE, encoding='utf-8'):
            ln = ln.rstrip('\n')
            if ln.strip() and not ln.lstrip().startswith('#') and ':' in ln:
                f, _, text = ln.partition(':')
                ok.add((f.strip(), text.strip()))
    return ok


def main():
    files = sys.argv[1:] or sorted(glob.glob(os.path.join(ROOT, 'static', 'js', '*.js')))
    ok = load_ok()
    counts = {}
    for f in files:
        base = os.path.basename(f)
        # the runtime itself and the tiny boot scripts hold no UI text
        if base in ('i18n.js', 'prepaint.js', 'diag.js'):
            continue
        for line, text in scan(f):
            if (base, text) in ok:
                continue
            print(f'{os.path.relpath(f, ROOT) if f.startswith(ROOT) else f}:{line}: {text}')
            counts[base] = counts.get(base, 0) + 1
    if counts:
        print()
        for b in sorted(counts):
            print(f'{counts[b]:5d}  {b}')
        print(f'{sum(counts.values()):5d}  total')
    else:
        print('No untranslated text found.')
    return 1 if counts else 0


if __name__ == '__main__':
    sys.exit(main())
