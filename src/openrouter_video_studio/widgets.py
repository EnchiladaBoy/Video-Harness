"""Reusable widgets for the OpenRouter Video Studio terminal UI."""

from __future__ import annotations

from datetime import timedelta
from decimal import Decimal
import json
import os
import sys
from typing import Any

from rich.text import Text
from textual.reactive import reactive
from textual.widgets import Static


def format_elapsed(seconds: float) -> str:
    """Format an elapsed duration without showing distracting fractions."""

    return str(timedelta(seconds=max(0, int(seconds))))


def format_money(value: Decimal | int | float | str | None) -> str:
    """Return a friendly USD value, preserving useful sub-cent precision."""

    if value is None:
        return "Estimate unavailable"
    amount = Decimal(str(value))
    if amount == 0:
        return "$0.00 USD"
    places = 4 if abs(amount) < Decimal("0.01") else 2
    return f"${amount:.{places}f} USD"


class CloudCinema(Static):
    """A lightweight animated cloud cinema that never implies fake progress."""

    DEFAULT_CSS = """
    CloudCinema {
        height: 12;
        min-height: 8;
        content-align: center middle;
        text-align: center;
    }
    """

    phase: reactive[int] = reactive(0)
    status: reactive[str] = reactive("Preparing")
    detail: reactive[str] = reactive("Warming up the projector…")
    elapsed_seconds: reactive[float] = reactive(0.0)
    countdown: reactive[int | None] = reactive(None)

    _stars = ("✦", "·", "⋆", "✧", "·", "✶")
    _clouds = ("☁", "☁", "☁", "☁")
    _reels = ("◜", "◝", "◞", "◟")

    def on_mount(self) -> None:
        self.set_interval(0.24, self._tick)

    def _tick(self) -> None:
        self.phase += 1
        self.elapsed_seconds += 0.24
        self.refresh()

    def set_job_state(
        self,
        status: str,
        detail: str | None = None,
        *,
        countdown: int | None = None,
    ) -> None:
        """Update real job state separately from the decorative animation."""

        self.status = status.replace("_", " ").title()
        if detail:
            self.detail = detail
        self.countdown = countdown
        self.refresh()

    def render(self) -> Text:
        width = max(36, min(72, self.size.width - 4 if self.size.width else 56))
        sky_width = width - 2
        encoding = (getattr(sys.stdout, "encoding", None) or "").lower()
        ascii_only = (
            os.environ.get("TERM", "").lower() == "dumb"
            or "utf" not in encoding
            or bool(os.environ.get("OPENROUTER_VIDEO_ASCII"))
        )
        stars_source = ("*", ".", "+", ".", "*", ".") if ascii_only else self._stars
        clouds_source = ("(cloud)",) * 4 if ascii_only else self._clouds
        stars = [" "] * sky_width
        for index in range(6):
            position = (index * 11 + self.phase * (1 if index % 2 else -1)) % sky_width
            stars[position] = stars_source[(self.phase + index) % len(stars_source)]
        sky = "".join(stars)

        cloud_line = [" "] * sky_width
        for index, cloud in enumerate(clouds_source):
            position = (index * 17 + self.phase // (index + 2)) % sky_width
            if len(cloud) == 1:
                cloud_line[position] = cloud
            else:
                for offset, character in enumerate(cloud):
                    cloud_line[(position + offset) % sky_width] = character
        clouds = "".join(cloud_line)

        reel = self._reels[self.phase % len(self._reels)] if not ascii_only else "o"
        cinema = (
            f"  {reel}O\\   +-------------+   /O{reel}  "
            if ascii_only
            else f"  {reel}◉╲   ┌─────────────┐   ╱◉{reel}  "
        )
        cinema = cinema.center(sky_width)
        beam = (
            "\\      tiny cloud cinema      /"
            if ascii_only
            else "╲      tiny cloud cinema      ╱"
        ).center(sky_width)
        ground_char = "-" if ascii_only else "─"
        ground = (ground_char * min(sky_width, 50)).center(sky_width)

        next_poll = ""
        if self.countdown is not None:
            next_poll = f"  •  checking again in {max(0, self.countdown)}s"

        output = Text()
        output.append(sky + "\n", style="bright_magenta")
        output.append(clouds + "\n", style="bright_cyan")
        output.append(cinema + "\n", style="bold bright_white")
        output.append(beam + "\n", style="cyan")
        output.append(ground + "\n", style="bright_magenta")
        output.append(f"{self.status}  ", style="bold bright_cyan")
        output.append(self.detail, style="white")
        output.append(
            f"\nElapsed {format_elapsed(self.elapsed_seconds)}{next_poll}",
            style="dim",
        )
        return output


class StatusPill(Static):
    """Compact status label used by onboarding and model loading states."""

    kind: reactive[str] = reactive("info")

    def set(self, message: str, kind: str = "info") -> None:
        self.kind = kind
        self.update(message)
        self.set_class(kind == "error", "error")
        self.set_class(kind == "success", "success")
        self.set_class(kind == "warning", "warning")


class CostSummary(Static):
    """Render exact and approximate estimates without overstating precision."""

    def show_estimate(self, estimate: Any | None) -> None:
        if estimate is None or getattr(estimate, "amount", None) is None:
            detail = getattr(estimate, "basis", "Pricing was not supplied by this model.")
            raw = getattr(estimate, "raw_pricing", None)
            raw_note = ""
            if raw:
                prices = {str(key): str(value) for key, value in dict(raw).items()}
                raw_note = "\nAdvertised pricing: " + json.dumps(prices, sort_keys=True)
            self.update(
                f"[bold yellow]Cost estimate unavailable[/]\n[dim]{detail}{raw_note}[/]"
            )
            self.set_class(True, "unknown")
            return

        amount = format_money(getattr(estimate, "amount"))
        exact = bool(getattr(estimate, "exact", False))
        qualifier = "Estimated cost" if exact else "Approximate cost"
        basis = getattr(estimate, "basis", "") or "Based on current model pricing"
        self.update(f"[bold bright_cyan]{qualifier}: {amount}[/]\n[dim]{basis}[/]")
        self.set_class(False, "unknown")
