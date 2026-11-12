#!/usr/bin/env python3
# Draws every AmberSight icon from one mark: a camera aperture (six amber
# blades) opening into an eye. Writes the SVGs, PNGs, .ico and .icns in
# static/icons, static/favicon.ico and docs/images/logo-mark.svg.
#
# Needs: pip install cairosvg pillow   (cairosvg needs the cairo library)
#   python3 scripts/make-icons.py
#
# The README lockups (docs/images/logo-light.svg, logo-dark.svg) were made
# from the same mark with the Inter font's letters turned into outlines.
import io, os, struct, sys

import cairosvg
from PIL import Image

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
ICONS = os.path.join(ROOT, 'static', 'icons')
AMBER = '#f2a650'
ALERT = '#e5484d'
TILE = '#171d23'


BLADE = 'M32 32 32 6A26 26 0 0 1 54.5 19Z'
SLIT = 'M14 32s7.5-10 18-10 18 10 18 10-7.5 10-18 10-18-10-18-10z'


def placed(body, scale, size):
    """`body` (in the mark's 64-unit design) centered in a size x size box."""
    c = size / 2
    return f'<g transform="translate({c} {c}) scale({scale}) translate(-32 -32)">{body}</g>'


def blades(fg):
    return ''.join(f'<path d="{BLADE}" fill="{fg}" fill-opacity="{1 if i % 2 else .55}" transform="rotate({i * 60} 32 32)"/>' for i in range(6))


def pupil(fg):
    return f'<circle cx="32" cy="32" r="5.5" fill="{fg}"/>'


def mark(fg, scale, size=512, mid='m'):
    """The mark as SVG. The eye opening is cut out of the blades with an
    even-odd clip (the whole square minus the eye), so whatever is behind the
    icon (the tile or the page) shows through it."""
    return (f'<defs><clipPath id="{mid}" clipPathUnits="userSpaceOnUse">'
            f'<path clip-rule="evenodd" d="M-1-1H65V65H-1Z{SLIT}"/></clipPath></defs>'
            + placed(f'<g clip-path="url(#{mid})">{blades(fg)}</g>{pupil(fg)}', scale, size))


def svg(body, size=512):
    return f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {size} {size}">{body}</svg>\n'


def tile(rx, size=512):
    return f'<rect width="{size}" height="{size}" rx="{rx}" fill="{TILE}"/>'


# Each icon: (tile corner radius or None for no tile, color, scale in a 512 box).
# The mark is 52 units across, so 5.76 is ~58% of the tile (as before);
# maskable icons keep it inside the 80% safe circle; bare marks fill the box.
SPECS = {
    'icon.svg': (96, AMBER, 5.76),
    'icon-square.svg': (0, AMBER, 5.76),
    'icon-maskable.svg': (0, AMBER, 4.6),
    'favicon.svg': (None, AMBER, 9.4),
    'favicon-alert.svg': (None, ALERT, 9.4),
    'badge.svg': (None, '#ffffff', 9.4),
}
FILES = {n: svg(('' if rx is None else tile(rx)) + mark(fg, sc)) for n, (rx, fg, sc) in SPECS.items()}


def layer(body, px):
    data = cairosvg.svg2png(bytestring=svg(body).encode(), output_width=px, output_height=px)
    return Image.open(io.BytesIO(data)).convert('RGBA')


def png(name, px, rgb=False):
    """Draws an icon at px x px. cairo ignores clip paths, so the eye opening
    is cut out here: the blades lose their alpha where the eye shape is."""
    rx, fg, sc = SPECS[name]
    im = layer(tile(rx), px) if rx is not None else Image.new('RGBA', (px, px), (0, 0, 0, 0))
    b = layer(placed(blades(fg), sc, 512), px)
    cut = layer(placed(f'<path d="{SLIT}" fill="#fff"/>', sc, 512), px).getchannel('A')
    # blade alpha x (1 - eye coverage): keeps the anti-aliased edge
    a = b.getchannel('A').load(); c = cut.load(); out = Image.new('L', b.size); o = out.load()
    for y in range(px):
        for x in range(px):
            o[x, y] = a[x, y] * (255 - c[x, y]) // 255
    b.putalpha(out)
    im.alpha_composite(b)
    im.alpha_composite(layer(placed(pupil(fg), sc, 512), px))
    if rgb:  # opaque icons (iOS, maskable): flatten on the tile color
        bg = Image.new('RGBA', im.size, TILE)
        bg.alpha_composite(im)
        im = bg.convert('RGB')
    return im


def png_bytes(im):
    b = io.BytesIO()
    im.save(b, 'PNG', optimize=True)
    return b.getvalue()


def write_ico(path, name, sizes):
    """.ico with one PNG picture per size (what Windows and setup.rs expect)."""
    pics = [png_bytes(png(name, s)) for s in sizes]
    head = struct.pack('<HHH', 0, 1, len(sizes))
    off = 6 + 16 * len(sizes)
    dirs = b''
    for s, data in zip(sizes, pics):
        dirs += struct.pack('<BBBBHHII', s % 256, s % 256, 0, 0, 1, 32, len(data), off)
        off += len(data)
    with open(path, 'wb') as f:
        f.write(head + dirs + b''.join(pics))


def write_icns(path, name):
    """.icns with PNG entries icp4 (16) .. ic09 (512)."""
    body = b''
    for kind, s in (('icp4', 16), ('icp5', 32), ('icp6', 64), ('ic07', 128), ('ic08', 256), ('ic09', 512)):
        data = png_bytes(png(name, s))
        body += kind.encode() + struct.pack('>I', 8 + len(data)) + data
    with open(path, 'wb') as f:
        f.write(b'icns' + struct.pack('>I', 8 + len(body)) + body)


def main():
    for name, text in FILES.items():
        with open(os.path.join(ICONS, name), 'w') as f:
            f.write(text)
    png('icon.svg', 192).save(os.path.join(ICONS, 'icon-192.png'), optimize=True)
    png('icon.svg', 512).save(os.path.join(ICONS, 'icon-512.png'), optimize=True)
    png('icon-square.svg', 180, rgb=True).save(os.path.join(ICONS, 'apple-touch-icon.png'), optimize=True)
    png('icon-maskable.svg', 512, rgb=True).save(os.path.join(ICONS, 'icon-maskable-512.png'), optimize=True)
    png('badge.svg', 96).save(os.path.join(ICONS, 'badge-96.png'), optimize=True)
    # Program icons: the tiled mark, drawn at each size (sharper than scaling one picture).
    write_ico(os.path.join(ICONS, 'ambersight.ico'), 'icon.svg', (16, 32, 48, 256))
    write_ico(os.path.join(ROOT, 'static', 'favicon.ico'), 'favicon.svg', (16, 32, 48))
    write_icns(os.path.join(ICONS, 'ambersight.icns'), 'icon.svg')
    with open(os.path.join(ROOT, 'docs', 'images', 'logo-mark.svg'), 'w') as f:
        f.write(svg(tile(14, 64) + mark(AMBER, 0.72, 64), 64))
    print('icons written')


if __name__ == '__main__':
    sys.exit(main())
