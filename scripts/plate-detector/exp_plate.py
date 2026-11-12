import os
BASE = os.path.expanduser(os.environ.get('PLATE_BASE', '~/plate-train')) + '/'
import os
from yolox.exp import Exp as MyExp


class Exp(MyExp):
    def __init__(self):
        super().__init__()
        arch = os.environ.get("PLATE_ARCH", "tiny")
        size = int(os.environ.get("PLATE_SIZE", "416"))
        self.depth, self.width = {"tiny": (0.33, 0.375), "nano": (0.33, 0.25)}[arch]
        self.num_classes = 1
        self.data_dir = BASE + "data"
        self.train_ann = "train.json"
        self.val_ann = "val.json"
        self.input_size = (size, size)
        self.test_size = (size, size)
        self.random_size = (size // 32 - 3, size // 32 + 3)
        self.max_epoch = int(os.environ.get("PLATE_EPOCHS", "60"))
        self.no_aug_epochs = 10
        self.warmup_epochs = 3
        self.basic_lr_per_img = 0.01 / 64.0
        self.data_num_workers = 10
        self.eval_interval = 5
        self.enable_mixup = False
        self.mosaic_scale = (0.5, 1.5)
        self.test_conf = 0.01
        self.nmsthre = 0.5
        if arch == "nano":
            self.act = "silu"
        self.exp_name = f"plate_{arch}_{size}"
        # no horizontal flip issue for plates text? keep default flip (detection only)

    def get_model(self):
        from yolox.models import YOLOX, YOLOPAFPN, YOLOXHead

        def init_yolo(M):
            for m in M.modules():
                if isinstance(m, __import__("torch").nn.BatchNorm2d):
                    m.eps = 1e-3
                    m.momentum = 0.03

        if getattr(self, "model", None) is None:
            in_channels = [256, 512, 1024]
            depthwise = self.exp_name.startswith("plate_nano")
            backbone = YOLOPAFPN(self.depth, self.width, in_channels=in_channels, act=self.act, depthwise=depthwise)
            head = YOLOXHead(self.num_classes, self.width, in_channels=in_channels, act=self.act, depthwise=depthwise)
            self.model = YOLOX(backbone, head)
        self.model.apply(init_yolo)
        self.model.head.initialize_biases(1e-2)
        self.model.train()
        return self.model

    def get_dataset(self, cache=False, cache_type="ram"):
        from yolox.data import COCODataset, TrainTransform
        return COCODataset(data_dir=self.data_dir, json_file=self.train_ann, name="train", img_size=self.input_size,
                           preproc=TrainTransform(max_labels=50, flip_prob=self.flip_prob, hsv_prob=self.hsv_prob),
                           cache=cache, cache_type=cache_type)

    def get_eval_dataset(self, **kwargs):
        from yolox.data import COCODataset, ValTransform
        return COCODataset(data_dir=self.data_dir, json_file=self.val_ann, name="val", img_size=self.test_size,
                           preproc=ValTransform(legacy=kwargs.get("legacy", False)))
