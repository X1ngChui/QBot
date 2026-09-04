"""The platform edge: OneBot events in, domain objects out, then on to the services.

This is the only layer that knows what OneBot is - the anti-corruption layer. domain and
services know nothing of QQ or of NoneBot, so changing platform or adapter does not reach
them.
"""

from .ingest import Ingested, Ingestor
from .onebot import GroupMessage, Role, Sender

__all__ = ["GroupMessage", "Role", "Sender", "Ingestor", "Ingested"]
