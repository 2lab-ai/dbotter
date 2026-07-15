#!/usr/bin/env python3
"""Validate a completed tap dispatch against final manifest and formula bytes."""

from __future__ import annotations

import argparse
import hashlib
import json
import pathlib
import re
import subprocess
import sys
from typing import Any


SHA1_RE = re.compile(r"^[0-9a-f]{40}$")
SHA256_RE = re.compile(r"^[0-9a-f]{64}$")
TAG_RE = re.compile(
    r"^preview-[0-9]{4}-[0-9]{2}-[0-9]{2}-[0-9]{6}-[1-9][0-9]*-"
    r"[1-9][0-9]*-[0-9a-f]{12}$"
)
VERSION_RE = re.compile(
    r"^[0-9]{4}\.[0-9]{2}\.[0-9]{2}\.[0-9]{6}\.[1-9][0-9]*\.[1-9][0-9]*$"
)
PROOF_KEYS = {"schema", "dispatch", "tap", "workflow"}
DISPATCH_KEYS = {"tag", "source_sha", "version", "manifest_url", "manifest_sha256"}
TAP_KEYS = {"repository", "formula_commit", "formula_blob", "formula_sha256"}
WORKFLOW_KEYS = {"run_id", "run_attempt"}
TARGETS = {
    "aarch64-apple-darwin",
    "x86_64-apple-darwin",
    "aarch64-unknown-linux-gnu",
    "x86_64-unknown-linux-gnu",
}


class ContractError(ValueError):
    pass


def object_without_duplicate_keys(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
    value: dict[str, Any] = {}
    for key, item in pairs:
        if key in value:
            raise ContractError(f"duplicate JSON key: {key}")
        value[key] = item
    return value


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


def exact_string(value: Any, pattern: re.Pattern[str], location: str) -> str:
    if not isinstance(value, str) or pattern.fullmatch(value) is None:
        raise ContractError(f"{location} has an invalid value")
    return value


def positive_integer(value: Any, location: str) -> int:
    if type(value) is not int or value < 1:
        raise ContractError(f"{location} must be a positive integer")
    return value


def sha256_bytes(value: bytes) -> str:
    return hashlib.sha256(value).hexdigest()


def git_blob_sha1(value: bytes) -> str:
    header = f"blob {len(value)}\0".encode("ascii")
    return hashlib.sha1(header + value).hexdigest()


def render_expected_formula(
    *,
    tag: str,
    source_sha: str,
    version: str,
    manifest_url: str,
    manifest_sha256: str,
    arm_url: str,
    arm_sha256: str,
    intel_url: str,
    intel_sha256: str,
) -> str:
    return f'''class DbotterPreview < Formula
  desc "Local Rust database client for MySQL and Redis (preview channel)"
  homepage "https://github.com/2lab-ai/dbotter"
  version "{version}"
  license "Apache-2.0"

  # Immutable release identity:
  # tag: {tag}
  # source: {source_sha}
  # manifest: {manifest_url}
  # manifest-sha256: {manifest_sha256}

  depends_on :macos

  on_macos do
    on_arm do
      url "{arm_url}"
      sha256 "{arm_sha256}"
    end
    on_intel do
      url "{intel_url}"
      sha256 "{intel_sha256}"
    end
  end

  link_overwrite "bin/dbotter"

  def install
    prefix.install "Dbotter Preview.app"
    bin.install_symlink prefix/"Dbotter Preview.app/Contents/MacOS/dbotter" => "dbotter"
  end

  test do
    assert_predicate prefix/"Dbotter Preview.app", :directory?
    assert_predicate prefix/"Dbotter Preview.app/Contents/MacOS/dbotter", :executable?
    assert_match "preview", shell_output("#{{bin}}/dbotter --version")
    shell_output("#{{bin}}/dbotter drivers")
  end
end
'''


def validate(args: argparse.Namespace) -> None:
    expected_tag = exact_string(args.expected_tag, TAG_RE, "expected tag")
    expected_source_sha = exact_string(args.expected_source_sha, SHA1_RE, "expected source SHA")
    expected_version = exact_string(args.expected_version, VERSION_RE, "expected version")
    immutable_manifest_url = (
        "https://github.com/2lab-ai/dbotter/releases/download/"
        f"{expected_tag}/preview-manifest.json"
    )
    if args.expected_manifest_url != immutable_manifest_url:
        raise ContractError("expected manifest URL is not the exact immutable GitHub release URL")
    expected_manifest_sha256 = exact_string(
        args.expected_manifest_sha256, SHA256_RE, "expected manifest digest"
    )
    expected_formula_commit = exact_string(
        args.expected_formula_commit, SHA1_RE, "expected formula commit"
    )
    expected_formula_blob = exact_string(
        args.expected_formula_blob, SHA1_RE, "expected formula blob"
    )
    expected_run_id = positive_integer(args.expected_workflow_run_id, "expected workflow run id")
    expected_run_attempt = positive_integer(
        args.expected_workflow_run_attempt, "expected workflow run attempt"
    )

    proof = exact_object(load_json(args.proof, "proof"), PROOF_KEYS, "proof")
    if proof["schema"] != "dbotter.tap-dispatch.v1":
        raise ContractError("proof schema is not dbotter.tap-dispatch.v1")
    dispatch = exact_object(proof["dispatch"], DISPATCH_KEYS, "proof.dispatch")
    tap = exact_object(proof["tap"], TAP_KEYS, "proof.tap")
    workflow = exact_object(proof["workflow"], WORKFLOW_KEYS, "proof.workflow")
    if dispatch != {
        "tag": expected_tag,
        "source_sha": expected_source_sha,
        "version": expected_version,
        "manifest_url": args.expected_manifest_url,
        "manifest_sha256": expected_manifest_sha256,
    }:
        raise ContractError("proof dispatch tuple disagrees with the exact expected five inputs")
    if tap["repository"] != "2lab-ai/homebrew-tap":
        raise ContractError("proof tap repository is not 2lab-ai/homebrew-tap")
    if tap["formula_commit"] != expected_formula_commit:
        raise ContractError("proof formula commit disagrees with the independently fetched commit")
    if tap["formula_blob"] != expected_formula_blob:
        raise ContractError("proof formula blob disagrees with the independently fetched blob")
    exact_string(tap["formula_sha256"], SHA256_RE, "proof.tap.formula_sha256")
    if workflow != {"run_id": expected_run_id, "run_attempt": expected_run_attempt}:
        raise ContractError("proof workflow identity disagrees with the completed dispatch run")

    manifest_bytes = args.manifest.read_bytes()
    if sha256_bytes(manifest_bytes) != expected_manifest_sha256:
        raise ContractError("downloaded manifest bytes disagree with the dispatched digest")
    manifest = exact_object(load_json(args.manifest, "manifest"), {
        "tag",
        "source_sha",
        "version",
        "package_version",
        "config_contract",
        "run_id",
        "run_attempt",
        "created_at",
        "artifacts",
    }, "manifest")
    validator = pathlib.Path(__file__).resolve().parent / "validate-preview-manifest.py"
    try:
        subprocess.run(
            [
                sys.executable,
                str(validator),
                str(args.manifest),
                "--expected-source-sha",
                expected_source_sha,
                "--expected-tag",
                expected_tag,
            ],
            check=True,
            stdout=subprocess.DEVNULL,
        )
    except subprocess.CalledProcessError as error:
        raise ContractError("downloaded manifest failed the four-target contract") from error
    if manifest["version"] != expected_version:
        raise ContractError("downloaded manifest version disagrees with dispatch")
    artifacts = manifest["artifacts"]
    if not isinstance(artifacts, list):
        raise ContractError("manifest artifacts is not an array")
    by_target = {
        artifact.get("target"): artifact for artifact in artifacts if isinstance(artifact, dict)
    }
    if set(by_target) != TARGETS:
        raise ContractError("manifest target inventory is not exact")

    if args.formula.is_symlink() or not args.formula.is_file():
        raise ContractError("formula must be a regular file, not a link")
    try:
        formula_bytes = args.formula.read_bytes()
        formula_text = formula_bytes.decode("utf-8")
    except (OSError, UnicodeError) as error:
        raise ContractError(f"formula is not readable UTF-8: {error}") from error
    if "\r" in formula_text or not formula_text.endswith("\n"):
        raise ContractError("formula must use canonical LF lines and end with one newline")
    if sha256_bytes(formula_bytes) != tap["formula_sha256"]:
        raise ContractError("formula SHA-256 disagrees with proof")
    if git_blob_sha1(formula_bytes) != expected_formula_blob:
        raise ContractError("formula bytes disagree with the Git blob identity")

    arm = by_target["aarch64-apple-darwin"]
    intel = by_target["x86_64-apple-darwin"]
    expected_formula = render_expected_formula(
        tag=expected_tag,
        source_sha=expected_source_sha,
        version=expected_version,
        manifest_url=immutable_manifest_url,
        manifest_sha256=expected_manifest_sha256,
        arm_url=arm["url"],
        arm_sha256=arm["sha256"],
        intel_url=intel["url"],
        intel_sha256=intel["sha256"],
    )
    if formula_text != expected_formula:
        raise ContractError("formula bytes are not the exact canonical four-target rendering")


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--proof", type=pathlib.Path, required=True)
    parser.add_argument("--manifest", type=pathlib.Path, required=True)
    parser.add_argument("--formula", type=pathlib.Path, required=True)
    parser.add_argument("--expected-tag", required=True)
    parser.add_argument("--expected-source-sha", required=True)
    parser.add_argument("--expected-version", required=True)
    parser.add_argument("--expected-manifest-url", required=True)
    parser.add_argument("--expected-manifest-sha256", required=True)
    parser.add_argument("--expected-formula-commit", required=True)
    parser.add_argument("--expected-formula-blob", required=True)
    parser.add_argument("--expected-workflow-run-id", type=int, required=True)
    parser.add_argument("--expected-workflow-run-attempt", type=int, required=True)
    args = parser.parse_args()
    try:
        validate(args)
    except (ContractError, OSError) as error:
        print(f"tap dispatch: {error}", file=sys.stderr)
        return 1
    print("tap dispatch: ok")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
