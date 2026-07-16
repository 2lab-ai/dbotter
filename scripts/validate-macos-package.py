#!/usr/bin/env python3
"""Fail-closed verifier for one signed native dbotter preview app package."""

from __future__ import annotations

import argparse
import hashlib
import json
import pathlib
import plistlib
import re
import struct
import subprocess
import sys
import tarfile
import tempfile
from typing import Any


SOURCE_SHA_RE = re.compile(r"^[0-9a-f]{40}$")
SHA256_RE = re.compile(r"^[0-9a-f]{64}$")
TAG_RE = re.compile(
    r"^preview-(?P<year>[0-9]{4})-(?P<month>[0-9]{2})-(?P<day>[0-9]{2})-"
    r"(?P<clock>[0-9]{6})-(?P<run>[1-9][0-9]*)-(?P<attempt>[1-9][0-9]*)-"
    r"(?P<short>[0-9a-f]{12})$"
)
DESCRIPTOR_KEYS = {"schema", "manifest", "artifact"}
MANIFEST_KEYS = {
    "tag",
    "source_sha",
    "version",
    "package_version",
    "config_contract",
    "run_id",
    "run_attempt",
    "created_at",
}
ARTIFACT_KEYS = {
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
RECEIPT_KEYS = {
    "schema",
    "tag",
    "source_sha",
    "target",
    "arch",
    "unsigned_executable_sha256",
    "post_sign_executable_sha256",
    "archive_sha256",
    "archive_bytes",
    "bundle_id",
    "bundle_short_version",
    "bundle_build_version",
    "icon",
    "signing",
    "identity",
    "config_contract",
    "build",
    "ax_identifiers",
    "manifest",
}
IDENTITY_KEYS = {"package_version", "channel", "build_id", "source_sha", "target", "arch"}
CONFIG = {
    "read_versions": [1, 2, 3],
    "write_version": 3,
    "migration_backup_suffixes": {"1": ".v1.bak", "2": ".v2.bak"},
}
ICON_MEMBERS = {
    "icon_16x16.png": 16,
    "icon_16x16@2x.png": 32,
    "icon_32x32.png": 32,
    "icon_32x32@2x.png": 64,
    "icon_128x128.png": 128,
    "icon_128x128@2x.png": 256,
    "icon_256x256.png": 256,
    "icon_256x256@2x.png": 512,
    "icon_512x512.png": 512,
    "icon_512x512@2x.png": 1024,
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


def load_json(path: pathlib.Path, location: str) -> Any:
    if path.is_symlink() or not path.is_file():
        raise ContractError(f"{location} must be a regular file, not a link")
    try:
        with path.open("r", encoding="utf-8") as handle:
            return json.load(handle, object_pairs_hook=object_without_duplicate_keys)
    except (OSError, UnicodeError, json.JSONDecodeError) as error:
        raise ContractError(f"{location} is not valid readable JSON: {error}") from error


def exact_object(value: Any, keys: set[str], location: str) -> dict[str, Any]:
    if not isinstance(value, dict) or set(value) != keys:
        actual = set(value) if isinstance(value, dict) else set()
        raise ContractError(
            f"{location} has wrong fields; missing={sorted(keys - actual)}, "
            f"extra={sorted(actual - keys)}"
        )
    return value


def exact_config_contract(value: Any, location: str) -> dict[str, Any]:
    config = exact_object(value, set(CONFIG), location)
    read_versions = config["read_versions"]
    backup_suffixes = exact_object(
        config["migration_backup_suffixes"],
        set(CONFIG["migration_backup_suffixes"]),
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
        raise ContractError(f"{location} is not the exact typed contract")
    return config


def sha256_bytes(value: bytes) -> str:
    return hashlib.sha256(value).hexdigest()


def sha256_path(path: pathlib.Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        while chunk := handle.read(1024 * 1024):
            digest.update(chunk)
    return digest.hexdigest()


def run_json(command: list[str], location: str) -> dict[str, Any]:
    try:
        result = subprocess.run(command, check=True, capture_output=True, text=True)
        value = json.loads(result.stdout, object_pairs_hook=object_without_duplicate_keys)
    except (OSError, subprocess.CalledProcessError, UnicodeError, json.JSONDecodeError) as error:
        raise ContractError(f"{location} command did not return exact JSON: {error}") from error
    return value


def png_dimensions(path: pathlib.Path) -> tuple[int, int]:
    data = path.read_bytes()
    if len(data) < 26 or data[:8] != b"\x89PNG\r\n\x1a\n" or data[12:16] != b"IHDR":
        raise ContractError(f"decoded icon member is not PNG: {path}")
    return struct.unpack(">II", data[16:24])


def validate(args: argparse.Namespace) -> None:
    if not SOURCE_SHA_RE.fullmatch(args.expected_source_sha):
        raise ContractError("expected source SHA is invalid")
    tag_match = TAG_RE.fullmatch(args.expected_tag)
    if tag_match is None or tag_match["short"] != args.expected_source_sha[:12]:
        raise ContractError("expected tag is invalid or disagrees with source")
    if args.app.is_symlink() or not args.app.is_dir() or args.app.name != "Dbotter Preview.app":
        raise ContractError("app must be a real Dbotter Preview.app directory")
    for path, location in ((args.archive, "archive"), (args.descriptor, "descriptor"), (args.receipt, "receipt")):
        if path.is_symlink() or not path.is_file():
            raise ContractError(f"{location} must be a regular file, not a link")

    descriptor = exact_object(load_json(args.descriptor, "descriptor"), DESCRIPTOR_KEYS, "descriptor")
    if descriptor["schema"] != "dbotter.preview-artifact.v1":
        raise ContractError("descriptor schema is wrong")
    manifest = exact_object(descriptor["manifest"], MANIFEST_KEYS, "descriptor.manifest")
    artifact = exact_object(descriptor["artifact"], ARTIFACT_KEYS, "descriptor.artifact")
    receipt = exact_object(load_json(args.receipt, "receipt"), RECEIPT_KEYS, "receipt")
    if receipt["schema"] != "dbotter.package-receipt.v1":
        raise ContractError("receipt schema is wrong")

    embedded = args.app / "Contents" / "MacOS" / "dbotter"
    info_path = args.app / "Contents" / "Info.plist"
    icon_path = args.app / "Contents" / "Resources" / "dbotter.icns"
    for path, location in ((embedded, "embedded executable"), (info_path, "Info.plist"), (icon_path, "ICNS")):
        if path.is_symlink() or not path.is_file():
            raise ContractError(f"{location} must be a regular file, not a link")

    identity = exact_object(run_json([str(embedded), "version", "--format", "json"], "identity"), IDENTITY_KEYS, "identity")
    config = exact_config_contract(
        run_json([str(embedded), "config-contract", "--format", "json"], "config contract"),
        "config contract",
    )
    if identity["channel"] != "preview" or identity["source_sha"] != args.expected_source_sha:
        raise ContractError("embedded executable is not bound to expected preview source")
    if f"preview-{identity['build_id']}" != args.expected_tag:
        raise ContractError("embedded executable build id disagrees with expected tag")
    target = identity["target"]
    arch = identity["arch"]
    target_arch = {
        "aarch64-apple-darwin": ("aarch64", "arm64"),
        "x86_64-apple-darwin": ("x86_64", "x86_64"),
    }
    if target not in target_arch or arch != target_arch[target][0]:
        raise ContractError("embedded macOS target and architecture are not approved")
    try:
        lipo_arch = subprocess.run(
            ["lipo", "-archs", str(embedded)], check=True, capture_output=True, text=True
        ).stdout.strip()
    except (OSError, subprocess.CalledProcessError) as error:
        raise ContractError(f"could not inspect embedded Mach-O architecture: {error}") from error
    if lipo_arch != target_arch[target][1]:
        raise ContractError("embedded Mach-O bytes disagree with identity architecture")
    try:
        subprocess.run(
            ["codesign", "--verify", "--deep", "--strict", str(args.app)],
            check=True,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
    except (OSError, subprocess.CalledProcessError) as error:
        raise ContractError(f"app code signature does not verify: {error}") from error

    package_version = identity["package_version"]
    build_match = re.fullmatch(
        r"([0-9]{4})-([0-9]{2})-([0-9]{2})-([0-9]{6})-([1-9][0-9]*)-"
        r"([1-9][0-9]*)-([0-9a-f]{12})",
        identity["build_id"],
    )
    if build_match is None:
        raise ContractError("embedded build id is invalid")
    year, month, day, clock, run_id, run_attempt, short_sha = build_match.groups()
    if short_sha != args.expected_source_sha[:12]:
        raise ContractError("embedded build id short SHA disagrees with source")
    version = f"{year}.{month}.{day}.{clock}.{run_id}.{run_attempt}"
    created_at = f"{year}-{month}-{day}T{clock[:2]}:{clock[2:4]}:{clock[4:]}Z"
    bundle_build_version = f"{run_id}.{run_attempt}"

    try:
        with info_path.open("rb") as handle:
            info = plistlib.load(handle)
    except (OSError, plistlib.InvalidFileException) as error:
        raise ContractError(f"Info.plist is invalid: {error}") from error
    expected_info = {
        "CFBundleIdentifier": "ai.2lab.dbotter.preview",
        "CFBundleExecutable": "dbotter",
        "CFBundleIconFile": "dbotter.icns",
        "CFBundleShortVersionString": package_version,
        "CFBundleVersion": bundle_build_version,
    }
    for key, expected in expected_info.items():
        if info.get(key) != expected:
            raise ContractError(f"Info.plist {key} disagrees with package identity")

    archive_sha256 = sha256_path(args.archive)
    archive_bytes = args.archive.stat().st_size
    embedded_sha256 = sha256_path(embedded)
    icns_sha256 = sha256_path(icon_path)
    expected_url = (
        f"https://github.com/2lab-ai/dbotter/releases/download/{args.expected_tag}/"
        f"dbotter-preview-{arch}.tar.gz"
    )
    manifest_url = (
        f"https://github.com/2lab-ai/dbotter/releases/download/{args.expected_tag}/"
        "preview-manifest.json"
    )
    expected_manifest = {
        "tag": args.expected_tag,
        "source_sha": args.expected_source_sha,
        "version": version,
        "package_version": package_version,
        "config_contract": CONFIG,
        "run_id": int(run_id),
        "run_attempt": int(run_attempt),
        "created_at": created_at,
    }
    if manifest != expected_manifest:
        raise ContractError("descriptor manifest metadata disagrees with embedded identity")
    expected_artifact = {
        "target": target,
        "arch": arch,
        "kind": "macos-app-tar-gz",
        "url": expected_url,
        "bytes": archive_bytes,
        "sha256": archive_sha256,
        "embedded_executable_sha256": embedded_sha256,
        "bundle_id": "ai.2lab.dbotter.preview",
        "bundle_short_version": package_version,
        "bundle_build_version": bundle_build_version,
    }
    if artifact != expected_artifact:
        raise ContractError("descriptor artifact disagrees with final package bytes")

    required_archive_members = {
        "Dbotter Preview.app/Contents/MacOS/dbotter": embedded,
        "Dbotter Preview.app/Contents/Info.plist": info_path,
        "Dbotter Preview.app/Contents/Resources/dbotter.icns": icon_path,
    }
    try:
        with tarfile.open(args.archive, "r:gz") as archive:
            members = archive.getmembers()
            for member in members:
                member_path = pathlib.PurePosixPath(member.name)
                if member_path.is_absolute() or ".." in member_path.parts or member.issym() or member.islnk():
                    raise ContractError("archive contains an unsafe path or link")
            by_name = {member.name: member for member in members}
            for name, package_path in required_archive_members.items():
                member = by_name.get(name)
                if member is None or not member.isreg():
                    raise ContractError(f"archive is missing regular package member: {name}")
                extracted = archive.extractfile(member)
                if extracted is None or sha256_bytes(extracted.read()) != sha256_path(package_path):
                    raise ContractError(f"archive member bytes disagree with app: {name}")
    except (OSError, tarfile.TarError) as error:
        raise ContractError(f"archive is unreadable: {error}") from error

    icon_source = pathlib.Path(__file__).resolve().parent.parent / "assets" / "dbotter-icon.png"
    icon_source_sha256 = sha256_path(icon_source)
    if icon_source_sha256 != "5548922d61e5d3bc0dda0abe795e8dd77afda63a763c5482815e262d718559bd":
        raise ContractError("approved icon source hash changed")
    with tempfile.TemporaryDirectory(prefix="dbotter-icns-verify.") as temporary:
        decoded = pathlib.Path(temporary) / "decoded.iconset"
        try:
            subprocess.run(
                ["iconutil", "-c", "iconset", str(icon_path), "-o", str(decoded)],
                check=True,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
            )
        except (OSError, subprocess.CalledProcessError) as error:
            raise ContractError(f"generated ICNS cannot be decoded by iconutil: {error}") from error
        actual_members = {path.name for path in decoded.iterdir()}
        if actual_members != set(ICON_MEMBERS):
            raise ContractError("decoded ICNS member inventory is not exact")
        for name, size in ICON_MEMBERS.items():
            if png_dimensions(decoded / name) != (size, size):
                raise ContractError(f"decoded ICNS member has wrong dimensions: {name}")

    signing = exact_object(receipt["signing"], {"identity", "verified"}, "receipt.signing")
    if not isinstance(signing["identity"], str) or not signing["identity"] or signing["verified"] is not True:
        raise ContractError("receipt signing evidence is invalid")
    icon = exact_object(
        receipt["icon"], {"source", "source_sha256", "icns_sha256"}, "receipt.icon"
    )
    if icon != {
        "source": "assets/dbotter-icon.png",
        "source_sha256": icon_source_sha256,
        "icns_sha256": icns_sha256,
    }:
        raise ContractError("receipt icon evidence disagrees with actual bytes")
    build = exact_object(
        receipt["build"],
        {"profile", "features", "locked", "rustc_version", "cargo_version", "cargo_lock_sha256"},
        "receipt.build",
    )
    root = pathlib.Path(__file__).resolve().parent.parent
    if build != {
        "profile": "release",
        "features": ["all"],
        "locked": True,
        "rustc_version": subprocess.run(["rustc", "--version"], check=True, capture_output=True, text=True).stdout.strip(),
        "cargo_version": subprocess.run(["cargo", "--version"], check=True, capture_output=True, text=True).stdout.strip(),
        "cargo_lock_sha256": sha256_path(root / "Cargo.lock"),
    }:
        raise ContractError("receipt build provenance disagrees with packaging environment")
    unsigned_sha256 = receipt["unsigned_executable_sha256"]
    if not isinstance(unsigned_sha256, str) or not SHA256_RE.fullmatch(unsigned_sha256):
        raise ContractError("receipt unsigned executable digest is invalid")
    if unsigned_sha256 == embedded_sha256:
        raise ContractError("receipt conflates unsigned and post-sign executable bytes")
    ax_inventory_path = root / "packaging" / "macos" / "stable-ax-identifiers.json"
    ax_inventory = load_json(ax_inventory_path, "stable AX inventory")
    if (
        not isinstance(ax_inventory, list)
        or not ax_inventory
        or not all(isinstance(value, str) and value for value in ax_inventory)
        or len(ax_inventory) != len(set(ax_inventory))
    ):
        raise ContractError("tracked stable AX inventory is invalid")
    if receipt["ax_identifiers"] != ax_inventory:
        raise ContractError("receipt stable AX inventory is not exact")
    manifest_link = exact_object(
        receipt["manifest"], {"url", "artifact_target", "artifact_url"}, "receipt.manifest"
    )
    if manifest_link != {
        "url": manifest_url,
        "artifact_target": target,
        "artifact_url": expected_url,
    }:
        raise ContractError("receipt manifest link disagrees with package artifact")
    expected_receipt = {
        "schema": "dbotter.package-receipt.v1",
        "tag": args.expected_tag,
        "source_sha": args.expected_source_sha,
        "target": target,
        "arch": arch,
        "unsigned_executable_sha256": unsigned_sha256,
        "post_sign_executable_sha256": embedded_sha256,
        "archive_sha256": archive_sha256,
        "archive_bytes": archive_bytes,
        "bundle_id": "ai.2lab.dbotter.preview",
        "bundle_short_version": package_version,
        "bundle_build_version": bundle_build_version,
        "icon": icon,
        "signing": signing,
        "identity": identity,
        "config_contract": config,
        "build": build,
        "ax_identifiers": ax_inventory,
        "manifest": manifest_link,
    }
    if receipt != expected_receipt:
        raise ContractError("package receipt is not exact or disagrees with final bytes")


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--app", type=pathlib.Path, required=True)
    parser.add_argument("--archive", type=pathlib.Path, required=True)
    parser.add_argument("--descriptor", type=pathlib.Path, required=True)
    parser.add_argument("--receipt", type=pathlib.Path, required=True)
    parser.add_argument("--expected-source-sha", required=True)
    parser.add_argument("--expected-tag", required=True)
    args = parser.parse_args()
    try:
        validate(args)
    except (ContractError, OSError, subprocess.CalledProcessError) as error:
        print(f"macOS package validation: {error}", file=sys.stderr)
        return 1
    print("macOS package validation: ok")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
