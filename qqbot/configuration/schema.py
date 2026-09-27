"""Operator decisions, separate from protocol and implementation safety policies."""

from __future__ import annotations

from typing import Any, Literal
from urllib.parse import urlsplit
from zoneinfo import ZoneInfo, ZoneInfoNotFoundError

from apscheduler.triggers.cron import CronTrigger
from pydantic import BaseModel, ConfigDict, Field, field_validator, model_validator


class ConfigModel(BaseModel):
    model_config = ConfigDict(extra="forbid", frozen=True, allow_inf_nan=False)


class BotCfg(ConfigModel):
    owners: tuple[str, ...] = Field(
        default=(), description="Accounts authorized to operate the bot."
    )
    timezone: str = Field(
        default="Asia/Shanghai", description="IANA timezone for conversation and jobs."
    )
    nicknames: tuple[str, ...] = Field(default=(), description="Names matched as complete words.")

    @field_validator("owners", mode="before")
    @classmethod
    def _account_strings(cls, value: Any) -> Any:
        return tuple(str(item) for item in value) if isinstance(value, list | tuple) else value

    @field_validator("timezone")
    @classmethod
    def _timezone_exists(cls, value: str) -> str:
        try:
            ZoneInfo(value)
        except (ZoneInfoNotFoundError, ValueError):
            raise ValueError("timezone must be a valid IANA timezone name") from None
        return value


class ConversationCfg(ConfigModel):
    history_messages: int = Field(
        default=90, ge=1, description="Recent messages available to a reply."
    )
    max_messages_per_reply: int = Field(
        default=4, ge=1, description="Confirmed sends allowed per reply."
    )
    reply_deadline_sec: float = Field(
        default=180,
        ge=30,
        le=900,
        description="Whole reply deadline, including queue and media waits.",
    )
    max_text_chars_per_message: int = Field(
        default=2000,
        ge=400,
        description="Output text ceiling; never truncates the inbound archive.",
    )
    evidence_ttl_days: int = Field(
        default=30,
        ge=1,
        description="Retention of retrieval evidence associated with a sent message.",
    )


Effort = Literal["off", "low", "high", "max"]


class NetworkBackend(ConfigModel):
    endpoint: str = Field(min_length=1, description="Provider API endpoint, without credentials.")
    credential_env: str = Field(min_length=1, description="Credential environment-variable name.")

    @field_validator("endpoint")
    @classmethod
    def _http_endpoint(cls, value: str) -> str:
        try:
            url = urlsplit(value)
            valid = url.scheme in {"http", "https"} and bool(url.hostname)
            valid = valid and url.username is None and url.password is None
            _ = url.port
        except ValueError:
            valid = False
        if not valid:
            raise ValueError("endpoint must be an HTTP(S) URL without embedded credentials")
        return value


class TextUseCfg(ConfigModel):
    model: str = Field(default="", description="Optional extraction model on the text account.")
    reasoning_effort: Effort | None = Field(
        default=None, description="Extraction reasoning override."
    )
    timeout_sec: float | None = Field(
        default=None, gt=0, description="Extraction request deadline."
    )


class TextCfg(NetworkBackend):
    provider: Literal["deepseek", "openai_responses", "local"] = Field(
        description="Adapter selected for this capability."
    )
    credential_env: str = Field(
        default="TEXT_API_KEY", description="Credential environment-variable name."
    )
    model: str = Field(min_length=1, description="Model identifier accepted by this adapter.")
    reasoning_effort: Effort = Field(
        default="off", description="Reasoning grade; may incur output charges."
    )
    max_concurrency: int = Field(
        default=3, ge=1, description="Concurrent requests on this text account."
    )
    timeout_sec: float = Field(default=30, gt=0, description="Deadline of one text request.")
    retries: int = Field(
        default=2, ge=0, description="Retries allowed for retryable provider failures."
    )
    extract: TextUseCfg = Field(default_factory=TextUseCfg)

    def for_extract(self) -> TextCfg:
        use = self.extract
        return self.model_copy(
            update={
                "model": use.model or self.model,
                "reasoning_effort": use.reasoning_effort or self.reasoning_effort,
                "timeout_sec": use.timeout_sec if use.timeout_sec is not None else self.timeout_sec,
            }
        )


class VisionCfg(NetworkBackend):
    provider: Literal["deepseek", "openai_responses", "local"] = Field(
        description="Adapter selected for this capability."
    )
    credential_env: str = Field(
        default="TEXT_API_KEY", description="Credential environment-variable name."
    )
    model: str = Field(min_length=1, description="Model identifier accepted by this adapter.")
    reasoning_effort: Effort = Field(
        default="off", description="Reasoning grade; may incur output charges."
    )
    max_concurrency: int = Field(
        default=1, ge=1, le=16, description="Concurrent description requests."
    )
    max_output_tokens: int = Field(
        default=4096, ge=256, le=8192, description="Description output and reasoning token ceiling."
    )
    timeout_sec: float = Field(
        default=30, gt=0, description="Deadline of one backend request in seconds."
    )


class AsrCfg(ConfigModel):
    model_dir: str = Field(min_length=1, description="Local SenseVoice model directory.")
    threads: int = Field(
        default=2, ge=1, description="Native inference threads in the local recognizer."
    )


class EmbeddingCfg(NetworkBackend):
    provider: Literal["dashscope"] = Field(description="Adapter selected for this capability.")
    credential_env: str = Field(
        default="MEDIA_API_KEY", description="Credential environment-variable name."
    )
    model: str = Field(min_length=1, description="Model identifier accepted by this adapter.")
    timeout_sec: float = Field(
        default=60, gt=0, description="Deadline of one backend request in seconds."
    )


class SearchCfg(NetworkBackend):
    provider: Literal["tavily"] = Field(description="Adapter selected for this capability.")
    credential_env: str = Field(
        default="SEARCH_API_KEY", description="Credential environment-variable name."
    )
    monthly_quota: int = Field(
        default=1000, ge=0, description="Shared monthly provider-credit allowance."
    )
    proxy: str = Field(default="", description="Search-only HTTP proxy; empty means direct.")
    timeout_sec: float = Field(
        default=20, gt=0, description="Deadline of one backend request in seconds."
    )
    count: int = Field(default=5, ge=1, le=20, description="Results requested for one web search.")
    depth: Literal["basic", "advanced"] = Field(
        default="basic", description="Advanced search consumes two provider credits."
    )


class BackendsCfg(ConfigModel):
    text: TextCfg
    vision: VisionCfg
    asr: AsrCfg
    embedding: EmbeddingCfg
    search: SearchCfg


class MediaCfg(ConfigModel):
    max_image_mb: float = Field(
        default=8, gt=0, description="Largest decoded/downloaded image payload."
    )
    max_audio_sec: int = Field(
        default=300, ge=1, description="Longest voice clip admitted for transcription."
    )
    max_images_per_min: int = Field(
        default=6, ge=1, description="Per-group automatic description pace."
    )
    max_clips_per_min: int = Field(default=6, ge=1, description="Per-group voice admission pace.")
    description_ttl_days: int = Field(
        default=15, ge=0, description="Description refresh age; zero disables."
    )


class MemoryCfg(ConfigModel):
    episode_ttl_days: float = Field(
        default=90, gt=0, description="Availability of semantic event recall."
    )
    alias_unused_days: float = Field(
        default=30, gt=0, description="Lifetime of unused candidate names."
    )
    temporary_alias_days: float = Field(
        default=7, gt=0, description="Lifetime of temporary candidate names."
    )


class BudgetCfg(ConfigModel):
    daily_cny_cap: float = Field(
        default=5, ge=0, description="Global stop-loss on already booked daily spend."
    )
    per_reply_cny: float = Field(
        default=0.30, ge=0, description="Per-reply stop-loss, including its media work."
    )


class TasksCfg(ConfigModel):
    max_days_ahead: int = Field(
        default=30, ge=1, le=365, description="Furthest future wakeup in days."
    )
    max_pending_per_account: int = Field(
        default=5, ge=1, description="Pending wakeups per account and group."
    )
    max_pending_per_group: int = Field(
        default=50, ge=1, description="Pending wakeups across a group."
    )
    max_chain_depth: int = Field(
        default=24, ge=0, description="Follow-up depth from one original task."
    )
    max_executions_per_group_day: int = Field(
        default=24, ge=1, description="Wakeups per group and local day."
    )


class MaintenanceCfg(ConfigModel):
    nightly_cron: str = Field(
        default="30 2 * * *", description="Nightly maintenance schedule in bot.timezone."
    )
    report_cron: str = Field(
        default="0 0 * * *", description="Owner report schedule in bot.timezone."
    )
    backup_keep: int = Field(default=14, ge=1, description="Verified database dumps retained.")
    napcat_cache_days: int = Field(
        default=7, ge=1, description="NapCat media-cache retention in days."
    )
    completed_job_keep_days: int = Field(
        default=30, ge=1, le=3650, description="Finished memory-job audit retention in days."
    )
    completed_task_keep_days: int = Field(
        default=30, ge=1, description="Terminal wakeup retention in days."
    )

    @field_validator("nightly_cron", "report_cron")
    @classmethod
    def _valid_cron(cls, value: str) -> str:
        if len(value.split()) != 5:
            raise ValueError("cron expression must have five fields")
        CronTrigger.from_crontab(value)
        return value


class DatabaseCfg(ConfigModel):
    pool_min: int = Field(
        default=2, ge=1, description="Minimum database pool size; cannot exceed pool_max."
    )
    pool_max: int = Field(
        default=8, ge=1, description="Maximum connections in the application pool."
    )
    command_timeout_sec: float = Field(
        default=20, gt=0, description="Database command timeout in seconds."
    )

    @model_validator(mode="after")
    def _coherent_pool(self) -> DatabaseCfg:
        if self.pool_min > self.pool_max:
            raise ValueError("pool_min must not exceed pool_max")
        return self


class PathsCfg(ConfigModel):
    personas_dir: str = Field(
        default="personas", description="Persona directory relative to CONFIG_DIR."
    )
    prompts_dir: str = Field(
        default="prompts", description="Prompt directory relative to CONFIG_DIR."
    )
    predicates_file: str = Field(
        default="predicates.yaml", description="Predicate table relative to CONFIG_DIR."
    )


class RuntimeCfg(ConfigModel):
    reply_capacity: int = Field(
        default=32, ge=1, description="Total active and waiting reply snapshots."
    )
    database: DatabaseCfg = Field(default_factory=DatabaseCfg)
    paths: PathsCfg = Field(default_factory=PathsCfg)


class Settings(ConfigModel):
    bot: BotCfg = Field(default_factory=BotCfg)
    conversation: ConversationCfg = Field(default_factory=ConversationCfg)
    backends: BackendsCfg
    media: MediaCfg = Field(default_factory=MediaCfg)
    memory: MemoryCfg = Field(default_factory=MemoryCfg)
    budget: BudgetCfg = Field(default_factory=BudgetCfg)
    tasks: TasksCfg = Field(default_factory=TasksCfg)
    maintenance: MaintenanceCfg = Field(default_factory=MaintenanceCfg)
    runtime: RuntimeCfg = Field(default_factory=RuntimeCfg)
