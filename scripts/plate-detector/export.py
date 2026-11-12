import os
BASE = os.path.expanduser(os.environ.get('PLATE_BASE', '~/plate-train')) + '/'
import sys, torch
sys.path.insert(0, BASE); sys.path.insert(0, BASE + 'YOLOX')
from exp_plate import Exp
ckpt, out = sys.argv[1], sys.argv[2]
exp = Exp(); model = exp.get_model()
model.load_state_dict(torch.load(ckpt, map_location='cpu', weights_only=False)['model'])
model.eval(); model.head.decode_in_inference = False
s = exp.test_size[0]
torch.onnx.export(model, torch.zeros(1, 3, s, s), out, input_names=['images'], output_names=['output'], opset_version=13, dynamo=False)
print('ok', out)
