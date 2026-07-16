#!/usr/bin/env python3
"""Validate the exact typed config contract in an installed receipt."""

from __future__ import annotations

import argparse
import json
import pathlib
import sys
from typing import Any


CONFIG_KEYS = {"read_versions", "write_version", "migration_backup_suffixes"}
MIGRATION_BACKUP_SUFFIX_KEYS = {"1", "2"}


class ContractError(ValueError):
    pass


def object_without_duplicate_keys(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
    result: dict[str, Any] = {}
    for key, value in pairs:
        if key in result:
            raise ContractError(f"duplicate JSON key: {key}")
        result[key] = value
    return result


def require_exact_object(value: Any, keys: set[str], location: str) -> dict[str, Any]:
    if not isinstance(value, dict):
        raise ContractError(f"{location} must be an object")
    actual = set(value)
    if actual != keys:
        missing = sorted(keys - actual)
        extra = sorted(actual - keys)
        raise ContractError(f"{location} has wrong fields; missing={missing}, extra={extra}")
    return value


def require_config_contract(value: Any) -> None:
    config = require_exact_object(value, CONFIG_KEYS, "config_contract")
    backup_suffixes = require_exact_object(
        config["migration_backup_suffixes"],
        MIGRATION_BACKUP_SUFFIX_KEYS,
        "config_contract.migration_backup_suffixes",
    )
    read_versions = config["read_versions"]
    if (
        not isinstance(read_versions, list)
        or len(read_versions) != 3
        or any(type(version) is not int for version in read_versions)
        or read_versions != [1, 2, 3]
        or type(config["write_version"]) is not int
        or config["write_version"] != 3
        or type(backup_suffixes["1"]) is not str
        or backup_suffixes["1"] != ".v1.bak"
        or type(backup_suffixes["2"]) is not str
        or backup_suffixes["2"] != ".v2.bak"
    ):
        raise ContractError("config_contract is not the exact typed three-field contract")


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("receipt", type=pathlib.Path)
    args = parser.parse_args()
    try:
        with args.receipt.open("r", encoding="utf-8") as handle:
            document = json.load(handle, object_pairs_hook=object_without_duplicate_keys)
        if not isinstance(document, dict):
            raise ContractError("receipt must be an object")
        require_config_contract(document.get("config_contract"))
    except (FileNotFoundError, OSError, json.JSONDecodeError, ContractError) as error:
        print(f"installed receipt config contract: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
