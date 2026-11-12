#!/usr/bin/env python3
# Writes THIRD-PARTY-NOTICES.txt: the licenses and copyright notices of the
# code AmberSight is built from (Rust crates in the release build, the
# vendored RTSP library) plus the runtimes and AI models it uses.
#
#   python3 scripts/third-party-notices.py            # writes THIRD-PARTY-NOTICES.txt
#   python3 scripts/third-party-notices.py --check    # fails if the file is out of date
#
# Run it after changing dependencies (Cargo.lock) and commit the result. The
# packaging scripts ship the file next to LICENSE.
import json, os, re, subprocess, sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
OUT = os.path.join(ROOT, 'THIRD-PARTY-NOTICES.txt')
LICENSE_NAME = re.compile(r'^(licen[cs]e|copying|notice|unlicense|copyright|authors)([-._].*)?$', re.I)

# Not Rust crates: shipped with or downloaded by AmberSight. Downloads are not
# part of the AmberSight packages; they are listed so users know their terms.
OTHER = """\
ONNX Runtime (AI inference runtime, linked into AmberSight)
  Copyright (c) Microsoft Corporation. License: MIT.
  https://github.com/microsoft/onnxruntime (its own third-party notices:
  https://github.com/microsoft/onnxruntime/blob/main/ThirdPartyNotices.txt)

DirectML (DirectML.dll, Windows packages only: graphics-card AI)
  Copyright (c) Microsoft Corporation. Redistributed under the Microsoft
  DirectML license terms: https://www.nuget.org/packages/Microsoft.AI.DirectML

AI models AmberSight downloads when you turn a feature on (not included in
the AmberSight packages; each keeps its own license):
  - YOLOX object detection models, Megvii: Apache-2.0.
    https://github.com/Megvii-BaseDetection/YOLOX
  - CLIP ViT-B/32 (smart search), OpenAI: MIT; ONNX conversion by Xenova.
    https://github.com/openai/CLIP, https://huggingface.co/Xenova/clip-vit-base-patch32
  - YuNet face detection: MIT; SFace face recognition: Apache-2.0
    (OpenCV model zoo). https://github.com/opencv/opencv_zoo
  - YAMNet sound classes, Google: Apache-2.0; ONNX conversion.
    https://github.com/tensorflow/models/tree/master/research/audioset/yamnet,
    https://huggingface.co/audiomagic/yamnet-onnx
  - fast-plate-ocr license plate reader, ankandrew: MIT.
    https://github.com/ankandrew/fast-plate-ocr

License plate finder (included in AmberSight, models/plate_yolox.onnx)
  AmberSight's own YOLOX-Nano model. Built with YOLOX by Megvii (Apache-2.0),
  starting from its COCO weights: https://github.com/Megvii-BaseDetection/YOLOX
  Trained on the Open Images Dataset V7 (annotations by Google LLC, CC BY 4.0,
  https://creativecommons.org/licenses/by/4.0/; images CC BY 2.0,
  https://creativecommons.org/licenses/by/2.0/, by their respective authors).
  https://storage.googleapis.com/openimages/web/index.html

ffmpeg (video tools AmberSight runs as a separate program)
  Not included in the AmberSight packages. On Windows and macOS AmberSight
  downloads a build from its builder (GPL v3); on Linux the system's ffmpeg is
  used. FFmpeg is LGPL/GPL: https://ffmpeg.org/legal.html
"""

MIT_TEXT = """\
MIT License

Copyright (c) the authors of the crate named above

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE."""

ZLIB_TEXT = """\
zlib License

Copyright (c) the authors of the crate named above

This software is provided 'as-is', without any express or implied warranty.
In no event will the authors be held liable for any damages arising from the
use of this software.

Permission is granted to anyone to use this software for any purpose,
including commercial applications, and to alter it and redistribute it
freely, subject to the following restrictions:

1. The origin of this software must not be misrepresented; you must not
   claim that you wrote the original software. If you use this software in a
   product, an acknowledgment in the product documentation would be
   appreciated but is not required.
2. Altered source versions must be plainly marked as such, and must not be
   misrepresented as being the original software.
3. This notice may not be removed or altered from any source distribution."""


def apache_text():
    with open(os.path.join(ROOT, 'vendor', 'retina', 'LICENSE-APACHE.txt'), encoding='utf-8') as fh:
        return fh.read().replace('\r\n', '\n').strip()


def metadata():
    out = subprocess.run(['cargo', 'metadata', '--format-version', '1', '--locked'], cwd=ROOT, check=True, capture_output=True, text=True).stdout
    return json.loads(out)


def build_packages(md):
    """Packages reached from AmberSight through normal and build dependencies
    (not dev-dependencies), for any target platform."""
    nodes = {n['id']: n for n in md['resolve']['nodes']}
    root = md['resolve']['root']
    seen, todo = set(), [root]
    while todo:
        i = todo.pop()
        if i in seen:
            continue
        seen.add(i)
        for d in nodes[i]['deps']:
            if any(k.get('kind') in (None, 'build') for k in d['dep_kinds']):
                todo.append(d['pkg'])
    seen.discard(root)
    return sorted((p for p in md['packages'] if p['id'] in seen), key=lambda p: (p['name'], p['version']))


def license_texts(pkg):
    d = os.path.dirname(pkg['manifest_path'])
    files = sorted(f for f in os.listdir(d) if LICENSE_NAME.match(f) and os.path.isfile(os.path.join(d, f)))
    if pkg.get('license_file'):
        lf = os.path.normpath(os.path.join(d, pkg['license_file']))
        if os.path.isfile(lf) and os.path.basename(lf) not in files:
            files.append(os.path.relpath(lf, d))
    texts = []
    for f in files:
        with open(os.path.join(d, f), encoding='utf-8', errors='replace') as fh:
            t = fh.read().replace('\r\n', '\n').strip()
        if t:
            texts.append(t)
    return texts


def render(md):
    pkgs = build_packages(md)
    groups = {}   # license text -> crate names
    missing = []
    for p in pkgs:
        label = '%s %s' % (p['name'], p['version'])
        texts = license_texts(p)
        if not texts:
            missing.append((label, p.get('license') or 'unknown', p.get('repository') or ''))
            continue
        for t in texts:
            groups.setdefault(t, []).append(label)
    lines = [
        'THIRD-PARTY NOTICES FOR AMBERSIGHT',
        '',
        'AmberSight itself is licensed under the PolyForm Noncommercial License',
        '1.0.0 (see LICENSE). It is built from, and uses, the third-party software',
        'below, which keeps its own license. This file is generated by',
        'scripts/third-party-notices.py from the release build\'s dependencies.',
        '',
        '=' * 78,
        'RUNTIMES, MODELS AND TOOLS',
        '=' * 78,
        '',
        OTHER.rstrip(),
        '',
        '=' * 78,
        'RUST CRATES (%d, compiled into AmberSight)' % len(pkgs),
        '=' * 78,
        '',
    ]
    for p in pkgs:
        lines.append('%s %s  (%s)' % (p['name'], p['version'], p.get('license') or 'see license text'))
    if missing:
        lines += ['', 'These crates ship no license file in their package; their license (as', 'declared by the crate) and source are:', '']
        for label, lic, repo in missing:
            lines.append('  %s: %s%s' % (label, lic, ('  ' + repo) if repo else ''))
        lines += ['', 'The standard texts of those licenses follow (where a crate offers a choice,',
                  'AmberSight uses it under MIT, or Apache-2.0 when MIT is not offered).',
                  '', '-' * 78, 'MIT License (standard text)', '-' * 78, '', MIT_TEXT,
                  '', '-' * 78, 'zlib License (standard text)', '-' * 78, '', ZLIB_TEXT,
                  '', '-' * 78, 'Apache License 2.0 (standard text)', '-' * 78, '', apache_text()]
    lines += ['', '=' * 78, 'LICENSE TEXTS AND COPYRIGHT NOTICES', '=' * 78]
    for text, names in sorted(groups.items(), key=lambda kv: (kv[1][0], kv[0])):
        lines += ['', '-' * 78, 'Used by: ' + ', '.join(names), '-' * 78, '', text]
    return '\n'.join(lines).rstrip() + '\n'


def main():
    text = render(metadata())
    if '--check' in sys.argv:
        cur = open(OUT, encoding='utf-8').read() if os.path.exists(OUT) else ''
        if cur != text:
            sys.exit('THIRD-PARTY-NOTICES.txt is out of date: run python3 scripts/third-party-notices.py')
        print('THIRD-PARTY-NOTICES.txt is up to date')
        return
    with open(OUT, 'w', encoding='utf-8', newline='\n') as fh:
        fh.write(text)
    print('wrote', OUT, len(text), 'bytes')


if __name__ == '__main__':
    main()
