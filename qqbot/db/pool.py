"""Database credential resolution and subprocess-safe connection strings."""

from __future__ import annotations

import logging
import os
from urllib.parse import parse_qsl, unquote, urlencode, urlsplit, urlunsplit


from qqbot.util import read_secret

log = logging.getLogger("qqbot.db")


def database_password() -> str | None:
    parts = urlsplit(os.getenv("DATABASE_URL", "postgresql://qqbot@postgres:5432/qqbot"))
    supplied = [
        value
        for key, value in parse_qsl(parts.query, keep_blank_values=True)
        if key.lower() == "password"
    ]
    if supplied:
        return supplied[-1]
    if parts.password is not None:
        return unquote(parts.password)
    return read_secret("DATABASE_PASSWORD_FILE", "DATABASE_PASSWORD")


def dsn(*, with_password: bool = True) -> str:
    """Never retain URI-embedded credentials in the subprocess-safe form."""
    parts = urlsplit(os.getenv("DATABASE_URL", "postgresql://qqbot@postgres:5432/qqbot"))
    auth, separator, host = parts.netloc.rpartition("@")
    netloc = auth.partition(":")[0] + "@" + host if separator else parts.netloc
    query = [
        (key, value)
        for key, value in parse_qsl(parts.query, keep_blank_values=True)
        if key.lower() != "password"
    ]
    if with_password and (password := database_password()) is not None:
        query.append(("password", password))
    return urlunsplit((parts.scheme, netloc, parts.path, urlencode(query), parts.fragment))
