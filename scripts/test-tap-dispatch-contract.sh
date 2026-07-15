#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
cd "$ROOT"

fail() {
  echo "tap dispatch contract: $*" >&2
  exit 1
}

sha256_file() {
  if command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | awk '{print $1}'
  else
    sha256sum "$1" | awk '{print $1}'
  fi
}

tmp_dir="$(mktemp -d "${TMPDIR:-/tmp}/dbotter-tap-dispatch.XXXXXX")"
cleanup() {
  rm -rf "$tmp_dir"
}
trap cleanup EXIT HUP INT TERM

manifest="tests/fixtures/release/preview-manifest.valid.json"
manifest_sha256="$(sha256_file "$manifest")"
tag="$(jq -r .tag "$manifest")"
source_sha="$(jq -r .source_sha "$manifest")"
version="$(jq -r .version "$manifest")"
manifest_url="https://github.com/2lab-ai/dbotter/releases/download/$tag/preview-manifest.json"
formula_commit="1111111111111111111111111111111111111111"

arm_url="$(jq -r '.artifacts[] | select(.target == "aarch64-apple-darwin") | .url' "$manifest")"
arm_sha="$(jq -r '.artifacts[] | select(.target == "aarch64-apple-darwin") | .sha256' "$manifest")"
intel_url="$(jq -r '.artifacts[] | select(.target == "x86_64-apple-darwin") | .url' "$manifest")"
intel_sha="$(jq -r '.artifacts[] | select(.target == "x86_64-apple-darwin") | .sha256' "$manifest")"
linux_url="$(jq -r '.artifacts[] | select(.target == "aarch64-unknown-linux-gnu") | .url' "$manifest")"

formula="$tmp_dir/dbotter-preview.rb"
{
  printf 'class DbotterPreview < Formula\n'
  printf '  desc "Local Rust database client for MySQL and Redis (preview channel)"\n'
  printf '  homepage "https://github.com/2lab-ai/dbotter"\n'
  printf '  version "%s"\n' "$version"
  printf '  license "Apache-2.0"\n'
  printf '\n'
  printf '  # Immutable release identity:\n'
  printf '  # tag: %s\n' "$tag"
  printf '  # source: %s\n' "$source_sha"
  printf '  # manifest: %s\n' "$manifest_url"
  printf '  # manifest-sha256: %s\n' "$manifest_sha256"
  printf '\n'
  printf '  depends_on :macos\n'
  printf '\n'
  printf '  on_macos do\n'
  printf '    on_arm do\n'
  printf '      url "%s"\n' "$arm_url"
  printf '      sha256 "%s"\n' "$arm_sha"
  printf '    end\n'
  printf '    on_intel do\n'
  printf '      url "%s"\n' "$intel_url"
  printf '      sha256 "%s"\n' "$intel_sha"
  printf '    end\n'
  printf '  end\n'
  printf '\n'
  printf '  link_overwrite "bin/dbotter"\n'
  printf '\n'
  printf '  def install\n'
  printf '    prefix.install "Dbotter Preview.app"\n'
  printf '    bin.install_symlink prefix/"Dbotter Preview.app/Contents/MacOS/dbotter" => "dbotter"\n'
  printf '  end\n'
  printf '\n'
  printf '  test do\n'
  printf '    assert_predicate prefix/"Dbotter Preview.app", :directory?\n'
  printf '    assert_predicate prefix/"Dbotter Preview.app/Contents/MacOS/dbotter", :executable?\n'
  printf '    assert_match "preview", shell_output("#{bin}/dbotter --version")\n'
  printf '    shell_output("#{bin}/dbotter drivers")\n'
  printf '  end\n'
  printf 'end\n'
} >"$formula"

formula_blob="$(git hash-object "$formula")"
formula_sha256="$(sha256_file "$formula")"
proof="$tmp_dir/dbotter-tap-dispatch.json"
jq -n \
  --arg tag "$tag" \
  --arg source_sha "$source_sha" \
  --arg version "$version" \
  --arg manifest_url "$manifest_url" \
  --arg manifest_sha256 "$manifest_sha256" \
  --arg formula_commit "$formula_commit" \
  --arg formula_blob "$formula_blob" \
  --arg formula_sha256 "$formula_sha256" \
  '{
    schema: "dbotter.tap-dispatch.v1",
    dispatch: {
      tag: $tag,
      source_sha: $source_sha,
      version: $version,
      manifest_url: $manifest_url,
      manifest_sha256: $manifest_sha256
    },
    tap: {
      repository: "2lab-ai/homebrew-tap",
      formula_commit: $formula_commit,
      formula_blob: $formula_blob,
      formula_sha256: $formula_sha256
    },
    workflow: {
      run_id: 9001,
      run_attempt: 2
    }
  }' >"$proof"

validate=(
  ./scripts/validate-tap-dispatch.py
  --proof "$proof"
  --manifest "$manifest"
  --formula "$formula"
  --expected-tag "$tag"
  --expected-source-sha "$source_sha"
  --expected-version "$version"
  --expected-manifest-url "$manifest_url"
  --expected-manifest-sha256 "$manifest_sha256"
  --expected-formula-commit "$formula_commit"
  --expected-formula-blob "$formula_blob"
  --expected-workflow-run-id 9001
  --expected-workflow-run-attempt 2
)
"${validate[@]}" >/dev/null

wrong_commit="$tmp_dir/wrong-commit.json"
jq '.tap.formula_commit = "2222222222222222222222222222222222222222"' "$proof" >"$wrong_commit"
if "${validate[@]/$proof/$wrong_commit}" >/dev/null 2>&1; then
  fail "proof accepted an arbitrary formula commit"
fi

wrong_manifest="$tmp_dir/wrong-manifest.json"
jq '.dispatch.manifest_sha256 = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"' \
  "$proof" >"$wrong_manifest"
if "${validate[@]/$proof/$wrong_manifest}" >/dev/null 2>&1; then
  fail "proof accepted a manifest digest that was not remeasured"
fi

linux_formula="$tmp_dir/linux-formula.rb"
cp "$formula" "$linux_formula"
printf 'url "%s"\n' "$linux_url" >>"$linux_formula"
linux_blob="$(git hash-object "$linux_formula")"
if ./scripts/validate-tap-dispatch.py \
  --proof "$proof" \
  --manifest "$manifest" \
  --formula "$linux_formula" \
  --expected-tag "$tag" \
  --expected-source-sha "$source_sha" \
  --expected-version "$version" \
  --expected-manifest-url "$manifest_url" \
  --expected-manifest-sha256 "$manifest_sha256" \
  --expected-formula-commit "$formula_commit" \
  --expected-formula-blob "$linux_blob" \
  --expected-workflow-run-id 9001 \
  --expected-workflow-run-attempt 2 >/dev/null 2>&1; then
  fail "formula selected a Linux asset"
fi

swapped_formula="$tmp_dir/swapped-formula.rb"
sed \
  -e "s|$arm_url|DBOTTER_ARM_URL|" \
  -e "s|$intel_url|$arm_url|" \
  -e "s|DBOTTER_ARM_URL|$intel_url|" \
  "$formula" >"$swapped_formula"
swapped_blob="$(git hash-object "$swapped_formula")"
if ./scripts/validate-tap-dispatch.py \
  --proof "$proof" \
  --manifest "$manifest" \
  --formula "$swapped_formula" \
  --expected-tag "$tag" \
  --expected-source-sha "$source_sha" \
  --expected-version "$version" \
  --expected-manifest-url "$manifest_url" \
  --expected-manifest-sha256 "$manifest_sha256" \
  --expected-formula-commit "$formula_commit" \
  --expected-formula-blob "$swapped_blob" \
  --expected-workflow-run-id 9001 \
  --expected-workflow-run-attempt 2 >/dev/null 2>&1; then
  fail "formula swapped the arm and Intel artifact mapping"
fi

duplicate_key="$tmp_dir/duplicate-key.json"
sed '1s/{/{"schema":"dbotter.tap-dispatch.v1",/' "$proof" >"$duplicate_key"
if "${validate[@]/$proof/$duplicate_key}" >/dev/null 2>&1; then
  fail "proof accepted a duplicate JSON key"
fi

echo "tap dispatch contract: ok"
