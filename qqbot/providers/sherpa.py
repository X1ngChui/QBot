"""In-process speech recognition via sherpa-onnx: the "sherpa" ASR backend.

SenseVoice-class models are small non-autoregressive encoders - a ten-second clip
decodes in well under a second on two CPU threads - so speech-to-text is the one
capability cheap enough to bring in-process: no endpoint, no key, no per-second
bill. Like the local text backend, every rate is zero and the ledger still counts
the calls, so /stats shows what ran at a cost of nothing.

The model is not baked into the image: the ONNX bundle lives under models/ on the
host (scripts/fetch_asr_model.sh downloads it once), mounted read-only, and
cfg.model_dir names it. Loading is lazy and cached per (dir, threads), so
/reload picks up a changed path the way the HTTP backends pick up a changed
endpoint.

Input is the 16 kHz mono WAV that media.py already fetches from NapCat; the
recognizer resamples internally, so the header's declared rate is passed through
rather than assumed.
"""

from __future__ import annotations

import asyncio
import io
import logging
import wave
from pathlib import Path
from typing import Any

from ..core.budget import BUDGET
from ..settings import AsrCfg
from .base import AsrModel, Kind, Rate

log = logging.getLogger("qqbot.providers")

_FREE = Rate("second", per_unit=0.0, source="self-hosted: watts, not CNY")


def _pcm_from_wav(data: bytes) -> tuple[list[float], int]:
    """Decode a WAV container to mono float32 samples in [-1, 1] plus its rate.

    Stdlib only: sherpa-onnx wants raw samples, and pulling in numpy/soundfile
    for one 16-bit PCM parse would be the heaviest dependency in the file.
    Multi-channel audio is averaged down; NapCat always sends mono anyway.
    """
    with wave.open(io.BytesIO(data)) as w:
        n_ch, width, rate = w.getnchannels(), w.getsampwidth(), w.getframerate()
        if width != 2:
            raise ValueError(f"expected 16-bit PCM, got sample width {width}")
        raw = w.readframes(w.getnframes())
    ints = memoryview(raw).cast("h")
    if n_ch > 1:
        ints = [sum(ints[i:i + n_ch]) // n_ch for i in range(0, len(ints), n_ch)]
    return [s / 32768.0 for s in ints], rate


class SherpaAsr(AsrModel):
    name = "sherpa"

    def __init__(self) -> None:
        self._recognizer: Any = None
        self._built_for: tuple[str, int] | None = None
        # One decode at a time: the recognizer's thread safety is not documented,
        # and clips are rare enough that serialising costs nothing.
        self._lock = asyncio.Lock()

    async def aclose(self) -> None:
        self._recognizer = None
        self._built_for = None

    def rate_for(self, model: str) -> Rate:
        return _FREE

    def _get(self, cfg: AsrCfg) -> Any:
        key = (cfg.model_dir, cfg.threads)
        if self._recognizer is None or self._built_for != key:
            import sherpa_onnx  # deferred: only this backend needs the wheel

            d = Path(cfg.model_dir)
            model = d / "model.int8.onnx"
            tokens = d / "tokens.txt"
            if not model.is_file() or not tokens.is_file():
                raise FileNotFoundError(
                    f"ASR model bundle incomplete under {d}: expected "
                    "model.int8.onnx and tokens.txt - run scripts/fetch_asr_model.sh"
                )
            # use_itn keeps punctuation and written-form numbers, matching what
            # the API backends were asked for (enable_itn) - the archive should
            # not change dialect because the backend moved in-process.
            self._recognizer = sherpa_onnx.OfflineRecognizer.from_sense_voice(
                model=str(model), tokens=str(tokens),
                num_threads=cfg.threads, use_itn=True, language="auto",
            )
            self._built_for = key
            log.info("sherpa ASR loaded: %s (%d threads)", model, cfg.threads)
        return self._recognizer

    def _decode(self, rec: Any, samples: list[float], rate: int) -> str:
        stream = rec.create_stream()
        stream.accept_waveform(rate, samples)
        rec.decode_stream(stream)
        return stream.result.text.strip()

    async def transcribe(
        self, data: bytes, *, cfg: AsrCfg, fmt: str = "wav",
        seconds: float | None = None, group_id: str | None = None,
    ) -> str:
        if fmt != "wav":
            # media.py always transcodes through NapCat first; anything else is
            # a caller bug, and decoding compressed audio is out of scope here.
            raise ValueError(f"sherpa backend takes WAV only, got {fmt!r}")
        samples, rate = _pcm_from_wav(data)
        async with self._lock:
            rec = self._get(cfg)
            # Model load above stays on the loop (once, ~a second); decoding is
            # the recurring cost and runs off-loop so a long clip cannot stall
            # message handling.
            text = await asyncio.to_thread(self._decode, rec, samples, rate)
        secs = seconds if seconds else len(samples) / max(rate, 1)
        await BUDGET.record(kind=Kind.ASR, model=cfg.model or "sense-voice",
                            cny=_FREE.units(secs), group_id=group_id)
        return text
