import os
BASE = os.path.expanduser(os.environ.get('PLATE_BASE', '~/plate-train')) + '/'
import csv, collections, json
R = BASE + 'raw/'
MID = '/m/01jfm_'
sel = {}
for split, f in [('train', 'oidv6-train-annotations-bbox.csv'), ('validation', 'validation-annotations-bbox.csv'), ('test', 'test-annotations-bbox.csv')]:
    boxes = collections.defaultdict(list)
    for r in csv.DictReader(open(R + f)):
        if r['LabelName'] != MID or r['Confidence'] != '1' or r['IsGroupOf'] == '1':
            continue
        boxes[r['ImageID']].append([float(r[k]) for k in ('XMin', 'XMax', 'YMin', 'YMax')])
    sel[split] = boxes
    print(split, len(boxes), sum(len(v) for v in boxes.values()))
meta = {}
for split, f in [('train', 'train-images-boxable-with-rotation.csv'), ('validation', 'validation-images-with-rotation.csv'), ('test', 'test-images-with-rotation.csv')]:
    for r in csv.DictReader(open(R + f)):
        if r['ImageID'] in sel[split]:
            meta[r['ImageID']] = {k: r[k] for k in ('Subset', 'License', 'Author', 'OriginalLandingURL', 'Rotation')}
lic = collections.Counter(m['License'] for m in meta.values())
print(lic)
json.dump({'boxes': sel, 'meta': meta}, open(BASE + 'selection.json', 'w'))
