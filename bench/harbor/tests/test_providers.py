"""Tests for the model-prefix mapping in the ReagentAgent adapter.

The mapping is pure Python: it picks the reagent wire, the gateway base URL
and the API-key variable from the model prefix, with agent options overriding
each part. No container, no Harbor run and no LLM call.
"""

from __future__ import annotations

from pathlib import Path, PurePosixPath

import pytest

from reagent_harbor.agent import (
    ReagentAgent,
    ReagentOptions,
    build_command,
    model_id,
    provider_defaults,
    session_headers,
)


def test_anthropic_prefix_uses_the_anthropic_wire_and_base() -> None:
    defaults = provider_defaults("anthropic/claude-sonnet-5-5")
    assert defaults.wire == "anthropic"
    assert defaults.base_url == "https://api.anthropic.com"
    assert defaults.api_key_env == "ANTHROPIC_API_KEY"


def test_anthropic_prefix_uses_the_host_base_url() -> None:
    defaults = provider_defaults(
        "anthropic/claude-sonnet-5-5", anthropic_base_url="http://gateway:8080"
    )
    assert defaults.base_url == "http://gateway:8080"


def test_opencode_go_defaults_to_chat_completions() -> None:
    defaults = provider_defaults("opencode-go/kimi-k2.6")
    assert defaults.wire == "openai-chat"
    assert defaults.base_url == "https://opencode.ai/inference/go/openai/v1"
    assert defaults.api_key_env == "OPENCODE_API_KEY"


def test_opencode_go_base_url_follows_the_wire() -> None:
    anthropic = provider_defaults("opencode-go/kimi-k2.6", wire="anthropic")
    assert anthropic.base_url == "https://opencode.ai/inference/go/anthropic"
    responses = provider_defaults("opencode-go/kimi-k2.6", wire="openai-responses")
    assert responses.base_url == "https://opencode.ai/inference/go/openai/v1"


def test_commandcode_defaults_to_chat_completions() -> None:
    defaults = provider_defaults("commandcode/deepseek/deepseek-v4-flash")
    assert defaults.wire == "openai-chat"
    assert defaults.base_url == "https://api.commandcode.ai/provider/v1"
    assert defaults.api_key_env == "CMD_API_KEY"


def test_commandcode_base_url_follows_the_wire() -> None:
    anthropic = provider_defaults("commandcode/x", wire="anthropic")
    assert anthropic.base_url == "https://api.commandcode.ai/provider"
    responses = provider_defaults("commandcode/x", wire="openai-responses")
    assert responses.base_url == "https://api.commandcode.ai/provider/v1"


def test_each_option_overrides_its_default() -> None:
    defaults = provider_defaults(
        "opencode-go/kimi-k2.6",
        wire="anthropic",
        base_url="http://localhost:9000",
        api_key_env="MY_GATEWAY_KEY",
    )
    assert defaults.wire == "anthropic"
    assert defaults.base_url == "http://localhost:9000"
    assert defaults.api_key_env == "MY_GATEWAY_KEY"


def test_an_unknown_prefix_needs_every_option() -> None:
    with pytest.raises(ValueError, match="no known provider prefix"):
        provider_defaults("openai/gpt-5")
    defaults = provider_defaults(
        "openai/gpt-5",
        wire="openai-chat",
        base_url="http://localhost:9000",
        api_key_env="OPENAI_API_KEY",
    )
    assert defaults.wire == "openai-chat"
    assert defaults.base_url == "http://localhost:9000"
    assert defaults.api_key_env == "OPENAI_API_KEY"


def test_the_model_id_is_everything_after_the_first_slash() -> None:
    assert (
        model_id("commandcode/deepseek/deepseek-v4-flash")
        == "deepseek/deepseek-v4-flash"
    )
    assert model_id("anthropic/claude-sonnet-5-5") == "claude-sonnet-5-5"


def test_the_session_header_is_sent_only_for_opencode_go() -> None:
    headers = session_headers("opencode-go/kimi-k2.6")
    assert len(headers) == 1
    name, value = headers[0]
    assert name == "x-opencode-session"
    assert value
    assert session_headers("anthropic/claude-sonnet-5-5") == []
    assert session_headers("commandcode/x") == []


def test_each_trial_gets_a_new_session_id() -> None:
    first = session_headers("opencode-go/kimi-k2.6")[0][1]
    second = session_headers("opencode-go/kimi-k2.6")[0][1]
    assert first != second


def test_the_key_value_is_never_in_the_command() -> None:
    key_value = "sk-gateway-not-a-real-key"
    command = build_command(
        workdir="/app",
        model_id="kimi-k2.6",
        task_env="REAGENT_TASK_TEST",
        logs_dir=PurePosixPath("/logs/agent"),
        timeout_sec=None,
        provider="openai-chat",
        base_url="https://opencode.ai/inference/go/openai/v1",
        api_key_env="OPENCODE_API_KEY",
        headers=session_headers("opencode-go/kimi-k2.6"),
    )
    assert "--provider openai-chat" in command
    assert "--base-url https://opencode.ai/inference/go/openai/v1" in command
    assert "--api-key-env OPENCODE_API_KEY" in command
    assert "--header x-opencode-session:" in command
    assert key_value not in command


def test_reagent_options_accept_the_three_wires() -> None:
    for wire in ("anthropic", "openai-chat", "openai-responses"):
        assert ReagentOptions(wire=wire).wire == wire


def test_reagent_options_reject_an_unknown_wire() -> None:
    with pytest.raises(ValueError):
        ReagentOptions(wire="google")


def test_harbor_resolves_no_key_for_the_gateway_prefixes(tmp_path: Path) -> None:
    agent = ReagentAgent(
        logs_dir=tmp_path,
        model_name="opencode-go/kimi-k2.6",
        extra_env={"OPENCODE_API_KEY": "secret"},
    )
    defaults = agent._resolve_provider()
    assert defaults.wire == "openai-chat"
    assert defaults.api_key_env == "OPENCODE_API_KEY"
    # Harbor's provider table has no opencode-go entry, so passthrough leaves
    # the resolved env without the key. The adapter reads it from the host env.
    assert "secret" not in dict(agent.model_connection.env).values()


def test_a_prefix_option_override_reaches_the_defaults(tmp_path: Path) -> None:
    agent = ReagentAgent(
        logs_dir=tmp_path,
        model_name="openai/gpt-5",
        extra_env={"OPENAI_API_KEY": "secret"},
        wire="openai-responses",
        base_url="http://localhost:9000",
        api_key_env="OPENAI_API_KEY",
    )
    defaults = agent._resolve_provider()
    assert defaults.wire == "openai-responses"
    assert defaults.base_url == "http://localhost:9000"
    assert defaults.api_key_env == "OPENAI_API_KEY"


def test_max_tokens_option_reaches_the_command() -> None:
    command = build_command(
        workdir="/app",
        model_id="kimi-k2.6",
        task_env="REAGENT_TASK_TEST",
        logs_dir=PurePosixPath("/logs/agent"),
        timeout_sec=None,
        max_tokens=16000,
    )
    assert "--max-tokens 16000 " in command
    assert ReagentOptions(max_tokens=16000).max_tokens == 16000


def test_max_tokens_flag_is_absent_by_default() -> None:
    command = build_command(
        workdir="/app",
        model_id="kimi-k2.6",
        task_env="REAGENT_TASK_TEST",
        logs_dir=PurePosixPath("/logs/agent"),
        timeout_sec=None,
    )
    assert "--max-tokens" not in command
    assert ReagentOptions().max_tokens is None
