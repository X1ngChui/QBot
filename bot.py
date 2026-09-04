"""Entrypoint. NapCat connects in over a reverse WebSocket, so the bot is the server.

Endpoint: ws://bot:8080/onebot/v11/ws
"""

from __future__ import annotations

import logging
import logging.handlers
import os
from pathlib import Path

import nonebot
from nonebot.adapters.onebot.v11 import Adapter

LOG_DIR = Path(os.getenv("LOG_DIR", "/app/logs"))


def _setup_logging() -> None:
    LOG_DIR.mkdir(parents=True, exist_ok=True)
    # Rotating, because nothing else ever truncates this file: the container's own
    # stdout is capped by the compose logging options, and this is the only log that
    # is not.
    handler = logging.handlers.RotatingFileHandler(
        LOG_DIR / "qqbot.log", maxBytes=5 * 1024 * 1024, backupCount=3,
        encoding="utf-8")
    handler.setFormatter(
        logging.Formatter("%(asctime)s %(levelname)-7s %(name)s: %(message)s")
    )
    root = logging.getLogger("qqbot")
    root.setLevel(logging.INFO)
    root.addHandler(handler)
    root.addHandler(logging.StreamHandler())


def main() -> None:
    _setup_logging()
    nonebot.init(
        driver="~fastapi",
        host=os.getenv("HOST", "0.0.0.0"),
        port=int(os.getenv("PORT", "8080")),
        command_start=["/"],
        command_sep=["."],
        log_level=os.getenv("LOG_LEVEL", "INFO"),
    )
    nonebot.get_driver().register_adapter(Adapter)
    nonebot.load_plugin("qqbot.plugin")
    nonebot.run()


if __name__ == "__main__":
    main()
