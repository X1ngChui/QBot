"""The platform edge: OneBot events in, domain objects out, then on to the services.

This is the only layer that knows what OneBot is - the anti-corruption layer. domain and
services know nothing of QQ or of NoneBot, so changing platform or adapter does not reach
them.
"""

from qqbot.gateway.ingest import Ingested
from qqbot.gateway.ingest import Ingestor
from qqbot.gateway.onebot import GroupMessage
from qqbot.gateway.onebot import Role
from qqbot.gateway.onebot import Sender

__all__ = ["GroupMessage", "Role", "Sender", "Ingestor", "Ingested"]
