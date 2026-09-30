"""Offline, exhaustive conversion of the pre-refactor configuration contract.

Runtime loading never calls this module. The manifest contains public schema defaults,
not a deployment's settings, and diagnostics deliberately report paths without values.
"""

from __future__ import annotations

import json
from dataclasses import dataclass
from pathlib import Path
from typing import Any

from pydantic import ValidationError

from qqbot.configuration.schema import Settings

MANIFEST = Path(__file__).resolve().parent / "settings_v1.json"


@dataclass(frozen=True, slots=True)
class FieldRule:
    target: str | None
    reason: str
    derived: bool = False


@dataclass(frozen=True, slots=True)
class FieldDisposition:
    source: str
    target: str | None
    reason: str
    explicit: bool
    derived: bool


@dataclass(frozen=True, slots=True)
class MigrationResult:
    sparse: dict[str, Any]
    settings: Settings
    fields: tuple[FieldDisposition, ...]


class MigrationError(ValueError):
    def __init__(self, paths: list[str], reason: str) -> None:
        self.paths = tuple(sorted(paths))
        super().__init__(reason + ": " + ", ".join(self.paths))


def field_rules() -> dict[str, FieldRule]:
    manifest = json.loads(MANIFEST.read_text(encoding="utf-8"))
    rules: dict[str, FieldRule] = {}
    for source in manifest:
        if source.startswith("capabilities."):
            rules[source] = FieldRule(
                source.replace("capabilities.", "backends.", 1),
                "Independent backend connection, quality or account capacity.",
            )
        elif source.startswith("budget."):
            rules[source] = FieldRule(source, "Operator spending stop-loss.")
        elif source.startswith("database."):
            rules[source] = FieldRule("runtime." + source, "Deployment database resources.")
        elif source.startswith("scheduled_tasks."):
            rules[source] = FieldRule(
                source.replace("scheduled_tasks.", "tasks.", 1), "Durable task admission policy."
            )
        elif source.startswith("schedule."):
            rules[source] = FieldRule(
                source.replace("schedule.", "maintenance.", 1),
                "Maintenance timing or storage retention.",
            )

    moves = {
        "owners": "bot.owners",
        "timezone": "bot.timezone",
        "trigger.nicknames": "bot.nicknames",
        "media.max_image_mb": "media.max_image_mb",
        "tools.max_messages_per_reply": "conversation.max_messages_per_reply",
        "tools.max_reply_sec": "conversation.reply_deadline_sec",
        "tools.send_message.max_text_chars_per_message": "conversation.max_text_chars_per_message",
        "tools.web_search.count": "backends.search.count",
        "tools.web_search.depth": "backends.search.depth",
        "capabilities.vision.description_ttl_days": "media.description_ttl_days",
        "capabilities.vision.max_images_per_min": "media.max_images_per_min",
        "capabilities.asr.max_audio_sec": "media.max_audio_sec",
        "capabilities.asr.max_clips_per_min": "media.max_clips_per_min",
        "prompt.evidence_ttl_days": "conversation.evidence_ttl_days",
        "memory.episode_ttl_days": "memory.episode_ttl_days",
        "memory.alias_unused_days": "memory.alias_unused_days",
        "memory.joke_unused_days": "memory.temporary_alias_days",
        "scheduled_tasks.completed_keep_days": "maintenance.completed_task_keep_days",
        "personas_dir": "runtime.paths.personas_dir",
        "prompts_dir": "runtime.paths.prompts_dir",
        "predicates_file": "runtime.paths.predicates_file",
    }
    for source, target in moves.items():
        rules[source] = FieldRule(target, "Preserve this operator choice at its owning mechanism.")
    for source in ("prompt.window_chunks", "prompt.evict_chunk"):
        rules[source] = FieldRule(
            "conversation.history_messages",
            "Preserve total window size; sliding chunks are internal.",
            True,
        )

    retired_groups = {
        "media.": "Owned work deadlines, bounded retries and backend attachment expiry.",
        "members.": "A size-bounded member cache owns its refresh policy.",
        "commands.": "Command pagination and rendering enforce their own output bounds.",
        "identity_link.": "Invitation lifetime and admission are code-owned invariants.",
        "diagnostics.": "Bounded diagnostic buffers and output projection own these limits.",
        "tools.": "Session fuel and bounded tool result projection replace independent knobs.",
        "prompt.": "Bounded parsing, stable window projection and evidence rendering.",
        "memory.": "Bounded durable work units with claim fencing, renewal and retries.",
    }
    for source in manifest:
        if source not in rules:
            for prefix, reason in retired_groups.items():
                if source.startswith(prefix):
                    rules[source] = FieldRule(None, reason)
                    break
    retired = {
        "capabilities.http_retries": "Each adapter owns a bounded retry policy.",
        "capabilities.retry_after_cap_sec": "Retry wait cannot exceed its operation deadline.",
        "capabilities.embedding.dimensions": "Vector width belongs to the adapter/schema contract.",
        "capabilities.asr.queue_capacity": "Media admission bounds decoding work.",
        "schedule.extract_drain_hours": "Maintenance stages own deadlines and durable progress.",
        "schedule.decay_drain_min": "Maintenance stages own deadlines and durable progress.",
        "schedule.drain_poll_sec": "Worker completion is an execution mechanism, not user policy.",
        "schedule.misfire_grace_sec": "The maintenance scheduler owns missed-trigger handling.",
        "schedule.backup_stale_hours": "Backup health follows expected scheduled completion.",
        "scheduled_tasks.max_pending_per_account": (
            "Tasks are group-owned; account admission is retired."
        ),
        "scheduled_tasks.min_delay_sec": "The scheduling service enforces its fixed minimum delay.",
        "scheduled_tasks.poll_sec": "The durable worker owns bounded polling.",
        "scheduled_tasks.max_concurrency": "The execution scheduler owns task slots.",
    }
    rules.update({source: FieldRule(None, reason) for source, reason in retired.items()})
    if rules.keys() != manifest.keys():
        raise RuntimeError("configuration migration must classify every legacy field exactly once")
    return rules


def _flatten(raw: dict[str, Any], known: set[str]) -> dict[str, Any]:
    leaves: dict[str, Any] = {}
    prefixes = {
        path.rsplit(".", level)[0] for path in known for level in range(1, path.count(".") + 1)
    }

    def walk(value, prefix=""):
        if not isinstance(value, dict):
            raise MigrationError([prefix or "<root>"], "expected a configuration mapping")
        for key, item in value.items():
            if not isinstance(key, str):
                raise MigrationError([prefix or "<root>"], "configuration keys must be strings")
            path = prefix + "." + key if prefix else key
            if path in known:
                leaves[path] = item
            elif path in prefixes:
                walk(item, path)
            else:
                raise MigrationError([path], "unknown legacy configuration field")

    walk(raw)
    return leaves


def _put(result: dict, path: str, value: Any) -> None:
    parts = path.split(".")
    node = result
    for part in parts[:-1]:
        node = node.setdefault(part, {})
    if parts[-1] in node:
        raise MigrationError([path], "multiple values target the same field")
    node[parts[-1]] = value


def convert(
    raw: dict[str, Any], *, retire_overrides: frozenset[str] = frozenset()
) -> MigrationResult:
    manifest = json.loads(MANIFEST.read_text(encoding="utf-8"))
    rules = field_rules()
    explicit = _flatten(raw, set(manifest))
    invalid_acceptance = [
        path for path in retire_overrides if path not in rules or rules[path].target is not None
    ]
    if invalid_acceptance:
        raise MigrationError(
            invalid_acceptance, "only explicitly retired fields can be acknowledged"
        )
    conflicts = []
    sparse: dict[str, Any] = {}
    for path, value in explicit.items():
        rule = rules[path]
        if rule.derived:
            continue
        if rule.target is not None:
            _put(sparse, rule.target, value)
        else:
            default = manifest[path].get("default")
            normalized = list(value) if isinstance(value, tuple) else value
            if normalized != default and path not in retire_overrides:
                conflicts.append(path)
    if conflicts:
        raise MigrationError(conflicts, "nondefault retired settings need explicit disposition")

    window_paths = ("prompt.window_chunks", "prompt.evict_chunk")
    if any(path in explicit for path in window_paths):
        factors = []
        for path in window_paths:
            value = explicit.get(path, manifest[path]["default"])
            try:
                parsed = int(value)
                valid = parsed > 0 and not isinstance(value, bool)
                if isinstance(value, float):
                    valid = valid and parsed == value
            except (TypeError, ValueError, OverflowError):
                valid = False
            if not valid:
                raise MigrationError([path], "history factors must be positive integers")
            factors.append(parsed)
        _put(sparse, "conversation.history_messages", factors[0] * factors[1])
    try:
        settings = Settings.model_validate(sparse)
    except ValidationError as exc:
        paths = [".".join(map(str, error["loc"])) for error in exc.errors(include_input=False)]
        raise MigrationError(paths, "converted settings fail the new schema") from None
    fields = tuple(
        FieldDisposition(path, rule.target, rule.reason, path in explicit, rule.derived)
        for path, rule in rules.items()
    )
    return MigrationResult(sparse, settings, fields)
