from __future__ import annotations

from decimal import Decimal

import pytest

from openrouter_video_studio.widgets import format_elapsed, format_money


@pytest.mark.parametrize(
    ("seconds", "expected"),
    [
        (-3, "0:00:00"),
        (0.99, "0:00:00"),
        (65.8, "0:01:05"),
        (3_661, "1:01:01"),
    ],
)
def test_format_elapsed_uses_whole_non_negative_seconds(
    seconds: float, expected: str
) -> None:
    assert format_elapsed(seconds) == expected


@pytest.mark.parametrize(
    ("amount", "expected"),
    [
        (None, "Estimate unavailable"),
        (Decimal("0"), "$0.00 USD"),
        (Decimal("0.0025"), "$0.0025 USD"),
        (Decimal("0.85"), "$0.85 USD"),
        ("12.345", "$12.34 USD"),
    ],
)
def test_format_money_is_consistent_and_human_readable(
    amount: Decimal | str | None, expected: str
) -> None:
    assert format_money(amount) == expected
