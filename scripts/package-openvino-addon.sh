#!/usr/bin/env sh
# Build the Intel graphics add-on for the AI on 64-bit Linux PCs:
#   dist/ambersight-openvino-<version>-linux-x86_64.tar.gz
# holding ONNX Runtime's OpenVINO provider (the same ONNX Runtime 1.31 the
# program has built in, from pyke's build of it) and the parts of Intel's
# OpenVINO 2026.4.1 it needs to run a model on Intel graphics:
#   libonnxruntime_providers_shared.so  libonnxruntime_providers_openvino.so
#   libopenvino.so.2641  libopenvino_c.so.2641  libopenvino_onnx_frontend.so.2641
#   libopenvino_intel_gpu_plugin.so  libtbb.so.12
# plus OpenVINO's license and third-party notices. Packages leave it out;
# AmberSight downloads this file from the release when it finds Intel
# graphics with Intel's compute driver, checked against the signed
# SHA256SUMS (src/gpu_addon.rs, src/openvino.rs).
#
# The two downloads below are pinned by SHA-256 and kept in
# $OPENVINO_DOWNLOADS (default target/openvino-downloads), so a second run
# needs no internet. Needs curl, xz, tar and python3.
set -eu
cd "$(dirname "$0")/.."
version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
name="ambersight-openvino-$version-linux-x86_64"

# ONNX Runtime 1.31.0 with its OpenVINO provider, as ort-sys 2.0.0-rc.15
# lists it (its build/download/dist.tsv): a tar packed as raw LZMA2.
ORT_URL="https://cdn.pyke.io/0/pyke:ort-rs/ms@1.31.0/x86_64-unknown-linux-gnu+openvino.tar.lzma2"
ORT_SHA=606d54fca76cbc77c0a1518c34691f8f00b7983c20775b5dca17bad68bdba990
# OpenVINO 2026.4.1 for Linux x86_64 (glibc 2.28 or newer), from PyPI; the
# libraries in it do not depend on Python.
OV_URL="https://files.pythonhosted.org/packages/ec/3a/2a173ac1ad749ff0b041788eefc1ade0d410231fedfc43f77474f3b806cc/openvino-2026.4.1-22982-cp312-cp312-manylinux_2_28_x86_64.whl"
OV_SHA=6b4375c17ddcac83a5180349e2e2bb811185c261066e2a920659892d58ef0e3b

dl=${OPENVINO_DOWNLOADS:-target/openvino-downloads}
mkdir -p "$dl"
fetch() { # url sha file
  if [ -f "$dl/$3" ] && echo "$2  $dl/$3" | sha256sum -c - >/dev/null 2>&1; then
    return
  fi
  echo "downloading $3"
  curl -fsSL --retry 3 -o "$dl/$3.part" "$1"
  echo "$2  $dl/$3.part" | sha256sum -c - >/dev/null || { echo "$3: SHA-256 does not match" >&2; rm -f "$dl/$3.part"; exit 1; }
  mv "$dl/$3.part" "$dl/$3"
}
fetch "$ORT_URL" "$ORT_SHA" ort-openvino.tar.lzma2
fetch "$OV_URL" "$OV_SHA" openvino.whl

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
out="$work/$name"
mkdir -p "$out/licenses" "$work/ort"
# (A raw LZMA2 stream has no header: xz is told its format.)
xz -dc --format=raw --lzma2=dict=256MiB "$dl/ort-openvino.tar.lzma2" | tar -x -C "$work/ort" libonnxruntime_providers_shared.so libonnxruntime_providers_openvino.so
cp "$work/ort/libonnxruntime_providers_shared.so" "$work/ort/libonnxruntime_providers_openvino.so" "$out/"
python3 -I - "$dl/openvino.whl" "$out" <<'PY'
import os, sys, zipfile
whl, out = sys.argv[1], sys.argv[2]
libs = ["libopenvino.so.2641", "libopenvino_c.so.2641", "libopenvino_onnx_frontend.so.2641",
        "libopenvino_intel_gpu_plugin.so", "libtbb.so.12"]
z = zipfile.ZipFile(whl)
names = z.namelist()
for l in libs:
    src = "openvino/libs/" + l
    if src not in names:
        sys.exit("the OpenVINO package has no " + src)
    with open(os.path.join(out, l), "wb") as f:
        f.write(z.read(src))
lic = "openvino-2026.4.1.dist-info/licenses/"
for n in names:
    if n.startswith(lic) and not n.endswith("/"):
        dest = os.path.join(out, "licenses", "openvino-" + os.path.basename(n))
        with open(dest, "wb") as f:
            f.write(z.read(n))
PY
chmod 644 "$out"/*.so* "$out"/licenses/*
[ -f THIRD-PARTY-NOTICES.txt ] && cp THIRD-PARTY-NOTICES.txt "$out/"
cat > "$out/README.txt" <<TXT
AmberSight $version: Intel graphics add-on for the AI (64-bit Linux)

ONNX Runtime's OpenVINO provider (ONNX Runtime 1.31.0) and Intel's OpenVINO
2026.4.1 runtime with its GPU plugin, for AmberSight $version (this version
only). AmberSight downloads this file by itself when it finds Intel graphics
and Intel's compute driver, and keeps the seven library files in
<data folder>/tools/openvino-$version/. To install it by hand (a computer
without internet), put the seven .so files there.

The card also needs Intel's compute driver: intel-opencl-icd (Debian,
Ubuntu) or intel-compute-runtime (Fedora). See the install guide.

OpenVINO and oneTBB: Copyright Intel Corporation, Apache License 2.0
(licenses/openvino-LICENSE); their third-party notices are in licenses/.
ONNX Runtime: Copyright Microsoft Corporation, MIT License.
TXT
for f in libonnxruntime_providers_shared.so libonnxruntime_providers_openvino.so libopenvino.so.2641 \
         libopenvino_c.so.2641 libopenvino_onnx_frontend.so.2641 libopenvino_intel_gpu_plugin.so libtbb.so.12; do
  [ -s "$out/$f" ] || { echo "$f is missing from the add-on" >&2; exit 1; }
done
mkdir -p dist
if tar --version 2>/dev/null | grep -q GNU; then
  tar -C "$work" --owner=0 --group=0 --numeric-owner --mode=u+rwX,go+rX,go-w --sort=name -czf "dist/$name.tar.gz" "$name"
else
  tar -C "$work" --uid 0 --gid 0 --uname root --gname wheel -czf "dist/$name.tar.gz" "$name"
fi
echo "dist/$name.tar.gz"
