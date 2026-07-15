#!/usr/bin/env python3
"""Validate installed-receipt timestamps as real canonical UTC seconds."""

from __future__ import annotations

import argparse
import datetime as dt
import json
import pathlib
import sys
from typing import Any


class TimestampError(ValueError):
    pass


def object_without_duplicate_keys(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
    result: dict[str, Any] = {}
    for key, value in pairs:
        if key in result:
            raise TimestampError(f"duplicate JSON key: {key}")
        result[key] = value
    return result


def parse_timestamp(value: Any, field: str) -> dt.datetime:
    if not isinstance(value, str):
        raise TimestampError(f"{field} must be a string")
    try:
        parsed = dt.datetime.strptime(value, "%Y-%m-%dT%H:%M:%SZ")
    except ValueError as error:
        raise TimestampError(f"{field} is not a real canonical UTC second") from error
    if parsed.strftime("%Y-%m-%dT%H:%M:%SZ") != value:
        raise TimestampError(f"{field} is not canonical")
    return parsed.replace(tzinfo=dt.timezone.utc)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("receipt", type=pathlib.Path)
    args = parser.parse_args()
    try:
        with args.receipt.open("r", encoding="utf-8") as handle:
            document = json.load(handle, object_pairs_hook=object_without_duplicate_keys)
        if not isinstance(document, dict):
            raise TimestampError("receipt must be an object")
        started = parse_timestamp(document.get("started_at"), "started_at")
        finished = parse_timestamp(document.get("finished_at"), "finished_at")
        if finished < started:
            raise TimestampError("finished_at precedes started_at")
    except (FileNotFoundError, OSError, json.JSONDecodeError, TimestampError) as error:
        print(f"installed receipt timestamp: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
