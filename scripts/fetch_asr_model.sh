#!/usr/bin/env bash
#
# Fetch the SenseVoice ONNX bundle the "sherpa" ASR backend loads at runtime.
#
# One-time, on whichever host holds the deploy tree: the bundle lands in
# models/asr/sense-voice/, which docker-compose mounts read-only at
# /app/models. It is neither in git nor in the deploy payload - a quarter-GB
# binary that never changes has no business travelling with every deploy.
#
# HF_ENDPOINT overrides the download host (default: the hf-mirror proxy, which
# is reachable where huggingface.co is not). If neither works from the server,
# run this on the workstation and copy the directory over:
#   scp -r models <deploy-host>:/opt/docker/qbot/

set -euo pipefail

ENDPOINT=${HF_ENDPOINT:-https://hf-mirror.com}
REPO=csukuangfj/sherpa-onnx-sense-voice-zh-en-ja-ko-yue-2024-07-17
DEST="$(dirname "$0")/../models/asr/sense-voice"

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

ls -l "$DEST"
