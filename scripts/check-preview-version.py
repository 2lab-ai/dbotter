#!/usr/bin/env python3
"""Validate and compare monotonic dbotter Homebrew preview versions."""

from __future__ import annotations

import argparse
import datetime as dt
import re
import sys


VERSION_RE = re.compile(
    r"^(?P<year>[0-9]{4})\.(?P<month>[0-9]{2})\.(?P<day>[0-9]{2})\."
    r"(?P<time>[0-9]{6})\.(?P<run>[1-9][0-9]*)\.(?P<attempt>[1-9][0-9]*)$"
)


def parse(value: str) -> tuple[int, ...]:
    match = VERSION_RE.fullmatch(value)
    if match is None:
        raise ValueError(f"invalid preview version: {value}")
    stamp = (
        f'{match["year"]}-{match["month"]}-{match["day"]}T'
        f'{match["time"][0:2]}:{match["time"][2:4]}:{match["time"][4:6]}Z'
    )
    dt.datetime.strptime(stamp, "%Y-%m-%dT%H:%M:%SZ")
    return tuple(
        int(match[name]) for name in ("year", "month", "day", "time", "run", "attempt")
    )


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--candidate", required=True)
    parser.add_argument("--greater-than", required=True)
    args = parser.parse_args()
    try:
        if parse(args.candidate) <= parse(args.greater_than):
            raise ValueError("candidate preview version is not strictly increasing")
    except ValueError as error:
        print(f"preview version: {error}", file=sys.stderr)
        return 1
    print("preview version: ok")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
