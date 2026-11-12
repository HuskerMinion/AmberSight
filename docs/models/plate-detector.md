# License plate detector (models/plate_yolox.onnx)

AmberSight's own plate finder, built into the program (no download). One class, "plate".
The plate *reader* (fast-plate-ocr, MIT) is a separate pinned download, see `src/lpr.rs`.

## What it is

- Network: YOLOX-Nano (depthwise), 416x416 input, 3.6 MB ONNX (opset 13, raw head output,
  decoded in `src/lpr.rs` with the same grid/stride code as `src/ai.rs`, then NMS 0.45).
- Training code: official YOLOX repository, https://github.com/Megvii-BaseDetection/YOLOX
  (commit 6ddff4824372906469a7fae2dc3206c7aa4bbaee), Apache-2.0.
- Starting weights: the official YOLOX-Nano COCO weights from that repository's release
  0.1.1rc0, Apache-2.0.
- Fine-tuned for 100 epochs (batch 32, mixed precision, mosaic augmentation, no mixup).
- The previous detector (a YOLOv9 network from open-image-models, GPL-3 upstream) is no longer used.

## Training data (all commercially usable with attribution)

Only Open Images V7, class "Vehicle registration plate" (`/m/01jfm_`), taken straight from the
official CSVs (https://storage.googleapis.com/openimages/) and image storage
(https://open-images-dataset.s3.amazonaws.com/), no accounts:

- Box annotations: Copyright Google LLC, license CC BY 4.0
  (https://creativecommons.org/licenses/by/4.0/).
- Images: CC BY 2.0 (https://creativecommons.org/licenses/by/2.0/), each by its Flickr author.
  Every image used was checked against the Open Images image metadata: all 8,150 carry the
  CC BY 2.0 license. Authors and source pages are in the Open Images metadata
  (`train-images-boxable-with-rotation.csv`, `validation-images-with-rotation.csv`,
  `test-images-with-rotation.csv`, columns Author / OriginalLandingURL), matched by ImageID.
- Only boxes with confidence 1 and not marked "group of" were used. Images with a rotation flag
  (7) were skipped.

| Set | Open Images split | Images | Plate boxes |
| --- | --- | --- | --- |
| train | train | 5,361 | 7,840 |
| held-out validation | validation + test | 2,789 | 3,812 |

No other data (no CCPD or Roboflow sets) was used. The validation images never took part in
training.

Attribution text for the product's license page:

> The license plate finder was trained on the Open Images Dataset V7 (annotations by Google LLC,
> CC BY 4.0; images CC BY 2.0 by their respective authors). Built with YOLOX by Megvii
> (Apache-2.0).

## Held-out results (same 2,789 images, same scoring code, CPU, 2 threads)

Scored with `scripts/plate-detector/eval_cmp.py` (AP with all-point interpolation; the YOLOX
networks see the whole photo letterboxed to 416, the YOLOv9 to 384). Plates are small in these
photos; AmberSight normally runs on vehicle crops where they are larger.

| Detector | Size | AP@0.5 | AP@0.5:0.95 | Recall / precision at confidence 0.4 | ms per photo (2 threads) |
| --- | --- | --- | --- | --- | --- |
| previous YOLOv9-t 384 | 7.7 MB | 0.840 | 0.628 | 0.779 / 0.969 | 15.7 |
| **YOLOX-Nano 416 (shipped)** | 3.6 MB | 0.841 | 0.531 | 0.791 / 0.935 | 6.9 |
| YOLOX-Tiny 416 | 20 MB | 0.863 | 0.568 | 0.822 / 0.940 | 24.0 |

The shipped model finds as many plates as the old one at the same confidence (slightly more)
with a few more false alarms, and its boxes are a little looser (lower AP@0.5:0.95), which does
not matter because the reader gets the box with a margin. The old detector may have seen some
of the same Open Images photos in its training, so its numbers could be flattering.
YOLOX-Tiny is more accurate but 20 MB; it is a drop-in replacement if size does not matter
(same code, change the file and the input size is read from the model).

## Rebuilding

See `scripts/plate-detector/`: `prep_select.py` (choose the images from the Open Images CSVs),
`prep_dl.py` (download them), `build_coco.py` (COCO-format train/val), `exp_plate.py` +
`train.sh` (YOLOX experiment, `PLATE_ARCH=nano PLATE_SIZE=416`), `export.py` (ONNX),
`eval_cmp.py` (comparison). Needs the YOLOX repository checked out at the commit above and a
CUDA PyTorch; training took about 40 minutes on one GPU.
