"""Mechanism policies are independent of legacy YAML and preserve resource bounds."""

import sys
from types import SimpleNamespace
from unittest.mock import Mock

import pytest
from hypothesis import given, strategies as st

from _budget import fake_budget
import _db as _test_db
from qqbot.configuration import AsrCfg
from qqbot.conversation.history import HistoryWindow
from qqbot.conversation.prompt import history_window
from qqbot.conversation.state import ChatMsg, GroupState
from qqbot.domain.ids import AccountId, GroupId, MessageId
from qqbot.providers.sherpa import SherpaAsr
from qqbot.runtime import Runtime
from _fixtures import now_local


@given(size=st.integers(1, 300), arrivals=st.integers(1, 1000))
def test_history_capacity_and_chunk_are_derived_from_one_preference(size, arrivals):
    policy = HistoryWindow(size)
    state = GroupState(
        GroupId("311"),
        history=policy,
        groups=_test_db.groups,
        archive=_test_db.archive,
        display_zone=_test_db.clock.zone,
    )
    stamp = now_local()
    for index in range(arrivals):
        state.add(ChatMsg(MessageId(str(index)), AccountId("101"), "Fictional", "x", stamp))
    shown = history_window(state, None)
    assert state.recent.maxlen == size + 2 * policy.chunk
    assert 1 <= len(shown) <= size
    assert len(state.recent) <= policy.capacity
    assert shown == history_window(state, None)
    assert shown[-1].msg_id == str(arrivals - 1)


def test_invalid_history_size_cannot_create_an_unbounded_deque():
    with pytest.raises(ValueError):
        HistoryWindow(0)


async def test_runtime_uses_configured_history_and_total_reply_capacity(bundle):
    cfg = bundle.default.model_copy(
        update={
            "conversation": bundle.default.conversation.model_copy(update={"history_messages": 17}),
            "runtime": bundle.default.runtime.model_copy(update={"reply_capacity": 7}),
        }
    )
    from qqbot.configuration import ConfigBundle

    configured = ConfigBundle(
        cfg.model_dump(), {"default": bundle._default_persona}, bundle.prompts, bundle.predicates
    )
    runtime = Runtime.build(bundle=configured)
    try:
        assert runtime.registry._history == HistoryWindow(17)
        assert runtime.gateway._replies._capacity == 7
    finally:
        await runtime.aclose()


def test_real_asr_builder_signature_and_warmup(monkeypatch, tmp_path):
    (tmp_path / "model.int8.onnx").touch()
    (tmp_path / "tokens.txt").touch()
    stream = SimpleNamespace(accept_waveform=Mock(), result=SimpleNamespace(text=""))
    recognizer = SimpleNamespace(create_stream=Mock(return_value=stream), decode_stream=Mock())
    build = Mock(return_value=recognizer)
    monkeypatch.setitem(
        sys.modules,
        "sherpa_onnx",
        SimpleNamespace(OfflineRecognizer=SimpleNamespace(from_sense_voice=build)),
    )
    cfg = AsrCfg(model_dir=str(tmp_path), threads=3)
    assert SherpaAsr._build(cfg) is recognizer
    assert build.call_args.kwargs["num_threads"] == 3
    recognizer.decode_stream.assert_called_once_with(stream)
    stream.accept_waveform.assert_called_once()


def test_asr_queue_capacity_is_owned_and_must_be_bounded():
    cfg = AsrCfg(model_dir="fictional")
    assert SherpaAsr(cfg, fake_budget())._queue_capacity == 8
    with pytest.raises(ValueError):
        SherpaAsr(cfg, fake_budget(), queue_capacity=0)
