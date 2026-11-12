import os
BASE = os.path.expanduser(os.environ.get('PLATE_BASE', '~/plate-train')) + '/'
"""Compare plate detectors on the held-out images. usage: eval_cmp.py model.onnx v9|yolox size val_dir val_json [max_images] [threads]"""
import sys, json, time
import numpy as np, cv2, onnxruntime as ort

path, kind, size, vdir, vjson = sys.argv[1], sys.argv[2], int(sys.argv[3]), sys.argv[4], sys.argv[5]
maxn = int(sys.argv[6]) if len(sys.argv) > 6 else 10**9
threads = int(sys.argv[7]) if len(sys.argv) > 7 else 2
so = ort.SessionOptions(); so.intra_op_num_threads = threads; so.inter_op_num_threads = 1
sess = ort.InferenceSession(path, so, providers=['CPUExecutionProvider'])
inp = sess.get_inputs()[0].name
J = json.load(open(vjson))
gt = {}
for a in J['annotations']:
    x, y, w, h = a['bbox']; gt.setdefault(a['image_id'], []).append([x, y, x + w, y + h])
imgs = J['images'][:maxn]

def grid(n_size):
    g = []
    for st in (8, 16, 32):
        n = n_size // st
        for gy in range(n):
            for gx in range(n):
                g.append((gx, gy, st))
    return np.array(g, dtype=np.float32)

G = grid(size) if kind == 'yolox' else None

def nms(b, s, thr=0.45):
    order = np.argsort(-s); keep = []
    while len(order):
        i = order[0]; keep.append(i)
        if len(order) == 1: break
        r = order[1:]
        xx0 = np.maximum(b[i, 0], b[r, 0]); yy0 = np.maximum(b[i, 1], b[r, 1])
        xx1 = np.minimum(b[i, 2], b[r, 2]); yy1 = np.minimum(b[i, 3], b[r, 3])
        inter = np.clip(xx1 - xx0, 0, None) * np.clip(yy1 - yy0, 0, None)
        ai = (b[i, 2] - b[i, 0]) * (b[i, 3] - b[i, 1]); ar = (b[r, 2] - b[r, 0]) * (b[r, 3] - b[r, 1])
        order = r[inter / (ai + ar - inter + 1e-9) <= thr]
    return keep

def predict(img):
    ih, iw = img.shape[:2]
    r = min(size / iw, size / ih)
    nw, nh = max(1, round(iw * r)), max(1, round(ih * r))
    rs = cv2.resize(img, (nw, nh), interpolation=cv2.INTER_LINEAR)
    if kind == 'yolox':
        canvas = np.full((size, size, 3), 114, np.float32)
        canvas[:nh, :nw] = rs  # BGR 0-255
        x = canvas.transpose(2, 0, 1)[None]
        out = sess.run(None, {inp: x})[0][0]
        sc = out[:, 4] * out[:, 5]
        m = sc > 0.01
        o = out[m]; sc = sc[m]; g = G[m]
        cx = (o[:, 0] + g[:, 0]) * g[:, 2]; cy = (o[:, 1] + g[:, 1]) * g[:, 2]
        w = np.exp(o[:, 2]) * g[:, 2]; h = np.exp(o[:, 3]) * g[:, 2]
        b = np.stack([cx - w / 2, cy - h / 2, cx + w / 2, cy + h / 2], 1) / r
        k = nms(b, sc) if len(sc) else []
        return b[k], sc[k]
    else:
        canvas = np.full((size, size, 3), 114, np.uint8)
        dw, dh = round((size - nw) / 2), round((size - nh) / 2)
        canvas[dh:dh + nh, dw:dw + nw] = rs
        x = (canvas[:, :, ::-1].transpose(2, 0, 1)[None].astype(np.float32) / 255.0)
        out = sess.run(None, {inp: x})[0]
        out = out.reshape(-1, 7)
        b = np.stack([(out[:, 1] - dw) / r, (out[:, 2] - dh) / r, (out[:, 3] - dw) / r, (out[:, 4] - dh) / r], 1)
        return b, out[:, 6]

def iou_m(a, b):
    xx0 = np.maximum(a[:, None, 0], b[None, :, 0]); yy0 = np.maximum(a[:, None, 1], b[None, :, 1])
    xx1 = np.minimum(a[:, None, 2], b[None, :, 2]); yy1 = np.minimum(a[:, None, 3], b[None, :, 3])
    inter = np.clip(xx1 - xx0, 0, None) * np.clip(yy1 - yy0, 0, None)
    aa = (a[:, 2] - a[:, 0]) * (a[:, 3] - a[:, 1]); bb = (b[:, 2] - b[:, 0]) * (b[:, 3] - b[:, 1])
    return inter / (aa[:, None] + bb[None, :] - inter + 1e-9)

preds = []; times = []
for n, im in enumerate(imgs):
    img = cv2.imread(f"{vdir}/{im['file_name']}")
    if n == 0: predict(img)
    t = time.perf_counter(); b, s = predict(img); times.append(time.perf_counter() - t)
    preds.append((np.array(gt.get(im['id'], []), dtype=np.float32).reshape(-1, 4), b, s))
ngt = sum(len(p[0]) for p in preds)

def ap_at(thr):
    recs = []
    for gi, (g, b, s) in enumerate(preds):
        used = set()
        if len(b) == 0: continue
        ious = iou_m(b, g) if len(g) else np.zeros((len(b), 0))
        for i in np.argsort(-s):
            tp = 0
            if ious.shape[1]:
                j = int(np.argmax(ious[i]))
                if ious[i, j] >= thr and j not in used: used.add(j); tp = 1
            recs.append((s[i], tp))
    recs.sort(key=lambda t: -t[0])
    tp = np.cumsum([r[1] for r in recs]); fp = np.cumsum([1 - r[1] for r in recs])
    rec = tp / max(ngt, 1); prec = tp / np.maximum(tp + fp, 1)
    mrec = np.concatenate([[0], rec, [1]]); mpre = np.concatenate([[0], prec, [0]])
    for i in range(len(mpre) - 2, -1, -1): mpre[i] = max(mpre[i], mpre[i + 1])
    idx = np.where(mrec[1:] != mrec[:-1])[0]
    return float(np.sum((mrec[idx + 1] - mrec[idx]) * mpre[idx + 1]))

def at_conf(c, thr=0.5):
    tp = fp = 0
    for g, b, s in preds:
        k = s >= c
        bb = b[k]; ss = s[k]
        used = set()
        ious = iou_m(bb, g) if len(g) and len(bb) else np.zeros((len(bb), 0))
        for i in np.argsort(-ss):
            ok = False
            if ious.shape[1]:
                j = int(np.argmax(ious[i]))
                if ious[i, j] >= thr and j not in used: used.add(j); ok = True
            tp += ok; fp += (not ok)
    return tp / max(ngt, 1), tp / max(tp + fp, 1)

ap50 = ap_at(0.5)
ap = np.mean([ap_at(t) for t in np.arange(0.5, 0.96, 0.05)])
print(f"{path} images={len(imgs)} plates={ngt} AP50={ap50:.3f} AP50:95={ap:.3f} ms/img={np.mean(times)*1000:.1f} (threads={threads})")
for c in (0.25, 0.4, 0.5):
    r, p = at_conf(c)
    print(f"  conf>={c}: recall={r:.3f} precision={p:.3f}")
