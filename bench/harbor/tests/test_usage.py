"""Unit tests for reagent JSON event parsing.

These tests feed ``tests/fixtures/events.jsonl`` — a real reagent ``--json``
output captured with the fake testing provider — into the parser and assert the
token and turn counts, plus the malformed-line handling. No LLM call and no
container is needed.
"""

from pathlib import Path

import pytest

from reagent_harbor.agent import parse_events

FIXTURE = Path(__file__).parent / "fixtures" / "events.jsonl"


def test_parses_recorded_reagent_events() -> None:
    """The recorded event file yields the expected token and turn totals."""
    summary = parse_events(FIXTURE.read_text())

    assert summary.input_tokens == 90
    assert summary.output_tokens == 26
    assert summary.cache_read_tokens == 165
    assert summary.cache_write_tokens == 15
    assert summary.turns == 3
    assert summary.malformed_lines == 0
    # Harbor counts input as prompt-side tokens including the cache.
    assert summary.total_tokens == 90 + 165 + 15


def test_empty_stream_yields_zeros() -> None:
    summary = parse_events("")
    assert summary.input_tokens == 0
    assert summary.output_tokens == 0
    assert summary.cache_read_tokens == 0
    assert summary.cache_write_tokens == 0
    assert summary.turns == 0
    assert summary.malformed_lines == 0


def test_malformed_lines_are_skipped_and_counted() -> None:
    """A torn line, a non-JSON line, and a non-dict JSON value must never crash."""
    output = "\n".join(
        [
            '{"type":"turn_start"}',
            "this is not json {",
            '[1, 2, 3]',
            "",  # blank line: ignored, not malformed
            '{"type":"summaress"',  # truncated JSON
            '{"type":"summary","input":10,"output":2,"cache_read":4,'
            '"cache_write":1,"turns":1}',
        ]
    )
    summary = parse_events(output)

    assert summary.input_tokens == 10
    assert summary.output_tokens == 2
    assert summary.cache_read_tokens == 4
    assert summary.cache_write_tokens == 1
    assert summary.turns == 1
    assert summary.malformed_lines == 3


def test_missing_summary_falls_back_to_usage_lines() -> None:
    """Without a summary, totals are summed from per-event usage lines."""
    output = "\n".join(
        [
            '{"type":"turn_start"}',
            '{"type":"usage","input":25,"output":8,"cache_read":40,'
            '"cache_write":12}',
            '{"type":"turn_start"}',
            '{"type":"usage","input":35,"output":12,"cache_read":70,'
            '"cache_write":0}',
        ]
    )
    summary = parse_events(output)

    assert summary.input_tokens == 60
    assert summary.output_tokens == 20
    assert summary.cache_read_tokens == 110
    assert summary.cache_write_tokens == 12
    assert summary.turns == 2


def test_negative_or_missing_counts_are_clamped_to_zero() -> None:
    output = '{"type":"summary","input":-5,"output":3,"cache_read":0,"turns":-1}'
    summary = parse_events(output)
    assert summary.input_tokens == 0
    assert summary.output_tokens == 3
    assert summary.cache_read_tokens == 0
    assert summary.cache_write_tokens == 0
    assert summary.turns == 0
