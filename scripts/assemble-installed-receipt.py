#!/usr/bin/env python3
"""Assemble one installed receipt only from exact, source-linked evidence."""

from __future__ import annotations

import argparse
import datetime as dt
import hashlib
import json
import os
import pathlib
import re
import stat
import subprocess
import sys
import tempfile
from typing import Any

from live_contract import SUITES


SHA256_RE = re.compile(r"^[0-9a-f]{64}$")
GIT_SHA_RE = re.compile(r"^[0-9a-f]{40}$")
LIVE_PROJECT_RE = re.compile(r"^[a-z0-9][a-z0-9-]{0,62}$")
SAFE_REPO_PATH_RE = re.compile(r"^[A-Za-z0-9._/-]+$")
SAFE_ACTION_RE = re.compile(r"^[A-Za-z0-9_.:-]+$")
SAFE_AUTHOR_RE = re.compile(r"^[a-z0-9_.:-]+$")
SAFE_CODE_RE = re.compile(r"^[A-Z0-9_]+$")
URI_SECRET_RE = re.compile(
    r"(?:mysql|mariadb|redis|rediss|mongodb(?:\+srv)?|postgres|postgresql|https?)"
    r"://[^\s/:@]+:[^\s@/]+@",
    re.IGNORECASE,
)
CONFIG_CONTRACT = {
    "read_versions": [1, 2, 3],
    "write_version": 3,
    "migration_backup_suffixes": {"1": ".v1.bak", "2": ".v2.bak"},
}
APP_BUNDLE_ID = "ai.2lab.dbotter.preview"
ICON_SHA256 = "5548922d61e5d3bc0dda0abe795e8dd77afda63a763c5482815e262d718559bd"
NATIVE_AX_DRIVER_SOURCE = "scripts/native-ax-driver.swift"
REQUIRED_AX_IDS = {
    "connection.new",
    "connection.new.mysql",
    "connection.new.redis",
    "connection.mongodb.planned",
    "profile.connection_id",
    "profile.host",
    "profile.redis_tls.ca_file",
    "profile.redis_tls.ca_file.pick",
    "profile.credential.session.keep",
    "profile.credential.session.replace",
    "profile.credential.session.forget",
    "profile.delete.active_warning",
    "editor.target",
    "editor.input",
    "editor.row_limit",
    "editor.timeout",
    "editor.execute",
    "editor.cancel",
    "result.table",
    "result.copy.cell",
    "result.copy.row",
    "result.copy.all",
    "result.export.csv",
    "result.export.tsv",
    "result.export.json",
}


class AssembleError(ValueError):
    pass


def object_without_duplicate_keys(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
    result: dict[str, Any] = {}
    for key, value in pairs:
        if key in result:
            raise AssembleError(f"duplicate JSON key: {key}")
        result[key] = value
    return result


def load_json(path: pathlib.Path, location: str) -> Any:
    try:
        metadata = path.lstat()
        if stat.S_ISLNK(metadata.st_mode) or not stat.S_ISREG(metadata.st_mode):
            raise AssembleError(f"{location} must be a regular file, not a symlink")
        with path.open("r", encoding="utf-8") as handle:
            return json.load(handle, object_pairs_hook=object_without_duplicate_keys)
    except FileNotFoundError as error:
        raise AssembleError(f"{location} does not exist: {path}") from error
    except (OSError, UnicodeError, json.JSONDecodeError) as error:
        raise AssembleError(f"{location} is not readable exact JSON: {error}") from error


def exact(value: Any, keys: set[str], location: str) -> dict[str, Any]:
    if not isinstance(value, dict) or set(value) != keys:
        actual = sorted(value) if isinstance(value, dict) else type(value).__name__
        raise AssembleError(f"{location} has wrong fields: {actual}")
    return value


def require_string(value: Any, location: str) -> str:
    if not isinstance(value, str) or not value or any(ord(char) < 32 for char in value):
        raise AssembleError(f"{location} must be a non-empty safe string")
    return value


def require_sha256(value: Any, location: str) -> str:
    if not isinstance(value, str) or SHA256_RE.fullmatch(value) is None:
        raise AssembleError(f"{location} must be one lowercase SHA-256")
    return value


def require_git_sha(value: Any, location: str) -> str:
    if not isinstance(value, str) or GIT_SHA_RE.fullmatch(value) is None:
        raise AssembleError(f"{location} must be one full lowercase Git SHA")
    return value


def require_positive_integer(value: Any, location: str) -> int:
    if type(value) is not int or value < 1:
        raise AssembleError(f"{location} must be a positive integer")
    return value


def require_nonnegative_integer(value: Any, location: str) -> int:
    if type(value) is not int or value < 0:
        raise AssembleError(f"{location} must be a nonnegative integer")
    return value


def require_timestamp(value: Any, location: str) -> str:
    if not isinstance(value, str):
        raise AssembleError(f"{location} must be a UTC timestamp")
    try:
        dt.datetime.strptime(value, "%Y-%m-%dT%H:%M:%SZ")
    except ValueError as error:
        raise AssembleError(f"{location} is not a real second-precision UTC timestamp") from error
    return value


def require_live_receipt(
    value: Any, source_sha: str, run_id: int, run_attempt: int
) -> dict[str, Any]:
    live = exact(
        value,
        {"schema", "source", "project", "started_at", "finished_at", "suites"},
        "live evidence",
    )
    if live["schema"] != "dbotter.live-contract-receipt.v2":
        raise AssembleError("live evidence schema mismatch")
    source = exact(
        live["source"],
        {"kind", "commit", "run_id", "run_attempt"},
        "live.source",
    )
    if source != {
        "kind": "ci_expected_sha",
        "commit": source_sha,
        "run_id": run_id,
        "run_attempt": run_attempt,
    }:
        raise AssembleError("live evidence does not identify the manifest run")
    project = require_string(live["project"], "live.project")
    if LIVE_PROJECT_RE.fullmatch(project) is None:
        raise AssembleError("live.project is not a stable project identifier")
    live_started = require_timestamp(live["started_at"], "live.started_at")
    live_finished = require_timestamp(live["finished_at"], "live.finished_at")
    if live_finished < live_started:
        raise AssembleError("live evidence timestamps are reversed")

    suites = exact(live["suites"], set(SUITES), "live.suites")
    for suite_name, contract in SUITES.items():
        location = f"live.suites.{suite_name}"
        suite = exact(
            suites[suite_name],
            {"test", "started_at", "finished_at", "cases", "measurements"},
            location,
        )
        if suite["test"] != contract["test"]:
            raise AssembleError(f"{location}.test is not exact")
        suite_started = require_timestamp(suite["started_at"], f"{location}.started_at")
        suite_finished = require_timestamp(suite["finished_at"], f"{location}.finished_at")
        if (
            suite_finished < suite_started
            or suite_started < live_started
            or suite_finished > live_finished
        ):
            raise AssembleError(f"{location} timestamps escape the live receipt")

        cases = suite["cases"]
        if not isinstance(cases, list):
            raise AssembleError(f"{location}.cases must be an array")
        case_ids: list[str] = []
        for index, raw_case in enumerate(cases):
            case_location = f"{location}.cases[{index}]"
            case = exact(raw_case, {"id", "executed", "passed"}, case_location)
            case_id = case["id"]
            if not isinstance(case_id, str):
                raise AssembleError(f"{case_location}.id must be a string")
            executed = require_positive_integer(case["executed"], f"{case_location}.executed")
            passed = require_positive_integer(case["passed"], f"{case_location}.passed")
            if passed != executed:
                raise AssembleError(f"{case_location} did not pass every execution")
            case_ids.append(case_id)
        if len(case_ids) != len(set(case_ids)):
            raise AssembleError(f"{location} contains duplicate case identifiers")
        if case_ids != sorted(case_ids):
            raise AssembleError(f"{location}.cases are not sorted")
        expected_cases: set[str] = contract["cases"]
        if set(case_ids) != expected_cases:
            missing = sorted(expected_cases - set(case_ids))
            unknown = sorted(set(case_ids) - expected_cases)
            raise AssembleError(
                f"{location}.cases are not exact; missing={missing}, unknown={unknown}"
            )

        measurement_contract: dict[str, tuple[int, int | None]] = contract["measurements"]
        measurements = exact(
            suite["measurements"], set(measurement_contract), f"{location}.measurements"
        )
        for name, (minimum, maximum) in measurement_contract.items():
            measured = require_nonnegative_integer(
                measurements[name], f"{location}.measurements.{name}"
            )
            if measured < minimum or (maximum is not None and measured > maximum):
                raise AssembleError(
                    f"{location}.measurements.{name} is outside [{minimum}, {maximum}]"
                )
    return live


def require_config(value: Any, location: str) -> dict[str, Any]:
    config = exact(value, set(CONFIG_CONTRACT), location)
    read_versions = config["read_versions"]
    backup_suffixes = exact(
        config["migration_backup_suffixes"],
        set(CONFIG_CONTRACT["migration_backup_suffixes"]),
        f"{location}.migration_backup_suffixes",
    )
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
        raise AssembleError(f"{location} is not the approved config contract")
    return config


def require_identity(value: Any, location: str) -> dict[str, Any]:
    identity = exact(
        value,
        {"package_version", "channel", "build_id", "source_sha", "target", "arch"},
        location,
    )
    require_string(identity["package_version"], f"{location}.package_version")
    if identity["channel"] != "preview":
        raise AssembleError(f"{location}.channel must be preview")
    require_string(identity["build_id"], f"{location}.build_id")
    require_git_sha(identity["source_sha"], f"{location}.source_sha")
    require_string(identity["target"], f"{location}.target")
    require_string(identity["arch"], f"{location}.arch")
    return identity


def require_boolean_map(
    value: Any, expected: dict[str, bool], location: str
) -> dict[str, bool]:
    assertions = exact(value, set(expected), location)
    if assertions != expected:
        raise AssembleError(f"{location} has a missing or false contract verdict")
    return assertions  # type: ignore[return-value]


def require_safe_ids(
    value: Any, pattern: re.Pattern[str], location: str
) -> list[str]:
    if (
        not isinstance(value, list)
        or not value
        or len(value) != len(set(value))
        or any(not isinstance(item, str) or pattern.fullmatch(item) is None for item in value)
    ):
        raise AssembleError(f"{location} must contain unique safe identifiers")
    return value


def sha256_file(path: pathlib.Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def reject_static_leak(path: pathlib.Path, location: str) -> None:
    content = path.read_text(encoding="utf-8")
    for secret in (
        "dbotter-local-only",
        "root-local-only",
        "dbotter-redis-local-only",
    ):
        if secret in content:
            raise AssembleError(f"{location} contains fixture credential material")
    if URI_SECRET_RE.search(content):
        raise AssembleError(f"{location} contains a credential-bearing URI")


def validate_inputs(args: argparse.Namespace) -> dict[str, Any]:
    root = pathlib.Path(__file__).resolve().parent.parent
    validator = root / "scripts" / "validate-preview-manifest.py"
    result = subprocess.run(
        [str(validator), str(args.manifest)],
        check=False,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.PIPE,
        text=True,
    )
    if result.returncode != 0:
        raise AssembleError(f"manifest validation failed: {result.stderr.strip()}")

    paths = {
        "manifest": args.manifest,
        "source": args.source_evidence,
        "package": args.package_evidence,
        "live": args.live_evidence,
        "cli": args.cli_evidence,
        "gui": args.gui_evidence,
        "p7": args.p7_evidence,
        "formula": args.formula_evidence,
    }
    documents: dict[str, Any] = {}
    for name, path in paths.items():
        documents[name] = load_json(path, f"{name} evidence")
        reject_static_leak(path, f"{name} evidence")
    return documents


def assemble(args: argparse.Namespace, documents: dict[str, Any]) -> dict[str, Any]:
    root = pathlib.Path(__file__).resolve().parent.parent
    manifest = documents["manifest"]
    manifest_sha256 = sha256_file(args.manifest)
    source_sha = require_git_sha(manifest["source_sha"], "manifest.source_sha")
    build_id = require_string(manifest["tag"], "manifest.tag").removeprefix("preview-")
    config_contract = require_config(manifest["config_contract"], "manifest.config_contract")

    source = exact(
        documents["source"],
        {"schema", "source", "build", "started_at", "finished_at", "assertions"},
        "source evidence",
    )
    if source["schema"] != "dbotter.source-verification.v1":
        raise AssembleError("source evidence schema mismatch")
    source_record = exact(
        source["source"],
        {"kind", "commit", "expected_sha", "clean", "run_id", "run_attempt"},
        "source.source",
    )
    source_build = exact(
        source["build"],
        {"profile", "features", "rustc_version", "cargo_version", "identity", "config_contract"},
        "source.build",
    )
    source_identity = require_identity(source_build["identity"], "source.build.identity")
    source_config = require_config(source_build["config_contract"], "source.build.config_contract")
    source_started = require_timestamp(source["started_at"], "source.started_at")
    source_finished = require_timestamp(source["finished_at"], "source.finished_at")
    if source_started > source_finished:
        raise AssembleError("source evidence timestamps are reversed")
    if (
        source_record["kind"] != "ci_expected_sha"
        or source_record["clean"] is not True
        or require_git_sha(source_record["commit"], "source.commit") != source_sha
        or source_record["expected_sha"] != source_sha
        or require_positive_integer(source_record["run_id"], "source.run_id") != manifest["run_id"]
        or require_positive_integer(source_record["run_attempt"], "source.run_attempt") != manifest["run_attempt"]
    ):
        raise AssembleError("source evidence does not identify the manifest run")
    if (
        source_build["profile"] != "release"
        or source_build["features"] != ["desktop", "mongodb"]
        or re.match(r"^rustc [0-9]+\.[0-9]+\.[0-9]+", require_string(source_build["rustc_version"], "source rustc")) is None
        or re.match(r"^cargo [0-9]+\.[0-9]+\.[0-9]+", require_string(source_build["cargo_version"], "source cargo")) is None
        or source_identity["package_version"] != manifest["package_version"]
        or source_identity["build_id"] != build_id
        or source_identity["source_sha"] != source_sha
        or source_config != config_contract
    ):
        raise AssembleError("source build evidence disagrees with the manifest")
    require_boolean_map(
        source["assertions"],
        {
            "source": True,
            "release_contract": True,
            "receipt_contracts": True,
            "format": True,
            "clippy": True,
            "tests": True,
            "identity": True,
            "config_contract": True,
            "overall": True,
        },
        "source.assertions",
    )

    package = exact(
        documents["package"],
        {
            "schema", "tag", "source_sha", "target", "arch",
            "unsigned_executable_sha256", "post_sign_executable_sha256",
            "archive_sha256", "archive_bytes", "bundle_id",
            "bundle_short_version", "bundle_build_version", "icon", "signing",
            "identity", "config_contract",
        },
        "package evidence",
    )
    if package["schema"] != "dbotter.package-receipt.v1":
        raise AssembleError("package evidence schema mismatch")
    package_identity = require_identity(package["identity"], "package.identity")
    package_config = require_config(package["config_contract"], "package.config_contract")
    matching_artifacts = [
        artifact for artifact in manifest["artifacts"] if artifact["target"] == package["target"]
    ]
    if len(matching_artifacts) != 1:
        raise AssembleError("package evidence has no unique manifest artifact")
    artifact = matching_artifacts[0]
    unsigned_sha = require_sha256(package["unsigned_executable_sha256"], "package unsigned hash")
    post_sign_sha = require_sha256(package["post_sign_executable_sha256"], "package post-sign hash")
    archive_sha = require_sha256(package["archive_sha256"], "package archive hash")
    icon = exact(package["icon"], {"source", "sha256"}, "package.icon")
    signing = exact(package["signing"], {"identity", "verified"}, "package.signing")
    if (
        package["tag"] != manifest["tag"]
        or package["source_sha"] != source_sha
        or package["target"] != artifact["target"]
        or package["arch"] != artifact["arch"]
        or package_identity["target"] != artifact["target"]
        or package_identity["arch"] != artifact["arch"]
        or package_identity["package_version"] != manifest["package_version"]
        or package_identity["build_id"] != build_id
        or package_identity["source_sha"] != source_sha
        or package_config != config_contract
        or post_sign_sha != artifact["embedded_executable_sha256"]
        or archive_sha != artifact["sha256"]
        or package["archive_bytes"] != artifact["bytes"]
        or unsigned_sha in {post_sign_sha, archive_sha}
        or post_sign_sha == archive_sha
        or package["bundle_id"] != APP_BUNDLE_ID
        or package["bundle_short_version"] != manifest["package_version"]
        or package["bundle_build_version"] != f'{manifest["run_id"]}.{manifest["run_attempt"]}'
        or icon != {"source": "assets/dbotter-icon.png", "sha256": ICON_SHA256}
        or signing["verified"] is not True
        or not isinstance(signing["identity"], str)
        or not signing["identity"]
    ):
        raise AssembleError("package evidence disagrees with its signed manifest artifact")

    live = require_live_receipt(
        documents["live"],
        source_sha,
        source_record["run_id"],
        source_record["run_attempt"],
    )
    live_started = live["started_at"]
    live_finished = live["finished_at"]
    if live_started < source_started:
        raise AssembleError("live evidence predates source verification")

    formula = exact(
        documents["formula"],
        {"schema", "repository", "commit", "name", "version", "prefix", "manifest_url", "manifest_sha256", "assertions"},
        "formula evidence",
    )
    manifest_url = f'https://github.com/2lab-ai/dbotter/releases/download/{manifest["tag"]}/preview-manifest.json'
    if (
        formula["schema"] != "dbotter.formula-install-evidence.v1"
        or formula["repository"] != "2lab-ai/homebrew-tap"
        or require_git_sha(formula["commit"], "formula.commit") != formula["commit"]
        or formula["name"] != "dbotter-preview"
        or formula["version"] != manifest["version"]
        or formula["manifest_url"] != manifest_url
        or formula["manifest_sha256"] != manifest_sha256
        or not isinstance(formula["prefix"], str)
        or not formula["prefix"].startswith("/")
        or formula["prefix"].endswith("/")
        or not formula["prefix"].endswith("/opt/dbotter-preview")
    ):
        raise AssembleError("formula evidence disagrees with the immutable release")
    require_boolean_map(
        formula["assertions"],
        {"tap_dispatch": True, "manifest": True, "install": True, "overall": True},
        "formula.assertions",
    )

    cli = exact(
        documents["cli"],
        {"schema", "started_at", "source_sha", "manifest_sha256", "formula", "app", "shim", "identity", "config_contract", "assertions"},
        "CLI evidence",
    )
    if cli["schema"] != "dbotter.installed-cli-evidence.v1":
        raise AssembleError("CLI evidence schema mismatch")
    require_timestamp(cli["started_at"], "cli.started_at")
    cli_formula = exact(cli["formula"], {"name", "version"}, "cli.formula")
    cli_app = exact(
        cli["app"], {"path", "resolved_path", "bundle_id", "executable"}, "cli.app"
    )
    cli_executable = exact(
        cli_app["executable"],
        {"realpath", "device", "inode", "bytes", "sha256", "codesign_valid"},
        "cli.app.executable",
    )
    cli_shim = exact(cli["shim"], {"path", "realpath", "device", "inode", "sha256"}, "cli.shim")
    cli_identity = require_identity(cli["identity"], "cli.identity")
    cli_config = require_config(cli["config_contract"], "cli.config_contract")
    requested_executable_path = f'{formula["prefix"]}/Dbotter Preview.app/Contents/MacOS/dbotter'
    app_path = f'{formula["prefix"]}/Dbotter Preview.app'
    brew_prefix = formula["prefix"].removesuffix("/opt/dbotter-preview")
    expected_shim_path = f"{brew_prefix}/bin/dbotter"
    resolved_app_path = require_string(cli_app["resolved_path"], "cli.app.resolved_path")
    resolved_executable_path = f"{resolved_app_path}/Contents/MacOS/dbotter"
    if (
        cli["manifest_sha256"] != manifest_sha256
        or cli["source_sha"] != source_sha
        or cli_formula != {"name": "dbotter-preview", "version": manifest["version"]}
        or cli_app["path"] != app_path
        or not resolved_app_path.startswith(f"{brew_prefix}/Cellar/dbotter-preview/")
        or not resolved_app_path.endswith("/Dbotter Preview.app")
        or cli_app["bundle_id"] != APP_BUNDLE_ID
        or cli_executable["realpath"] != resolved_executable_path
        or cli_executable["sha256"] != post_sign_sha
        or cli_executable["codesign_valid"] is not True
        or any(type(cli_executable[name]) is not int or cli_executable[name] < 1 for name in ("device", "inode", "bytes"))
        or cli_shim["path"] != expected_shim_path
        or cli_shim["realpath"] != resolved_executable_path
        or cli_shim["device"] != cli_executable["device"]
        or cli_shim["inode"] != cli_executable["inode"]
        or cli_shim["sha256"] != post_sign_sha
        or cli_identity != package_identity
        or cli_config != config_contract
    ):
        raise AssembleError("installed CLI evidence disagrees with package/formula identity")
    require_boolean_map(
        cli["assertions"],
        {
            "formula": True,
            "app_bundle": True,
            "executable_hash": True,
            "shim_same_executable": True,
            "identity": True,
            "config_contract": True,
            "check": True,
            "exec": True,
            "mysql_browse": True,
            "redis_browse": True,
            "redis_inspect": True,
            "overall": True,
        },
        "cli.assertions",
    )

    gui = exact(
        documents["gui"],
        {"schema", "source_sha", "driver", "app_path", "bundle_id", "pid", "pid_executable", "stale_process_disposition", "ax_identifiers", "assertions"},
        "GUI evidence",
    )
    if gui["schema"] != "dbotter.installed-gui-evidence.v1":
        raise AssembleError("GUI evidence schema mismatch")
    driver = exact(gui["driver"], {"executable_sha256", "source_repo_path", "source_sha256"}, "gui.driver")
    require_sha256(driver["executable_sha256"], "gui.driver.executable_sha256")
    require_sha256(driver["source_sha256"], "gui.driver.source_sha256")
    source_repo_path = require_string(driver["source_repo_path"], "gui.driver.source_repo_path")
    if source_repo_path != NATIVE_AX_DRIVER_SOURCE:
        raise AssembleError("GUI evidence does not name the canonical native AX driver source")
    if (
        SAFE_REPO_PATH_RE.fullmatch(source_repo_path) is None
        or source_repo_path.startswith("/")
        or ".." in pathlib.PurePosixPath(source_repo_path).parts
    ):
        raise AssembleError("GUI driver source path is not a safe repository path")
    driver_source_path = root / source_repo_path
    try:
        driver_source_metadata = driver_source_path.lstat()
    except OSError as error:
        raise AssembleError("reviewed GUI driver source is unavailable") from error
    if stat.S_ISLNK(driver_source_metadata.st_mode) or not stat.S_ISREG(driver_source_metadata.st_mode):
        raise AssembleError("reviewed GUI driver source is not a regular tracked file")
    tracked = subprocess.run(
        ["git", "ls-files", "--error-unmatch", "--", source_repo_path],
        cwd=root,
        check=False,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    if tracked.returncode != 0 or sha256_file(driver_source_path) != driver["source_sha256"]:
        raise AssembleError("reviewed GUI driver source provenance disagrees with the repository")
    pid_executable = exact(gui["pid_executable"], {"realpath", "device", "inode", "sha256"}, "gui.pid_executable")
    author_ids = require_safe_ids(gui["ax_identifiers"], SAFE_AUTHOR_RE, "gui.ax_identifiers")
    if not REQUIRED_AX_IDS.issubset(author_ids):
        raise AssembleError("GUI evidence omits a required AXIdentifier")
    if (
        gui["source_sha"] != source_sha
        or gui["app_path"] != app_path
        or gui["bundle_id"] != APP_BUNDLE_ID
        or require_positive_integer(gui["pid"], "gui.pid") != gui["pid"]
        or gui["stale_process_disposition"] not in {"none", "terminated"}
        or pid_executable["realpath"] != resolved_executable_path
        or pid_executable["device"] != cli_executable["device"]
        or pid_executable["inode"] != cli_executable["inode"]
        or pid_executable["sha256"] != post_sign_sha
    ):
        raise AssembleError("installed GUI PID evidence disagrees with the exact app")
    gui_assertions = require_boolean_map(
        gui["assertions"],
        {
            "accessibility": True,
            "ax_identifier_readback": True,
            "clipboard_contracts": True,
            "contrast": True,
            "create_recovery": True,
            "disclosure": True,
            "export_contracts": True,
            "overall": True,
            "pid_identity": True,
            "recovery_totality": True,
            "session_intents": True,
            "stale_process_handled": True,
            "tls_split_recovery": True,
        },
        "gui.assertions",
    )

    p7 = exact(
        documents["p7"],
        {"schema", "action_ids", "public_codes", "external_export_verifier", "assertions"},
        "P7 evidence",
    )
    if p7["schema"] != "dbotter.p7-installed-evidence.v1":
        raise AssembleError("P7 evidence schema mismatch")
    action_ids = require_safe_ids(p7["action_ids"], SAFE_ACTION_RE, "p7.action_ids")
    public_codes = require_safe_ids(p7["public_codes"], SAFE_CODE_RE, "p7.public_codes")
    export_verdicts = p7["external_export_verifier"]
    if not isinstance(export_verdicts, list) or len(export_verdicts) != 3:
        raise AssembleError("P7 evidence must contain exactly three external export verdicts")
    if sorted(item.get("fixture_id") for item in export_verdicts if isinstance(item, dict)) != ["seeded.csv", "seeded.json", "seeded.tsv"]:
        raise AssembleError("P7 export fixture inventory is incomplete")
    for index, verdict in enumerate(export_verdicts):
        exact(verdict, {"fixture_id", "expected_sha256", "actual_sha256", "verdict"}, f"p7.export[{index}]")
        expected_digest = require_sha256(verdict["expected_sha256"], f"p7.export[{index}].expected")
        actual_digest = require_sha256(verdict["actual_sha256"], f"p7.export[{index}].actual")
        if expected_digest != actual_digest or verdict["verdict"] is not True:
            raise AssembleError("P7 external export byte verdict failed")
    p7_assertions = require_boolean_map(
        p7["assertions"],
        {
            "clipboard": True,
            "export": True,
            "disclosure": True,
            "recovery_totality": True,
            "credential_leak": False,
            "user_content_leak": False,
            "overall": True,
        },
        "p7.assertions",
    )

    finished_at = require_timestamp(args.finished_at, "--finished-at")
    if finished_at < max(source_finished, live_finished, cli["started_at"]):
        raise AssembleError("--finished-at predates required evidence")

    return {
        "schema": "dbotter.installed-receipt.v1",
        "started_at": source_started,
        "finished_at": finished_at,
        "source": {
            "kind": source_record["kind"],
            "commit": source_record["commit"],
            "expected_sha": source_record["expected_sha"],
            "run_id": source_record["run_id"],
            "run_attempt": source_record["run_attempt"],
        },
        "build": {
            "target": package["target"],
            "arch": package["arch"],
            "profile": source_build["profile"],
            "features": source_build["features"],
            "rustc_version": source_build["rustc_version"],
            "cargo_version": source_build["cargo_version"],
            "unsigned_executable_sha256": unsigned_sha,
        },
        "release": {
            "tag": manifest["tag"],
            "manifest_url": manifest_url,
            "manifest_sha256": manifest_sha256,
            "version": manifest["version"],
        },
        "formula": {
            "repository": formula["repository"],
            "commit": formula["commit"],
            "name": formula["name"],
            "version": formula["version"],
            "prefix": formula["prefix"],
        },
        "install": {
            "requested_app_path": app_path,
            "resolved_app_path": resolved_app_path,
            "bundle_id": APP_BUNDLE_ID,
            "arch": package["arch"],
            "executable": {
                "path": requested_executable_path,
                "realpath": cli_executable["realpath"],
                "device": cli_executable["device"],
                "inode": cli_executable["inode"],
                "bytes": cli_executable["bytes"],
                "sha256": cli_executable["sha256"],
                "codesign_valid": cli_executable["codesign_valid"],
            },
            "cli_shim": cli_shim,
        },
        "identity": cli_identity,
        "config_contract": cli_config,
        "checks": {
            "version": True,
            "config_contract": True,
            "shim_identity": True,
            "bundle_identity": True,
            "executable_hash": True,
            "codesign": True,
            "check": True,
            "exec": True,
            "mysql_browse": True,
            "redis_browse": True,
            "redis_inspect": True,
        },
        "ax": {
            "app_path": gui["app_path"],
            "pid": gui["pid"],
            "stale_process_disposition": gui["stale_process_disposition"],
            "pid_executable": pid_executable,
            "driver": driver,
            "author_ids": author_ids,
            "action_ids": action_ids,
            "public_codes": public_codes,
        },
        "external_export_verifier": export_verdicts,
        "assertions": {
            "source_match": True,
            "build_match": True,
            "manifest_valid": True,
            "release_match": True,
            "formula_match": True,
            "app_path_exact": True,
            "pid_identity": True,
            "identity_exact": True,
            "config_contract_exact": True,
            "shim_same_executable": True,
            "executable_hash_match": True,
            "codesign_valid": True,
            "cli_contracts": True,
            "live_contracts": True,
            "accessibility": gui_assertions["accessibility"],
            "contrast": gui_assertions["contrast"],
            "recovery_totality": gui_assertions["recovery_totality"] and p7_assertions["recovery_totality"],
            "clipboard": gui_assertions["clipboard_contracts"] and p7_assertions["clipboard"],
            "disclosure": gui_assertions["disclosure"] and p7_assertions["disclosure"],
            "export": gui_assertions["export_contracts"] and p7_assertions["export"],
            "credential_leak": p7_assertions["credential_leak"],
            "user_content_leak": p7_assertions["user_content_leak"],
            "overall": True,
        },
    }


def write_validated(
    receipt: dict[str, Any], manifest: pathlib.Path, output: pathlib.Path
) -> None:
    if output.exists() or output.is_symlink():
        raise AssembleError(f"refusing to replace installed receipt: {output}")
    output.parent.mkdir(parents=True, exist_ok=True)
    descriptor, temporary_name = tempfile.mkstemp(
        prefix=".installed-receipt.", suffix=".json", dir=output.parent
    )
    temporary = pathlib.Path(temporary_name)
    try:
        with os.fdopen(descriptor, "w", encoding="utf-8") as handle:
            json.dump(receipt, handle, indent=2, sort_keys=True)
            handle.write("\n")
        os.chmod(temporary, 0o600)
        reject_static_leak(temporary, "assembled installed receipt")
        root = pathlib.Path(__file__).resolve().parent.parent
        checker = root / "scripts" / "check-installed-receipt-contract.sh"
        result = subprocess.run(
            [str(checker), "--manifest", str(manifest), str(temporary)],
            check=False,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.PIPE,
            text=True,
        )
        if result.returncode != 0:
            raise AssembleError(f"assembled receipt failed validation: {result.stderr.strip()}")
        try:
            os.link(temporary, output)
        except FileExistsError as error:
            raise AssembleError(f"refusing to replace installed receipt: {output}") from error
    finally:
        temporary.unlink(missing_ok=True)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--manifest", required=True, type=pathlib.Path)
    parser.add_argument("--source-evidence", required=True, type=pathlib.Path)
    parser.add_argument("--package-evidence", required=True, type=pathlib.Path)
    parser.add_argument("--live-evidence", required=True, type=pathlib.Path)
    parser.add_argument("--cli-evidence", required=True, type=pathlib.Path)
    parser.add_argument("--gui-evidence", required=True, type=pathlib.Path)
    parser.add_argument("--p7-evidence", required=True, type=pathlib.Path)
    parser.add_argument("--formula-evidence", required=True, type=pathlib.Path)
    parser.add_argument("--finished-at", required=True)
    parser.add_argument("--output", required=True, type=pathlib.Path)
    args = parser.parse_args()
    try:
        documents = validate_inputs(args)
        receipt = assemble(args, documents)
        write_validated(receipt, args.manifest, args.output)
    except (AssembleError, OSError) as error:
        print(f"installed receipt assembler: {error}", file=sys.stderr)
        return 1
    print(f"installed receipt assembler: ok: {args.output}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
