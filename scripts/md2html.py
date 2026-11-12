# Turn a docs/*.md guide into one self-contained HTML page (contents sidebar,
# light/dark): python3 scripts/md2html.py docs/api-guide.md out.html [sidebar title]
import re, sys, html
src = open(sys.argv[1]).read().split('\n')
# Fenced code indented inside a list item: render it now and leave a one-line
# placeholder at the same indent, so the list parser keeps it in the item.
pres = []
_k = 0; _out = []
while _k < len(src):
    _m = re.match(r'^(\s+)```', src[_k])
    if _m:
        _ind = _m.group(1); _j = _k + 1; _buf = []
        while _j < len(src) and not src[_j].strip().startswith('```'):
            _buf.append(src[_j][len(_ind):] if src[_j].startswith(_ind) else src[_j].lstrip()); _j += 1
        pres.append('<pre><code>' + html.escape('\n'.join(_buf)) + '</code></pre>')
        _out.append(_ind + '\x00PRE' + str(len(pres) - 1) + '\x00'); _k = _j + 1; continue
    _out.append(src[_k]); _k += 1
src = _out
def inline(t):
    parts = re.split(r'(`[^`]+`)', t)
    out = []
    for p in parts:
        if p.startswith('`') and p.endswith('`') and len(p) > 1:
            out.append('<code>' + html.escape(p[1:-1]) + '</code>')
        else:
            e = html.escape(p, quote=False)
            e = re.sub(r'\*\*(.+?)\*\*', r'<strong>\1</strong>', e)
            e = re.sub(r'(?<![\w*])\*(?!\s)(.+?)(?<!\s)\*(?![\w*])', r'<em>\1</em>', e)
            # [text](#anchor or https://...) and bare https:// addresses
            e = re.sub(r'\[([^\]]+)\]\((#[\w-]+|https?://[^)\s]+)\)', lambda m: '<a href="' + m.group(2) + '">' + m.group(1) + '</a>', e)
            e = re.sub(r'(?<!href=")(?<!">)(https://[\w-]+(?:\.[\w-]+)+(?:/(?:[\w./?=&%#-]*[\w/])?)?)', r'<a href="\1">\1</a>', e)
            out.append(e)
    t = ''.join(out)
    # bold that contains inline code: **`x`**, **404 `not found`**
    t = re.sub(r'\*\*((?:(?!\*\*)[^\n])*?<code>(?:(?!\*\*)[^\n])*?)\*\*', r'<strong>\1</strong>', t)
    return re.sub(r'\s*\x00PRE(\d+)\x00\s*', lambda m: pres[int(m.group(1))], t)
def slugify(t):
    t = re.sub(r'<[^>]+>', '', t).lower()
    t = re.sub(r'[^a-z0-9]+', '-', t).strip('-')
    return t or 'section'
body, toc, used = [], [], set()
i = 0
def heading(level, text):
    s = slugify(text); base = s; n = 2
    while s in used: s = f'{base}-{n}'; n += 1
    used.add(s)
    if level in (2, 3): toc.append((level, s, inline(text)))
    body.append(f'<h{level} id="{s}">{inline(text)}</h{level}>')
while i < len(src):
    line = src[i]
    if line.startswith('```'):
        lang = line[3:].strip(); j = i + 1; buf = []
        while j < len(src) and not src[j].startswith('```'): buf.append(src[j]); j += 1
        body.append('<pre><code>' + html.escape('\n'.join(buf)) + '</code></pre>'); i = j + 1; continue
    m = re.match(r'^(#{1,4}) (.*)', line)
    if m: heading(len(m.group(1)), m.group(2)); i += 1; continue
    if line.strip() == '---': body.append('<hr>'); i += 1; continue
    if line.startswith('|'):
        rows = []
        while i < len(src) and src[i].startswith('|'): rows.append(src[i]); i += 1
        def cells(r):
            r = r.strip().strip('|'); out = []; cur = ''; k = 0; tick = False
            while k < len(r):
                c = r[k]
                if c == '`': tick = not tick
                if c == '\\' and k + 1 < len(r) and r[k+1] == '|': cur += '|'; k += 2; continue
                if c == '|' and not tick: out.append(cur.strip()); cur = ''
                else: cur += c
                k += 1
            out.append(cur.strip()); return out
        h = cells(rows[0]); t = ['<div class="tw"><table><thead><tr>' + ''.join('<th>' + inline(c) + '</th>' for c in h) + '</tr></thead><tbody>']
        for r in rows[2:]: t.append('<tr>' + ''.join('<td>' + inline(c) + '</td>' for c in cells(r)) + '</tr>')
        t.append('</tbody></table></div>'); body.append(''.join(t)); continue
    if line.startswith('>'):
        buf = []
        while i < len(src) and src[i].startswith('>'): buf.append(src[i].lstrip('>').strip()); i += 1
        body.append('<blockquote><p>' + inline(' '.join(buf)) + '</p></blockquote>'); continue
    if re.match(r'^\s*(- |\d+\. )', line):
        # list block (nested by indentation of 2+ spaces)
        items = []
        while i < len(src) and (re.match(r'^\s*(- |\d+\. )', src[i]) or (src[i].startswith('  ') and src[i].strip()) or (src[i].strip() == '' and i + 1 < len(src) and src[i+1].startswith('  ') and src[i+1].strip())):
            items.append(src[i]); i += 1
        def render(lines, indent):
            out = []; k = 0
            ordered = bool(re.match(r'^\s*\d+\. ', lines[0])) if lines else False
            tag = 'ol' if ordered else 'ul'
            out.append(f'<{tag}>')
            while k < len(lines):
                l = lines[k]
                m2 = re.match(r'^(\s*)(- |\d+\. )(.*)', l)
                if m2 and len(m2.group(1)) == indent:
                    text = [m2.group(3)]; sub = []; k += 1
                    while k < len(lines):
                        m3 = re.match(r'^(\s*)(- |\d+\. )', lines[k])
                        if m3 and len(m3.group(1)) == indent: break
                        if m3 and len(m3.group(1)) > indent: sub.append(lines[k])
                        elif sub: sub.append(lines[k])
                        else: text.append(lines[k].strip())
                        k += 1
                    item = inline(' '.join(t for t in text if t))
                    if sub:
                        sub_lines = [s for s in sub if s.strip()]
                        nested = [s for s in sub_lines if re.match(r'^\s*(- |\d+\. )', s)]
                        if nested:
                            ind = len(re.match(r'^(\s*)', nested[0]).group(1))
                            lst = [s for s in sub_lines if re.match(r'^\s*(- |\d+\. )', s) or len(re.match(r'^(\s*)', s).group(1)) > ind]
                            rest = [s for s in sub_lines if s not in lst]
                            item += render(lst, ind)
                            if rest: item += '<p>' + inline(' '.join(r.strip() for r in rest)) + '</p>'
                        else:
                            item += '<p>' + inline(' '.join(s.strip() for s in sub_lines)) + '</p>'
                    out.append('<li>' + item + '</li>')
                else: k += 1
            out.append(f'</{tag}>'); return ''.join(out)
        body.append(render(items, len(re.match(r'^(\s*)', items[0]).group(1)))); continue
    if line.strip() == '': i += 1; continue
    buf = []
    while i < len(src) and src[i].strip() and not re.match(r'^(#{1,4} |```|\||>|\s*- |\s*\d+\. |---$)', src[i]): buf.append(src[i].strip()); i += 1
    body.append('<p>' + inline(' '.join(buf)) + '</p>')
title = next((l[2:].strip() for l in src if l.startswith('# ')), 'AmberSight')
brand = sys.argv[3] if len(sys.argv) > 3 else 'AmberSight'
nav = ''.join(f'<a class="l{lv}" href="#{s}">{t}</a>' for lv, s, t in toc)
page = f'''<!DOCTYPE html>
<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1">
<title>{html.escape(title)}</title>
<style>
:root {{ --bg:#fbfaf7; --fg:#1f2328; --mute:#5b6168; --line:#e3e0d8; --code:#f2efe8; --acc:#b4690e; --side:#f4f1ea; }}
@media (prefers-color-scheme: dark) {{ :root {{ --bg:#15171a; --fg:#e6e3dd; --mute:#9aa0a6; --line:#2c3036; --code:#1f2226; --acc:#f0a64a; --side:#1a1d21; }} }}
* {{ box-sizing:border-box; }}
html {{ scroll-padding-top: 16px; }}
body {{ margin:0; background:var(--bg); color:var(--fg); font:16px/1.6 -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, Helvetica, Arial, sans-serif; }}
.wrap {{ display:grid; grid-template-columns: 260px minmax(0, 1fr); min-height:100vh; }}
nav {{ position:sticky; top:0; align-self:start; height:100vh; overflow:auto; padding:24px 16px; background:var(--side); border-right:1px solid var(--line); font-size:14px; }}
nav .brand {{ font-weight:700; margin-bottom:12px; color:var(--acc); }}
nav a {{ display:block; color:var(--fg); text-decoration:none; padding:3px 6px; border-radius:6px; }}
nav a:hover {{ background:var(--code); }}
nav a.l3 {{ padding-left:20px; color:var(--mute); font-size:13px; }}
main a {{ color:var(--acc); }}
main {{ max-width: 920px; padding: 32px 40px 80px; }}
h1 {{ font-size:30px; margin:0 0 16px; }}
h2 {{ font-size:23px; margin:40px 0 12px; padding-bottom:6px; border-bottom:1px solid var(--line); }}
h3 {{ font-size:18px; margin:28px 0 8px; }}
code {{ font:13.5px/1.5 ui-monospace, SFMono-Regular, Menlo, Consolas, monospace; background:var(--code); padding:1px 5px; border-radius:4px; }}
pre {{ background:var(--code); border:1px solid var(--line); border-radius:8px; padding:12px 14px; overflow:auto; }}
pre code {{ background:none; padding:0; }}
.tw {{ overflow-x:auto; margin:12px 0; }}
table {{ border-collapse:collapse; width:100%; font-size:14.5px; }}
th, td {{ border:1px solid var(--line); padding:6px 9px; text-align:left; vertical-align:top; }}
th {{ background:var(--side); }}
blockquote {{ margin:16px 0; padding:10px 16px; border-left:4px solid var(--acc); background:var(--side); border-radius:0 8px 8px 0; }}
hr {{ border:0; border-top:1px solid var(--line); margin:32px 0; }}
li {{ margin:3px 0; }}
li > p {{ margin:4px 0; }}
@media (max-width: 860px) {{ .wrap {{ grid-template-columns: 1fr; }} nav {{ position:static; height:auto; border-right:0; border-bottom:1px solid var(--line); }} main {{ padding:20px 16px 60px; }} }}
</style></head>
<body><div class="wrap"><nav aria-label="Contents"><div class="brand">{html.escape(brand)}</div>{nav}</nav>
<main>{''.join(body)}</main></div></body></html>'''
open(sys.argv[2], 'w').write(page)
print('ok', len(page), 'bytes;', len(toc), 'toc entries')
