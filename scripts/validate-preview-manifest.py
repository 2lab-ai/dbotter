#!/usr/bin/env python3
"""Fail-closed validator for dbotter.preview-manifest.v1."""

from __future__ import annotations

import argparse
import datetime as dt
import json
import pathlib
import re
import sys
import urllib.parse
from typing import Any


TAG_RE = re.compile(
    r"^preview-(?P<year>[0-9]{4})-(?P<month>[0-9]{2})-(?P<day>[0-9]{2})-"
    r"(?P<time>[0-9]{6})-(?P<run>[1-9][0-9]*)-(?P<attempt>[1-9][0-9]*)-"
    r"(?P<sha>[0-9a-f]{12})$"
)
VERSION_RE = re.compile(
    r"^(?P<year>[0-9]{4})\.(?P<month>[0-9]{2})\.(?P<day>[0-9]{2})\."
    r"(?P<time>[0-9]{6})\.(?P<run>[1-9][0-9]*)\.(?P<attempt>[1-9][0-9]*)$"
)
LEGACY_BASELINE_RE = re.compile(
    r"^(?P<year>[0-9]{4})\.(?P<month>[0-9]{2})\.(?P<day>[0-9]{2})\."
    r"(?P<time>[0-9]{4})$"
)
SEMVER_RE = re.compile(r"^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$")
SHA256_RE = re.compile(r"^[0-9a-f]{64}$")
SOURCE_SHA_RE = re.compile(r"^[0-9a-f]{40}$")
CREATED_AT_RE = re.compile(r"^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}Z$")

TOP_LEVEL_KEYS = {
    "tag",
    "source_sha",
    "version",
    "package_version",
    "config_contract",
    "run_id",
    "run_attempt",
    "created_at",
    "artifacts",
}
CONFIG_KEYS = {"read_versions", "write_version", "migration_backup_suffixes"}
MIGRATION_BACKUP_SUFFIX_KEYS = {"1", "2"}
MACOS_ARTIFACT_KEYS = {
    "target",
    "arch",
    "kind",
    "url",
    "bytes",
    "sha256",
    "embedded_executable_sha256",
    "bundle_id",
    "bundle_short_version",
    "bundle_build_version",
}
LINUX_ARTIFACT_KEYS = {
    "target",
    "arch",
    "kind",
    "url",
    "bytes",
    "sha256",
    "executable_mode",
}
TARGET_SPECS = {
    "aarch64-apple-darwin": ("aarch64", "macos-app-tar-gz", "dbotter-preview-aarch64.tar.gz"),
    "x86_64-apple-darwin": ("x86_64", "macos-app-tar-gz", "dbotter-preview-x86_64.tar.gz"),
    "aarch64-unknown-linux-gnu": (
        "aarch64",
        "linux-native-executable",
        "dbotter-preview-linux-aarch64",
    ),
    "x86_64-unknown-linux-gnu": (
        "x86_64",
        "linux-native-executable",
        "dbotter-preview-linux-x86_64",
    ),
}


class ContractError(ValueError):
    pass


def object_without_duplicate_keys(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
    result: dict[str, Any] = {}
    for key, value in pairs:
        if key in result:
            raise ContractError(f"duplicate JSON key: {key}")
        result[key] = value
    return result


def load_json(path: pathlib.Path) -> Any:
    try:
        with path.open("r", encoding="utf-8") as handle:
            return json.load(handle, object_pairs_hook=object_without_duplicate_keys)
    except FileNotFoundError as error:
        raise ContractError(f"manifest does not exist: {path}") from error
    except (OSError, json.JSONDecodeError) as error:
        raise ContractError(f"manifest is not valid readable JSON: {error}") from error


def require_exact_object(value: Any, keys: set[str], location: str) -> dict[str, Any]:
    if not isinstance(value, dict):
        raise ContractError(f"{location} must be an object")
    actual = set(value)
    if actual != keys:
        missing = sorted(keys - actual)
        extra = sorted(actual - keys)
        raise ContractError(f"{location} has wrong fields; missing={missing}, extra={extra}")
    return value


def require_string(value: Any, pattern: re.Pattern[str], location: str) -> str:
    if not isinstance(value, str) or pattern.fullmatch(value) is None:
        raise ContractError(f"{location} has an invalid value")
    return value


def require_positive_integer(value: Any, location: str) -> int:
    if type(value) is not int or value < 1:
        raise ContractError(f"{location} must be a positive integer")
    return value


def require_config_contract(value: Any, location: str) -> dict[str, Any]:
    config = require_exact_object(value, CONFIG_KEYS, location)
    read_versions = config["read_versions"]
    backup_suffixes = require_exact_object(
        config["migration_backup_suffixes"],
        MIGRATION_BACKUP_SUFFIX_KEYS,
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
        raise ContractError(f"{location} is not the exact typed three-field contract")
    return config


def version_tuple(value: str) -> tuple[int, ...]:
    match = VERSION_RE.fullmatch(value)
    if match is None:
        raise ContractError("version has an invalid format")
    return tuple(int(match[name]) for name in ("year", "month", "day", "time", "run", "attempt"))


def baseline_version_tuple(value: str) -> tuple[int, ...]:
    match = VERSION_RE.fullmatch(value)
    if match is not None:
        stamp = (
            f'{match["year"]}-{match["month"]}-{match["day"]}T'
            f'{match["time"][0:2]}:{match["time"][2:4]}:{match["time"][4:6]}Z'
        )
        try:
            dt.datetime.strptime(stamp, "%Y-%m-%dT%H:%M:%SZ")
        except ValueError as error:
            raise ContractError("current version baseline is not a real UTC timestamp") from error
        return version_tuple(value)
    legacy = LEGACY_BASELINE_RE.fullmatch(value)
    if legacy is None:
        raise ContractError("current version baseline has an invalid format")
    stamp = (
        f'{legacy["year"]}-{legacy["month"]}-{legacy["day"]}T'
        f'{legacy["time"][0:2]}:{legacy["time"][2:4]}:00Z'
    )
    try:
        dt.datetime.strptime(stamp, "%Y-%m-%dT%H:%M:%SZ")
    except ValueError as error:
        raise ContractError("current version baseline is not a real UTC minute") from error
    return (
        int(legacy["year"]),
        int(legacy["month"]),
        int(legacy["day"]),
        int(f'{legacy["time"]}00'),
        0,
        0,
    )


def validate_manifest(
    document: Any,
    *,
    expected_source_sha: str | None = None,
    expected_tag: str | None = None,
    greater_than: str | None = None,
) -> None:
    manifest = require_exact_object(document, TOP_LEVEL_KEYS, "manifest")
    tag = require_string(manifest["tag"], TAG_RE, "tag")
    tag_match = TAG_RE.fullmatch(tag)
    if tag_match is None:
        raise ContractError("tag has an invalid format")
    source_sha = require_string(manifest["source_sha"], SOURCE_SHA_RE, "source_sha")
    version = require_string(manifest["version"], VERSION_RE, "version")
    version_match = VERSION_RE.fullmatch(version)
    if version_match is None:
        raise ContractError("version has an invalid format")
    package_version = require_string(
        manifest["package_version"], SEMVER_RE, "package_version"
    )
    run_id = require_positive_integer(manifest["run_id"], "run_id")
    run_attempt = require_positive_integer(manifest["run_attempt"], "run_attempt")
    created_at = require_string(manifest["created_at"], CREATED_AT_RE, "created_at")
    try:
        dt.datetime.strptime(created_at, "%Y-%m-%dT%H:%M:%SZ")
    except ValueError as error:
        raise ContractError("created_at is not a real UTC timestamp") from error

    tag_components = tuple(
        tag_match[name] for name in ("year", "month", "day", "time", "run", "attempt")
    )
    version_components = tuple(
        version_match[name] for name in ("year", "month", "day", "time", "run", "attempt")
    )
    tag_created_at = (
        f'{tag_match["year"]}-{tag_match["month"]}-{tag_match["day"]}T'
        f'{tag_match["time"][0:2]}:{tag_match["time"][2:4]}:{tag_match["time"][4:6]}Z'
    )
    try:
        dt.datetime.strptime(tag_created_at, "%Y-%m-%dT%H:%M:%SZ")
    except ValueError as error:
        raise ContractError("tag/version stamp is not a real UTC timestamp") from error
    if created_at != tag_created_at:
        raise ContractError("created_at must exactly equal the tag/version UTC stamp")
    if tag_components != version_components:
        raise ContractError("tag and Homebrew version do not describe the same run")
    if int(tag_match["run"]) != run_id or int(tag_match["attempt"]) != run_attempt:
        raise ContractError("tag/version run tuple disagrees with run_id/run_attempt")
    if tag_match["sha"] != source_sha[:12]:
        raise ContractError("tag short SHA disagrees with source_sha")
    if expected_source_sha is not None and source_sha != expected_source_sha:
        raise ContractError("manifest source_sha disagrees with expected source")
    if expected_tag is not None and tag != expected_tag:
        raise ContractError("manifest tag disagrees with expected tag")
    if greater_than is not None and version_tuple(version) <= baseline_version_tuple(greater_than):
        raise ContractError("preview version is not strictly greater than the current version")

    require_config_contract(manifest["config_contract"], "config_contract")

    artifacts = manifest["artifacts"]
    if not isinstance(artifacts, list) or len(artifacts) != 4:
        raise ContractError("artifacts must contain exactly the four approved native target records")
    seen_targets: set[str] = set()
    seen_urls: set[str] = set()
    for index, raw_artifact in enumerate(artifacts):
        location = f"artifacts[{index}]"
        if not isinstance(raw_artifact, dict):
            raise ContractError(f"{location} must be an object")
        target = raw_artifact.get("target")
        if target not in TARGET_SPECS:
            raise ContractError(f"{location}.target is not an approved native target")
        expected_arch, expected_kind, expected_name = TARGET_SPECS[target]
        expected_keys = (
            MACOS_ARTIFACT_KEYS
            if expected_kind == "macos-app-tar-gz"
            else LINUX_ARTIFACT_KEYS
        )
        artifact = require_exact_object(raw_artifact, expected_keys, location)
        arch = artifact["arch"]
        if arch != expected_arch:
            raise ContractError(f"{location} swaps target and architecture identity")
        if target in seen_targets:
            raise ContractError("artifact targets must be unique")
        seen_targets.add(target)
        if artifact["kind"] != expected_kind:
            raise ContractError(f"{location}.kind disagrees with its target")
        url = artifact["url"]
        if not isinstance(url, str):
            raise ContractError(f"{location}.url must be a string")
        parsed = urllib.parse.urlsplit(url)
        expected_path = f"/2lab-ai/dbotter/releases/download/{tag}/{expected_name}"
        if (
            parsed.scheme != "https"
            or parsed.netloc != "github.com"
            or parsed.path != expected_path
            or parsed.query
            or parsed.fragment
            or parsed.username is not None
            or parsed.password is not None
        ):
            raise ContractError(f"{location}.url is not the immutable approved release URL")
        if url in seen_urls:
            raise ContractError("artifact URLs must be unique")
        seen_urls.add(url)
        require_positive_integer(artifact["bytes"], f"{location}.bytes")
        asset_hash = require_string(artifact["sha256"], SHA256_RE, f"{location}.sha256")
        if expected_kind == "macos-app-tar-gz":
            executable_hash = require_string(
                artifact["embedded_executable_sha256"],
                SHA256_RE,
                f"{location}.embedded_executable_sha256",
            )
            if asset_hash == executable_hash:
                raise ContractError(
                    f"{location} falsely equates transformed archive/executable bytes"
                )
            if artifact["bundle_id"] != "ai.2lab.dbotter.preview":
                raise ContractError(f"{location}.bundle_id is not the preview bundle id")
            if artifact["bundle_short_version"] != package_version:
                raise ContractError(
                    f"{location}.bundle_short_version must equal package_version"
                )
            expected_build_version = f"{run_id}.{run_attempt}"
            if artifact["bundle_build_version"] != expected_build_version:
                raise ContractError(
                    f"{location}.bundle_build_version must equal the numeric run tuple"
                )
            if (
                artifact["bundle_short_version"] == version
                or artifact["bundle_build_version"] == version
            ):
                raise ContractError(f"{location} conflates bundle and Homebrew versions")
        elif artifact["executable_mode"] != "0755":
            raise ContractError(f"{location}.executable_mode is not 0755")
    if seen_targets != set(TARGET_SPECS):
        raise ContractError("manifest is missing an approved native target")


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("manifest", type=pathlib.Path)
    parser.add_argument("--expected-source-sha")
    parser.add_argument("--expected-tag")
    parser.add_argument("--greater-than")
    args = parser.parse_args()
    try:
        document = load_json(args.manifest)
        validate_manifest(
            document,
            expected_source_sha=args.expected_source_sha,
            expected_tag=args.expected_tag,
            greater_than=args.greater_than,
        )
    except ContractError as error:
        print(f"preview manifest: {error}", file=sys.stderr)
        return 1
    print("preview manifest: ok")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
