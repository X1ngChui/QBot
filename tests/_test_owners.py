"""Fresh runtime owners for independent workflow tests."""

from _fixtures import example_bundle
import _db as test_db
from qqbot.repositories.ledger import LedgerRepository
from qqbot.services.budget import Budget
from qqbot.services.members import MemberDirectory


def fresh_budget() -> Budget:
    return Budget(
        LedgerRepository(test_db.pool, today=test_db.clock.today),
        daily_cap=example_bundle().default.budget.daily_cny_cap,
        today=test_db.clock.today,
    )


def fresh_members() -> MemberDirectory:
    return MemberDirectory()
