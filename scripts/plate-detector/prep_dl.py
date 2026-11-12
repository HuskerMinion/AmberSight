import os
BASE = os.path.expanduser(os.environ.get('PLATE_BASE', '~/plate-train')) + '/'
import json, os, concurrent.futures as cf, urllib.request
S = json.load(open(BASE + 'selection.json'))
out = BASE + 'raw/img/'
os.makedirs(out, exist_ok=True)
jobs = []
for split, boxes in S['boxes'].items():
    for i in boxes:
        if float(S['meta'][i]['Rotation'] or 0) == 0:
            jobs.append((split, i))
print(len(jobs), 'of', len(S['meta']))
def get(j):
    split, i = j
    p = out + i + '.jpg'
    if os.path.exists(p): return 1
    try:
        d = urllib.request.urlopen(f'https://open-images-dataset.s3.amazonaws.com/{split}/{i}.jpg', timeout=30).read()
        open(p, 'wb').write(d); return 1
    except Exception as e:
        return 0
with cf.ThreadPoolExecutor(4) as ex:
    r = list(ex.map(get, jobs))
print('ok', sum(r), 'failed', len(r) - sum(r))
