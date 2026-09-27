"""Production budgets are composed once and never fetched from a service locator."""

import ast
from pathlib import Path

from qqbot.runtime import Runtime


async def test_runtime_injects_one_budget_into_every_capability_and_owner(bundle):
    runtime = Runtime.build(bundle)
    try:
        budget = runtime.budget
        owners = (
            runtime.router,
            runtime.worker,
            runtime.media,
            runtime.media_processor,
            runtime.providers.text._executor,
            runtime.providers.vision._executor,
            runtime.providers.asr,
            runtime.providers.embedding,
            runtime.providers.search,
        )
        assert all(owner._budget is budget for owner in owners)
        assert runtime.reply_executor.budget is budget
    finally:
        await runtime.aclose()


def test_no_production_budget_service_locator_or_ledger_facade():
    root = Path(__file__).parents[2] / "qqbot"
    for path in root.rglob("*.py"):
        tree = ast.parse(path.read_text(encoding="utf-8"))
        for node in ast.walk(tree):
            if isinstance(node, ast.ImportFrom) and node.module == "qqbot.services.budget":
                assert all(alias.name != "BUDGET" for alias in node.names), path
    budget_tree = ast.parse((root / "services/budget.py").read_text(encoding="utf-8"))
    for node in budget_tree.body:
        if isinstance(node, ast.Assign):
            assert not any(
                isinstance(target, ast.Name) and target.id == "BUDGET" for target in node.targets
            )


async def test_runtime_injects_narrow_storage_and_one_shared_reply_inbox(bundle):
    runtime = Runtime.build(bundle)
    try:
        assert not hasattr(runtime, "operations")
        assert runtime.router._groups is runtime.groups
        assert runtime.registry._policies is runtime.groups
        assert runtime.registry._archive is runtime.archive
        assert runtime.media._archive is runtime.archive
        assert runtime.media_processor._cache is runtime.media_cache
        assert runtime.reply_executor.archive is runtime.archive
        assert runtime.reply_executor.identities is runtime.identities
        assert runtime.reply_executor.evidence_store is runtime.evidence
        assert runtime.gateway._replies is runtime.scheduled._replies is runtime.replies
        assert runtime.replies._execute is runtime.reply_executor
    finally:
        await runtime.aclose()
