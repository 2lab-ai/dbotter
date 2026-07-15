#!/usr/bin/env python3
"""Validate measured live-suite evidence and assemble the source-bound receipt."""

from __future__ import annotations

import argparse
import json
import os
import re
import stat
import sys
import tempfile
from datetime import datetime
from pathlib import Path
from typing import Any


class EvidenceError(ValueError):
    pass


SHA_RE = re.compile(r"^[0-9a-f]{40}$")
PROJECT_RE = re.compile(r"^[a-z0-9][a-z0-9-]{0,62}$")
CREDENTIAL_URI_RE = re.compile(
    r"(?:mysql|mariadb|redis|rediss|mongodb(?:\+srv)?|postgres|postgresql|https?)"
    r"://[^\s/:@]+:[^\s@/]+@",
    re.IGNORECASE,
)
STATIC_SECRETS = {
    "dbotter-local-only",
    "root-local-only",
    "dbotter-redis-local-only",
}

SUITES: dict[str, dict[str, Any]] = {
    "mysql_catalog": {
        "test": "p4_live_catalog_fixture_proves_pages_caps_permissions_and_cli",
        "cases": {
            "mysql.catalog.cli.page1",
            "mysql.catalog.cli.page2",
            "mysql.catalog.column.ordinal_order",
            "mysql.catalog.column.page1",
            "mysql.catalog.column.page2",
            "mysql.catalog.count_cap",
            "mysql.catalog.empty",
            "mysql.catalog.filter.clear_after_metadata_cap",
            "mysql.catalog.filter.narrow_after_count_cap",
            "mysql.catalog.metadata_cap_4mib",
            "mysql.catalog.permission.check_denied",
            "mysql.catalog.permission.execute_denied",
            "mysql.catalog.relation.binary_order",
            "mysql.catalog.relation.page1",
            "mysql.catalog.relation.page2",
            "mysql.catalog.relation.table",
            "mysql.catalog.relation.view",
            "mysql.catalog.schema.scope",
            "mysql.catalog.schema.visibility",
            "mysql.catalog.token.cross_config_rejected",
            "mysql.catalog.token.stale_connection_rejected",
            "mysql.catalog.token.stale_generation_rejected",
            "mysql.catalog.token.tampered_rejected",
        },
        "measurements": {
            "cli_pages": (2, None),
            "column_pages": (2, None),
            "column_rows": (100, None),
            "denied_operations": (2, None),
            "metadata_retained_bytes": (1, 4 * 1024 * 1024),
            "metadata_truncations": (1, None),
            "relation_pages": (2, None),
            "relation_rows": (34, None),
            "retained_relations": (2_000, 2_000),
        },
    },
    "mysql_safety": {
        "test": "live_mysql_safety_receipt",
        "cases": {
            "mysql.auth.environment.available.correct",
            "mysql.auth.environment.available.wrong",
            "mysql.auth.environment.empty",
            "mysql.auth.environment.missing",
            "mysql.auth.recovery",
            "mysql.auth.session.correct",
            "mysql.auth.session.wrong",
            "mysql.execute.mutation",
            "mysql.execute.read",
            "mysql.marker.current_target.absent",
            "mysql.marker.current_target.prepare_only_rejected",
            "mysql.marker.explicit_selection.absent",
            "mysql.marker.explicit_selection.prepare_only_rejected",
            "mysql.marker.explicit_selection.ui_rejected",
            "mysql.prepared_unsupported.no_raw_fallback",
            "mysql.prepared_unsupported.session_retained",
            "mysql.prepared_unsupported.static_recovery",
        },
        "measurements": {
            "auth_failures": (4, None),
            "marker_rows_after": (0, 0),
            "prepared_attempts": (2, None),
            "prepared_unsupported_attempts": (1, None),
            "raw_fallback_attempts": (0, 0),
            "statements_executed": (2, None),
        },
    },
    "redis": {
        "test": "redis_live_receipt",
        "cases": {
            "redis.auth.plaintext.environment.available.correct",
            "redis.auth.plaintext.environment.available.wrong",
            "redis.auth.plaintext.environment.empty",
            "redis.auth.plaintext.environment.missing",
            "redis.auth.plaintext.recovery",
            "redis.auth.plaintext.session.correct",
            "redis.auth.plaintext.session.wrong",
            "redis.auth.tls.environment.available.correct",
            "redis.auth.tls.environment.available.wrong",
            "redis.auth.tls.environment.empty",
            "redis.auth.tls.environment.missing",
            "redis.auth.tls.recovery",
            "redis.auth.tls.session.correct",
            "redis.auth.tls.session.wrong",
            "redis.classifier.no_command",
            "redis.cli.browse",
            "redis.cli.inspect",
            "redis.inspect.truncation_64kib",
            "redis.inspect.ttl.expiring",
            "redis.inspect.ttl.missing",
            "redis.inspect.ttl.persistent",
            "redis.inspect.type.hash",
            "redis.inspect.type.list",
            "redis.inspect.type.set",
            "redis.inspect.type.stream",
            "redis.inspect.type.string",
            "redis.inspect.type.zset",
            "redis.mutation.readback",
            "redis.scan.multiple_pages",
            "redis.scan.oversize_skipped",
            "redis.scan.raw_binary_identity",
            "redis.tls.ca_preserved",
            "redis.tls.host_recovery",
            "redis.tls.wrong_ca.action",
            "redis.tls.wrong_ca.code",
            "redis.tls.wrong_ca.focus_ca",
            "redis.tls.wrong_host.action",
            "redis.tls.wrong_host.code",
            "redis.tls.wrong_host.focus_host",
        },
        "measurements": {
            "auth_failures": (8, None),
            "cli_operations": (2, None),
            "inspect_types": (6, 6),
            "mutation_readbacks": (2, None),
            "plaintext_fallback_attempts": (0, 0),
            "required_tls_attempts": (3, None),
            "scan_pages": (2, None),
            "tls_recovery_attempts": (1, None),
        },
    },
}


def reject_duplicate_key(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
    result: dict[str, Any] = {}
    for key, value in pairs:
        if key in result:
            raise EvidenceError(f"duplicate JSON key: {key}")
        result[key] = value
    return result


def exact(value: Any, keys: set[str], label: str) -> dict[str, Any]:
    if not isinstance(value, dict) or set(value) != keys:
        actual = sorted(value) if isinstance(value, dict) else type(value).__name__
        raise EvidenceError(f"{label} keys are not exact: {actual}")
    return value


def positive_integer(value: Any, label: str) -> int:
    if isinstance(value, bool) or not isinstance(value, int) or value <= 0:
        raise EvidenceError(f"{label} must be a positive integer")
    return value


def nonnegative_integer(value: Any, label: str) -> int:
    if isinstance(value, bool) or not isinstance(value, int) or value < 0:
        raise EvidenceError(f"{label} must be a nonnegative integer")
    return value


def timestamp(value: Any, label: str) -> datetime:
    if not isinstance(value, str) or not re.fullmatch(
        r"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}Z", value
    ):
        raise EvidenceError(f"{label} must be a second-precision UTC timestamp")
    try:
        return datetime.strptime(value, "%Y-%m-%dT%H:%M:%SZ")
    except ValueError as error:
        raise EvidenceError(f"{label} is not a valid timestamp") from error


def scan_strings(value: Any, label: str = "evidence") -> None:
    if isinstance(value, str):
        if value in STATIC_SECRETS or CREDENTIAL_URI_RE.search(value):
            raise EvidenceError(f"{label} contains credential-bearing text")
        return
    if isinstance(value, list):
        for index, item in enumerate(value):
            scan_strings(item, f"{label}[{index}]")
        return
    if isinstance(value, dict):
        for key, item in value.items():
            scan_strings(key, f"{label}.key")
            scan_strings(item, f"{label}.{key}")


def load_document(path: Path, label: str) -> dict[str, Any]:
    try:
        mode = path.lstat().st_mode
    except OSError as error:
        raise EvidenceError(f"cannot stat {label}: {error}") from error
    if stat.S_ISLNK(mode) or not stat.S_ISREG(mode):
        raise EvidenceError(f"{label} must be a regular file, not a symlink")
    try:
        document = json.loads(
            path.read_text(encoding="utf-8"), object_pairs_hook=reject_duplicate_key
        )
    except (OSError, UnicodeError, json.JSONDecodeError) as error:
        raise EvidenceError(f"cannot load {label}: {error}") from error
    scan_strings(document, label)
    if not isinstance(document, dict):
        raise EvidenceError(f"{label} must contain one JSON object")
    return document


def validate_suite(
    document: dict[str, Any],
    suite_name: str,
    source_sha: str,
    run_id: int,
    run_attempt: int,
) -> dict[str, Any]:
    suite = exact(
        document,
        {
            "schema",
            "suite",
            "test",
            "source",
            "started_at",
            "finished_at",
            "cases",
            "measurements",
        },
        f"{suite_name} evidence",
    )
    contract = SUITES[suite_name]
    if (
        suite["schema"] != "dbotter.live-suite-evidence.v1"
        or suite["suite"] != suite_name
        or suite["test"] != contract["test"]
    ):
        raise EvidenceError(f"{suite_name} identity is not exact")

    source = exact(
        suite["source"], {"kind", "commit", "run_id", "run_attempt"}, f"{suite_name}.source"
    )
    if source != {
        "kind": "ci_expected_sha",
        "commit": source_sha,
        "run_id": run_id,
        "run_attempt": run_attempt,
    }:
        raise EvidenceError(f"{suite_name} is not bound to the expected source and run")

    suite_started = timestamp(suite["started_at"], f"{suite_name}.started_at")
    suite_finished = timestamp(suite["finished_at"], f"{suite_name}.finished_at")
    if suite_finished < suite_started:
        raise EvidenceError(f"{suite_name} finished before it started")

    cases = suite["cases"]
    if not isinstance(cases, list):
        raise EvidenceError(f"{suite_name}.cases must be an array")
    ids: list[str] = []
    for index, raw_case in enumerate(cases):
        case = exact(raw_case, {"id", "executed", "passed"}, f"{suite_name}.cases[{index}]")
        case_id = case["id"]
        if not isinstance(case_id, str):
            raise EvidenceError(f"{suite_name}.cases[{index}].id must be a string")
        executed = positive_integer(case["executed"], f"{case_id}.executed")
        passed = positive_integer(case["passed"], f"{case_id}.passed")
        if passed != executed:
            raise EvidenceError(f"{case_id} did not pass every measured execution")
        ids.append(case_id)
    if len(ids) != len(set(ids)):
        raise EvidenceError(f"{suite_name} contains duplicate case identifiers")
    if ids != sorted(ids):
        raise EvidenceError(f"{suite_name} cases must be sorted by identifier")
    if set(ids) != contract["cases"]:
        missing = sorted(contract["cases"] - set(ids))
        unknown = sorted(set(ids) - contract["cases"])
        raise EvidenceError(f"{suite_name} case set is not exact; missing={missing}, unknown={unknown}")

    measurement_contract: dict[str, tuple[int, int | None]] = contract["measurements"]
    measurements = exact(
        suite["measurements"], set(measurement_contract), f"{suite_name}.measurements"
    )
    for name, (minimum, maximum) in measurement_contract.items():
        measured = nonnegative_integer(measurements[name], f"{suite_name}.measurements.{name}")
        if measured < minimum or (maximum is not None and measured > maximum):
            raise EvidenceError(
                f"{suite_name}.measurements.{name} is outside [{minimum}, {maximum}]"
            )

    return {
        "test": suite["test"],
        "started_at": suite["started_at"],
        "finished_at": suite["finished_at"],
        "cases": cases,
        "measurements": measurements,
    }


def write_atomic_no_replace(output: Path, document: dict[str, Any]) -> None:
    output_parent = output.parent
    if not output_parent.is_dir():
        raise EvidenceError("output parent must already be a directory")
    try:
        output.lstat()
    except FileNotFoundError:
        pass
    except OSError as error:
        raise EvidenceError(f"cannot inspect output: {error}") from error
    else:
        raise EvidenceError("output already exists; receipts are immutable")
    fd, temporary_name = tempfile.mkstemp(prefix=f".{output.name}.", dir=output_parent)
    temporary = Path(temporary_name)
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as handle:
            json.dump(document, handle, indent=2, sort_keys=True)
            handle.write("\n")
            handle.flush()
            os.fsync(handle.fileno())
        try:
            os.link(temporary, output)
        except FileExistsError as error:
            raise EvidenceError("output appeared during assembly; refusing replacement") from error
        except OSError as error:
            raise EvidenceError(f"cannot publish immutable output: {error}") from error
        temporary.unlink()
        directory_flags = os.O_RDONLY | getattr(os, "O_DIRECTORY", 0)
        directory_fd = os.open(output_parent, directory_flags)
        try:
            os.fsync(directory_fd)
        finally:
            os.close(directory_fd)
    finally:
        try:
            temporary.unlink()
        except FileNotFoundError:
            pass


def parser() -> argparse.ArgumentParser:
    result = argparse.ArgumentParser()
    result.add_argument("--source-sha", required=True)
    result.add_argument("--run-id", required=True, type=int)
    result.add_argument("--run-attempt", required=True, type=int)
    result.add_argument("--project", required=True)
    result.add_argument("--started-at", required=True)
    result.add_argument("--finished-at", required=True)
    result.add_argument("--mysql-catalog", required=True, type=Path)
    result.add_argument("--mysql-safety", required=True, type=Path)
    result.add_argument("--redis", required=True, type=Path)
    result.add_argument("--output", required=True, type=Path)
    return result


def main() -> int:
    args = parser().parse_args()
    try:
        if not SHA_RE.fullmatch(args.source_sha):
            raise EvidenceError("--source-sha must be one lowercase full Git SHA")
        run_id = positive_integer(args.run_id, "--run-id")
        run_attempt = positive_integer(args.run_attempt, "--run-attempt")
        if not PROJECT_RE.fullmatch(args.project):
            raise EvidenceError("--project is not a stable project identifier")
        started_at = timestamp(args.started_at, "--started-at")
        finished_at = timestamp(args.finished_at, "--finished-at")
        if finished_at < started_at:
            raise EvidenceError("receipt finished before it started")

        paths = {
            "mysql_catalog": args.mysql_catalog,
            "mysql_safety": args.mysql_safety,
            "redis": args.redis,
        }
        suites = {
            name: validate_suite(
                load_document(path, f"{name} evidence"),
                name,
                args.source_sha,
                run_id,
                run_attempt,
            )
            for name, path in paths.items()
        }
        for name, suite in suites.items():
            if timestamp(suite["started_at"], f"{name}.started_at") < started_at:
                raise EvidenceError(f"{name} started before the receipt")
            if timestamp(suite["finished_at"], f"{name}.finished_at") > finished_at:
                raise EvidenceError(f"{name} finished after the receipt")

        receipt = {
            "schema": "dbotter.live-contract-receipt.v2",
            "source": {
                "kind": "ci_expected_sha",
                "commit": args.source_sha,
                "run_id": run_id,
                "run_attempt": run_attempt,
            },
            "project": args.project,
            "started_at": args.started_at,
            "finished_at": args.finished_at,
            "suites": suites,
        }
        scan_strings(receipt, "assembled receipt")
        write_atomic_no_replace(args.output, receipt)
    except EvidenceError as error:
        print(f"live evidence assembler: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
