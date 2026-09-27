"""Database credentials stay out of backup argv, including credentials embedded in URIs."""

from urllib.parse import parse_qsl, urlsplit

import pytest

from qqbot.db.pool import database_password, dsn


@pytest.mark.parametrize(
    "url,expected",
    [
        ("postgresql://user:fictional%40password@localhost/db", "fictional@password"),
        (
            "postgresql://user@localhost/db?password=fictional%20password&sslmode=require",
            "fictional password",
        ),
        ("postgresql://user:first@localhost/db?password=second", "second"),
        ("postgresql://user:@localhost/db", ""),
        ("postgresql://user@localhost/db", "fictional-env"),
    ],
)
def test_all_uri_password_forms_have_one_private_channel(monkeypatch, url, expected):
    monkeypatch.setenv("DATABASE_URL", url)
    monkeypatch.setenv("DATABASE_PASSWORD", "fictional-env")
    monkeypatch.delenv("DATABASE_PASSWORD_FILE", raising=False)
    safe = urlsplit(dsn(with_password=False))
    assert safe.password is None
    assert "password" not in dict(parse_qsl(safe.query, keep_blank_values=True))
    assert database_password() == expected
    connected = urlsplit(dsn())
    assert dict(parse_qsl(connected.query, keep_blank_values=True))["password"] == expected
