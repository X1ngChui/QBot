"""Fixed in-process SenseVoice speech recognition on one CPU worker."""

from __future__ import annotations

import asyncio
import io
import logging
import wave
from concurrent.futures import ThreadPoolExecutor
from dataclasses import dataclass
from enum import StrEnum
from pathlib import Path
from typing import Any

from ..core.budget import BUDGET
from ..settings import AsrCfg
from .base import AsrModel, Kind, Rate

log = logging.getLogger("qqbot.providers")

_FREE = Rate("second", per_unit=0.0, source="self-hosted: watts, not CNY")
_MODEL = "sense-voice"


class AsrBusy(RuntimeError):
    """The bounded recognizer queue has no waiting slot."""


class AsrUnavailable(RuntimeError):
    """The recognizer is not ready or is shutting down."""


class _State(StrEnum):
    NEW = "new"
    READY = "ready"
    CLOSING = "closing"
    CLOSED = "closed"


@dataclass(frozen=True, slots=True)
class _Job:
    data: bytes
    seconds: float | None
    group_id: str | None
    result: asyncio.Future[str]


def _pcm_from_wav(data: bytes) -> tuple[Any, int]:
    """Decode 16-bit WAV bytes to mono float samples and their sample rate."""

    with wave.open(io.BytesIO(data)) as wav:
        channels = wav.getnchannels()
        width = wav.getsampwidth()
        rate = wav.getframerate()
        if width != 2:
            raise ValueError(f"expected 16-bit PCM, got sample width {width}")
        raw = wav.readframes(wav.getnframes())
    frame = 2 * channels
    raw = raw[: len(raw) // frame * frame]
    try:
        import numpy as np
    except ImportError:
        ints = memoryview(raw).cast("h")
        if channels > 1:
            ints = [
                sum(ints[index:index + channels]) // channels
                for index in range(0, len(ints), channels)
            ]
        return [sample / 32768.0 for sample in ints], rate
    pcm = np.frombuffer(raw, np.int16).reshape(-1, channels)
    mono = pcm.mean(axis=1) if channels > 1 else pcm[:, 0]
    return mono.astype(np.float32) / 32768.0, rate


class SherpaAsr(AsrModel):
    """One recognizer, one native worker and a bounded FIFO of waiting clips."""

    name = "sherpa"
    needs_key = False

    def __init__(self) -> None:
        self._state = _State.NEW
        self._recognizer: Any = None
        self._executor: ThreadPoolExecutor | None = None
        self._queue: asyncio.Queue[_Job] | None = None
        self._worker: asyncio.Task[None] | None = None

    def rate_for(self, model: str) -> Rate:
        return _FREE

    @staticmethod
    def _build(cfg: AsrCfg) -> Any:
        import sherpa_onnx

        directory = Path(cfg.model_dir)
        model = directory / "model.int8.onnx"
        tokens = directory / "tokens.txt"
        if not model.is_file() or not tokens.is_file():
            raise FileNotFoundError(
                f"ASR model bundle incomplete under {directory}: expected "
                "model.int8.onnx and tokens.txt - run scripts/fetch_asr_model.sh"
            )
        recognizer = sherpa_onnx.OfflineRecognizer.from_sense_voice(
            model=str(model),
            tokens=str(tokens),
            num_threads=cfg.threads,
            use_itn=True,
            language="auto",
        )
        # Force model initialization before readiness is announced. A zero-valued
        # tenth of a second is long enough to exercise stream creation and decode.
        stream = recognizer.create_stream()
        stream.accept_waveform(16_000, [0.0] * 1_600)
        recognizer.decode_stream(stream)
        _ = stream.result.text
        return recognizer

    async def start(self, cfg: AsrCfg) -> None:
        if self._state is not _State.NEW:
            raise RuntimeError(f"ASR cannot start from {self._state}")
        self._executor = ThreadPoolExecutor(max_workers=1, thread_name_prefix="qbot-asr")
        loop = asyncio.get_running_loop()
        try:
            self._recognizer = await loop.run_in_executor(self._executor, self._build, cfg)
        except BaseException:
            self._executor.shutdown(wait=False, cancel_futures=True)
            self._executor = None
            self._state = _State.CLOSED
            raise
        self._queue = asyncio.Queue(maxsize=cfg.queue_capacity)
        self._state = _State.READY
        self._worker = asyncio.create_task(self._serve(), name="qbot-asr-worker")
        log.info(
            "sherpa ASR ready: %s (%d threads, %d queued)",
            cfg.model_dir,
            cfg.threads,
            cfg.queue_capacity,
        )

    @staticmethod
    def _decode(recognizer: Any, data: bytes) -> tuple[str, float]:
        samples, rate = _pcm_from_wav(data)
        stream = recognizer.create_stream()
        stream.accept_waveform(rate, samples)
        recognizer.decode_stream(stream)
        return stream.result.text.strip(), len(samples) / max(rate, 1)

    async def _serve(self) -> None:
        assert self._queue is not None
        assert self._executor is not None
        loop = asyncio.get_running_loop()
        while True:
            job = await self._queue.get()
            try:
                text, measured_seconds = await loop.run_in_executor(
                    self._executor,
                    self._decode,
                    self._recognizer,
                    job.data,
                )
                seconds = job.seconds if job.seconds is not None else measured_seconds
                await BUDGET.record(
                    kind=Kind.ASR,
                    model=_MODEL,
                    cny=0.0,
                    group_id=job.group_id,
                )
                if not job.result.done():
                    job.result.set_result(text)
                log.debug("local ASR decoded %.2fs of audio", seconds)
            except asyncio.CancelledError:
                if not job.result.done():
                    job.result.set_exception(AsrUnavailable("ASR shut down during decode"))
                raise
            except BaseException as exc:
                if not job.result.done():
                    job.result.set_exception(exc)
            finally:
                self._queue.task_done()

    async def transcribe(
        self,
        data: bytes,
        *,
        cfg: AsrCfg,
        fmt: str = "wav",
        seconds: float | None = None,
        group_id: str | None = None,
    ) -> str:
        del cfg
        if fmt != "wav":
            raise ValueError(f"sherpa backend takes WAV only, got {fmt!r}")
        if self._state is not _State.READY or self._queue is None:
            raise AsrUnavailable(f"ASR is {self._state}")
        if self._queue.full():
            raise AsrBusy("ASR queue is full")
        result = asyncio.get_running_loop().create_future()
        self._queue.put_nowait(_Job(data, seconds, group_id, result))
        # Cancelling a caller does not cancel a native decode already in progress.
        return await asyncio.shield(result)

    async def aclose(self) -> None:
        if self._state is _State.CLOSED:
            return
        if self._state is _State.NEW:
            self._state = _State.CLOSED
            return
        self._state = _State.CLOSING
        if self._queue is not None:
            while True:
                try:
                    job = self._queue.get_nowait()
                except asyncio.QueueEmpty:
                    break
                if not job.result.done():
                    job.result.set_exception(AsrUnavailable("ASR shut down before decode"))
                self._queue.task_done()
        if self._worker is not None:
            self._worker.cancel()
            await asyncio.gather(self._worker, return_exceptions=True)
            self._worker = None
        if self._executor is not None:
            self._executor.shutdown(wait=False, cancel_futures=True)
            self._executor = None
        self._recognizer = None
        self._state = _State.CLOSED
