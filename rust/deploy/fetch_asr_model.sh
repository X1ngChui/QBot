#!/usr/bin/env bash
#
# Fetch the SenseVoice ONNX bundle the in-process speech recognizer loads (about 230 MB, once).
#
# Run it on the host that holds the deploy tree. The bundle lands in the persistent data volume,
# where the container reads it as /var/lib/qbot/models/asr/sense-voice. It is neither in Git nor
# in the image: a large binary that never changes has no business travelling with every build.
#
#   deploy/fetch_asr_model.sh
#
# HF_ENDPOINT overrides the download host (default https://huggingface.co; a mirror such as
# https://hf-mirror.com works where the default is not reachable). If neither is reachable from
# the server, run this on a workstation and copy data/qbot/models over.

set -euo pipefail

ENDPOINT=${HF_ENDPOINT:-https://huggingface.co}
REPO=csukuangfj/sherpa-onnx-sense-voice-zh-en-ja-ko-yue-2024-07-17
DEST="$(dirname "$0")/data/qbot/models/asr/sense-voice"

mkdir -p "$DEST"
for f in model.int8.onnx tokens.txt; do
    if [ -s "$DEST/$f" ]; then
        echo "already present: $DEST/$f"
        continue
    fi
    echo "==> $f"
    curl -fL --retry 3 -o "$DEST/$f.part" "$ENDPOINT/$REPO/resolve/main/$f"
    mv "$DEST/$f.part" "$DEST/$f"
done

# The container runs as uid 10001 and only reads the model.
chmod -R a+rX "$DEST"
ls -l "$DEST"
