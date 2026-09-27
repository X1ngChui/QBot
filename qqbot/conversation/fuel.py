"""Per-session work and retained-output bounds, independent of prices or model behavior."""

from dataclasses import dataclass

from qqbot.util import SYS_L, SYS_R

MAX_MODEL_TURNS = 20
MAX_CALLS_PER_TURN = 8
MAX_CALLS_PER_SESSION = 64
MAX_WIRE_CALLS = 32
MAX_ARGUMENT_BYTES = 16_384
MAX_RESULT_BYTES = 65_536
MAX_IMAGE_PARTS = 24


@dataclass(slots=True)
class SessionFuel:
    turns_left: int = MAX_MODEL_TURNS
    calls_left: int = MAX_CALLS_PER_SESSION
    result_bytes_left: int = MAX_RESULT_BYTES
    images_left: int = MAX_IMAGE_PARTS

    @property
    def wrapping_up(self) -> bool:
        return self.turns_left <= 1 or self.calls_left == 0 or self.result_bytes_left < 4

    def begin_turn(self) -> bool:
        if self.turns_left == 0:
            return False
        self.turns_left -= 1
        return True

    def take_calls(self, count: int) -> int:
        admitted = min(count, MAX_CALLS_PER_TURN, self.calls_left)
        self.calls_left -= admitted
        return admitted

    def take_images(self, count: int) -> int:
        admitted = min(count, self.images_left)
        self.images_left -= admitted
        return admitted

    def retain(self, value: str) -> str:
        """Bound UTF-8 allocation before encoding and never cut a trusted marker in half."""
        remaining = self.result_bytes_left
        if remaining < 4:
            return ""
        prefix = value[:remaining]
        encoded = prefix.encode("utf-8")
        if len(prefix) == len(value) and len(encoded) <= remaining:
            self.result_bytes_left -= len(encoded)
            return value
        prefix = encoded[: remaining - 3].decode("utf-8", errors="ignore")
        if prefix.rfind(SYS_L) > prefix.rfind(SYS_R):
            prefix = prefix[: prefix.rfind(SYS_L)]
        result = prefix + "…"
        self.result_bytes_left -= len(result.encode("utf-8"))
        return result
