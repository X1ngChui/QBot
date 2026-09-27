"""Billing attribution is typed before a model request can be dispatched."""

import pytest

from qqbot.domain.ids import GroupId
from qqbot.providers.contracts import CallContext, CallPurpose


@pytest.mark.parametrize("value", ["311", "", 311, 0, False])
def test_call_context_rejects_untyped_group_identifiers(value):
    with pytest.raises(TypeError, match="group_id must be GroupId or None"):
        CallContext(CallPurpose.EXTRACT, value)


@pytest.mark.parametrize("group_id", [None, GroupId("311")])
def test_call_context_preserves_valid_billing_attribution(group_id):
    context = CallContext(CallPurpose.EXTRACT, group_id)
    assert context.group_id is group_id
