"""Exercise installed SDK serialization and streaming without network access."""

import json

import httpx2
import openai
import pytest

from qqbot.providers.openai_transport import (
    ResponsesTransport,
    StreamError,
    StreamInterrupted,
)

ENDPOINT = "https://sdk-test.example.invalid/v1"
KEY_ENV = "QBOT_TEST_SDK_KEY"


class Events(httpx2.AsyncByteStream):
    def __init__(self, events):
        self.events = events
        self.closed = False

    async def __aiter__(self):
        for event in self.events:
            yield ("data: " + json.dumps(event) + "\n\n").encode()

    async def aclose(self):
        self.closed = True


def terminal(status="completed"):
    response = {
        "id": "resp_synthetic",
        "object": "response",
        "created_at": 1,
        "status": status,
        "model": "fictional",
        "output": [
            {
                "type": "function_call",
                "id": "fc_synthetic",
                "call_id": "call_synthetic",
                "name": "finish_reply",
                "arguments": "{}",
                "status": "completed",
            }
        ],
        "usage": {
            "input_tokens": 10,
            "input_tokens_details": {"cached_tokens": 4},
            "output_tokens": 2,
            "output_tokens_details": {"reasoning_tokens": 1},
            "total_tokens": 12,
        },
    }
    return {"type": f"response.{status}", "sequence_number": 1, "response": response}


def transport(monkeypatch, handler):
    monkeypatch.setenv(KEY_ENV, "synthetic")
    client = openai.AsyncOpenAI(
        api_key="synthetic",
        base_url=ENDPOINT,
        max_retries=0,
        http_client=httpx2.AsyncClient(transport=httpx2.MockTransport(handler)),
    )
    adapter = ResponsesTransport()
    adapter._clients[(ENDPOINT, "synthetic")] = client
    return adapter


async def complete(adapter, *, timeout=0.2):
    return await adapter.complete(
        {
            "model": "fictional",
            "input": [{"role": "user", "content": "Fictional request"}],
            "tools": [
                {
                    "type": "function",
                    "name": "finish_reply",
                    "parameters": {
                        "type": "object",
                        "properties": {},
                        "additionalProperties": False,
                    },
                    "strict": True,
                }
            ],
            "stream": True,
            "store": False,
            "parallel_tool_calls": True,
            "timeout": timeout,
        },
        endpoint=ENDPOINT,
        credential_env=KEY_ENV,
        timeout=timeout,
        purpose="synthetic compatibility test",
    )


async def test_real_sdk_serializes_tools_and_applies_each_request_timeout(monkeypatch):
    requests, streams = [], []

    def handle(request):
        requests.append(request)
        stream = Events([terminal()])
        streams.append(stream)
        return httpx2.Response(200, headers={"content-type": "text/event-stream"}, stream=stream)

    adapter = transport(monkeypatch, handle)
    try:
        for timeout in (0.2, 0.4):
            result = await complete(adapter, timeout=timeout)
            assert result.status_hint == "completed"
            assert result.data["output"][0]["call_id"] == "call_synthetic"
            assert result.data["usage"]["input_tokens_details"]["cached_tokens"] == 4
        assert len(adapter._clients) == 1
        assert [req.extensions["timeout"]["read"] for req in requests] == [0.2, 0.4]
        for request in requests:
            body = json.loads(request.content)
            assert request.url.path == "/v1/responses"
            assert body["store"] is False and body["parallel_tool_calls"] is True
            assert body["tools"][0]["strict"] is True
            assert "timeout" not in body
        assert all(stream.closed for stream in streams)
    finally:
        await adapter.aclose()


@pytest.mark.parametrize("status", ["failed", "incomplete"])
async def test_real_sdk_keeps_non_success_terminal_states(monkeypatch, status):
    stream = Events([terminal(status)])
    adapter = transport(
        monkeypatch,
        lambda _: httpx2.Response(
            200, headers={"content-type": "text/event-stream"}, stream=stream
        ),
    )
    try:
        assert (await complete(adapter)).status_hint == status
        assert stream.closed
    finally:
        await adapter.aclose()


@pytest.mark.parametrize(
    "events,error",
    [
        ([], StreamInterrupted),
        ([terminal(), terminal()], StreamInterrupted),
        (
            [
                {
                    "type": "error",
                    "code": "synthetic_error",
                    "message": "Synthetic",
                    "sequence_number": 1,
                }
            ],
            StreamError,
        ),
    ],
)
async def test_real_sdk_rejects_broken_streams_and_closes_them(monkeypatch, events, error):
    stream = Events(events)
    adapter = transport(
        monkeypatch,
        lambda _: httpx2.Response(
            200, headers={"content-type": "text/event-stream"}, stream=stream
        ),
    )
    try:
        with pytest.raises(error):
            await complete(adapter)
        assert stream.closed
    finally:
        await adapter.aclose()


async def test_sdk_does_not_hide_an_extra_http_retry(monkeypatch):
    requests = []

    def handle(request):
        requests.append(request)
        return httpx2.Response(429, json={"error": {"message": "Synthetic rate limit"}})

    adapter = transport(monkeypatch, handle)
    try:
        with pytest.raises(openai.RateLimitError):
            await complete(adapter)
        assert len(requests) == 1
    finally:
        await adapter.aclose()


async def test_production_client_disables_sdk_retry_and_reuses_its_account(monkeypatch):
    monkeypatch.setenv(KEY_ENV, "synthetic")
    adapter = ResponsesTransport()
    options = {
        "endpoint": ENDPOINT,
        "credential_env": KEY_ENV,
        "purpose": "test",
        "key_required": True,
    }
    try:
        first = adapter._client(timeout=1, **options)
        second = adapter._client(timeout=2, **options)
        assert first is second
        assert first.max_retries == 0
    finally:
        await adapter.aclose()
