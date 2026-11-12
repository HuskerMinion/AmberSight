cd ~/plate-train/YOLOX
export PYTHONPATH=$HOME/plate-train/YOLOX
export PLATE_ARCH=$1 PLATE_SIZE=$2 PLATE_EPOCHS=${4:-60}
export CUDA_VISIBLE_DEVICES=$3
mkdir -p ~/plate-train/out
nohup ../venv/bin/python -I tools/train.py -f ../exp_plate.py -d 1 -b 32 --fp16 -c ../weights/yolox_$1.pth --exp_name plate_$1_$2 -o > ../train_$1_$2.log 2>&1 &
