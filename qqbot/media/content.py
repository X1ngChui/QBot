"""Explicit byte outcomes and allocation-safe decoding for protocol media."""

import base64
import binascii
from enum import Enum, auto


class ByteFailure(Enum):
    TOO_LARGE = auto()


type ByteRead = bytes | ByteFailure | None


def decode_base64(value: object, max_bytes: int) -> ByteRead:
    if not isinstance(value, str) or not value:
        return None
    prefix = value.find(",", 0, 128) + 1 if value.startswith("data:") else 0
    # Validate the encoded size before slicing or decoding. Padding can overestimate
    # the decoded size by at most two bytes, checked again after decoding.
    if len(value) - prefix > 4 * ((max_bytes + 2) // 3):
        return ByteFailure.TOO_LARGE
    try:
        result = base64.b64decode(value[prefix:], validate=True)
    except (binascii.Error, ValueError):
        return None
    if len(result) > max_bytes:
        return ByteFailure.TOO_LARGE
    return result or None
