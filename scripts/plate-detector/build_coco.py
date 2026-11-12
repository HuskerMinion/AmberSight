import os
BASE = os.path.expanduser(os.environ.get('PLATE_BASE', '~/plate-train')) + '/'
import json, os
from PIL import Image
B = BASE
S = json.load(open(B + 'selection.json'))
os.makedirs(B + 'data/annotations', exist_ok=True)
os.makedirs(B + 'data/train', exist_ok=True)
os.makedirs(B + 'data/val', exist_ok=True)
sets = {'train': ['train'], 'val': ['validation', 'test']}
for name, splits in sets.items():
    images, anns = [], []
    for sp in splits:
        for iid, boxes in S['boxes'][sp].items():
            p = B + 'raw/img/' + iid + '.jpg'
            if not os.path.exists(p):
                continue
            try:
                w, h = Image.open(p).size
            except Exception:
                continue
            d = B + f'data/{name}/' + iid + '.jpg'
            if not os.path.exists(d):
                os.link(p, d)
            n = len(images) + 1
            images.append({'id': n, 'file_name': iid + '.jpg', 'width': w, 'height': h})
            for x0, x1, y0, y1 in boxes:
                bw, bh = (x1 - x0) * w, (y1 - y0) * h
                if bw < 2 or bh < 2:
                    continue
                anns.append({'id': len(anns) + 1, 'image_id': n, 'category_id': 1, 'bbox': [x0 * w, y0 * h, bw, bh], 'area': bw * bh, 'iscrowd': 0})
    json.dump({'images': images, 'annotations': anns, 'categories': [{'id': 1, 'name': 'plate'}]}, open(B + f'data/annotations/{name}.json', 'w'))
    print(name, len(images), len(anns))
