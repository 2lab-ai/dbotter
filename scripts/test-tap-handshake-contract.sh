#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
cd "$ROOT"

fail() {
  echo "tap handshake contract: $*" >&2
  exit 1
}

sha256_file() {
  if command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | awk '{print $1}'
  else
    sha256sum "$1" | awk '{print $1}'
  fi
}

tmp_dir="$(mktemp -d "${TMPDIR:-/tmp}/dbotter-tap-handshake.XXXXXX")"
cleanup() {
  rm -rf "$tmp_dir"
}
trap cleanup EXIT HUP INT TERM
mkdir -p "$tmp_dir/bin"

manifest="tests/fixtures/release/preview-manifest.valid.json"
manifest_sha256="$(sha256_file "$manifest")"
tag="$(jq -r .tag "$manifest")"
source_sha="$(jq -r .source_sha "$manifest")"
version="$(jq -r .version "$manifest")"
manifest_url="https://github.com/2lab-ai/dbotter/releases/download/$tag/preview-manifest.json"
formula_commit="1111111111111111111111111111111111111111"
master_commit="2222222222222222222222222222222222222222"

arm_url="$(jq -r '.artifacts[] | select(.target == "aarch64-apple-darwin") | .url' "$manifest")"
arm_sha="$(jq -r '.artifacts[] | select(.target == "aarch64-apple-darwin") | .sha256' "$manifest")"
intel_url="$(jq -r '.artifacts[] | select(.target == "x86_64-apple-darwin") | .url' "$manifest")"
intel_sha="$(jq -r '.artifacts[] | select(.target == "x86_64-apple-darwin") | .sha256' "$manifest")"

formula="$tmp_dir/dbotter-preview.rb"
{
  printf 'class DbotterPreview < Formula\n'
  printf '  desc "Local Rust database client for MySQL and Redis (preview channel)"\n'
  printf '  homepage "https://github.com/2lab-ai/dbotter"\n'
  printf '  version "%s"\n' "$version"
  printf '  license "Apache-2.0"\n\n'
  printf '  # Immutable release identity:\n'
  printf '  # tag: %s\n' "$tag"
  printf '  # source: %s\n' "$source_sha"
  printf '  # manifest: %s\n' "$manifest_url"
  printf '  # manifest-sha256: %s\n\n' "$manifest_sha256"
  printf '  depends_on :macos\n\n'
  printf '  on_macos do\n'
  printf '    on_arm do\n'
  printf '      url "%s"\n' "$arm_url"
  printf '      sha256 "%s"\n' "$arm_sha"
  printf '    end\n'
  printf '    on_intel do\n'
  printf '      url "%s"\n' "$intel_url"
  printf '      sha256 "%s"\n' "$intel_sha"
  printf '    end\n'
  printf '  end\n\n'
  printf '  link_overwrite "bin/dbotter"\n\n'
  printf '  def install\n'
  printf '    prefix.install "Dbotter Preview.app"\n'
  printf '    bin.install_symlink prefix/"Dbotter Preview.app/Contents/MacOS/dbotter" => "dbotter"\n'
  printf '  end\n\n'
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
    workflow: {run_id: 9001, run_attempt: 2}
  }' >"$proof"

fake_gh="$tmp_dir/bin/gh"
cat >"$fake_gh" <<'SH'
#!/usr/bin/env bash
set -euo pipefail

case "$1:$2" in
  workflow:run)
    exit 0
    ;;
  api:repos/2lab-ai/homebrew-tap/actions/workflows/bump.yml/runs?event=workflow_dispatch\&per_page=100)
    jq -n \
      --arg title "dbotter-preview $GH_FAKE_TAG $GH_FAKE_SOURCE_SHA" \
      '{workflow_runs:[{id:9001,display_title:$title,event:"workflow_dispatch",created_at:"2099-01-01T00:00:00Z"}]}'
    ;;
  api:repos/2lab-ai/homebrew-tap/actions/runs/9001)
    jq -n \
      --arg title "dbotter-preview $GH_FAKE_TAG $GH_FAKE_SOURCE_SHA" \
      --arg conclusion "${GH_FAKE_CONCLUSION:-success}" \
      '{id:9001,run_attempt:2,display_title:$title,event:"workflow_dispatch",status:"completed",conclusion:$conclusion}'
    ;;
  api:repos/2lab-ai/homebrew-tap/git/ref/heads/master)
    jq -n --arg sha "$GH_FAKE_MASTER_COMMIT" '{object:{type:"commit",sha:$sha}}'
    ;;
  api:repos/2lab-ai/homebrew-tap/compare/*)
    jq -n \
      --arg base "$GH_FAKE_FORMULA_COMMIT" \
      --arg merge_base "${GH_FAKE_MERGE_BASE:-$GH_FAKE_FORMULA_COMMIT}" \
      '{status:"ahead",base_commit:{sha:$base},merge_base_commit:{sha:$merge_base}}'
    ;;
  api:repos/2lab-ai/homebrew-tap/contents/Formula/dbotter-preview.rb?ref=*)
    content="$(base64 <"$GH_FAKE_FORMULA" | tr -d '\n')"
    jq -n \
      --arg content "$content" \
      --arg sha "$GH_FAKE_FORMULA_BLOB" \
      '{type:"file",path:"Formula/dbotter-preview.rb",encoding:"base64",content:$content,sha:$sha}'
    ;;
  run:download)
    output=""
    while [[ $# -gt 0 ]]; do
      if [[ "$1" == "--dir" ]]; then
        output="$2"
        break
      fi
      shift
    done
    mkdir -p "$output"
    cp "$GH_FAKE_PROOF" "$output/dbotter-tap-dispatch.json"
    ;;
  release:download)
    output=""
    while [[ $# -gt 0 ]]; do
      if [[ "$1" == "--dir" ]]; then
        output="$2"
        break
      fi
      shift
    done
    mkdir -p "$output"
    cp "$GH_FAKE_MANIFEST" "$output/preview-manifest.json"
    ;;
  *)
    echo "unexpected fake gh command: $*" >&2
    exit 90
    ;;
esac
SH
chmod +x "$fake_gh"

run_handshake() {
  local output="$1"
  GH_TOKEN=fake \
  GH_FAKE_TAG="$tag" \
  GH_FAKE_SOURCE_SHA="$source_sha" \
  GH_FAKE_FORMULA_COMMIT="$formula_commit" \
  GH_FAKE_MASTER_COMMIT="$master_commit" \
  GH_FAKE_FORMULA_BLOB="$formula_blob" \
  GH_FAKE_FORMULA="$formula" \
  GH_FAKE_PROOF="$proof" \
  GH_FAKE_MANIFEST="$manifest" \
  DBOTTER_TAP_POLL_ATTEMPTS=1 \
  DBOTTER_TAP_POLL_INTERVAL_SECONDS=0 \
  PATH="$tmp_dir/bin:$PATH" \
    ./scripts/dispatch-and-verify-tap.sh \
      --tag "$tag" \
      --source-sha "$source_sha" \
      --version "$version" \
      --manifest-url "$manifest_url" \
      --manifest-sha256 "$manifest_sha256" \
      --output "$output"
}

verified="$tmp_dir/verified.json"
run_handshake "$verified" >/dev/null
cmp -s "$proof" "$verified" || fail "verified output is not the exact tap proof"

if GH_FAKE_CONCLUSION=failure run_handshake "$tmp_dir/failed-run.json" >/dev/null 2>&1; then
  fail "failed tap workflow conclusion was accepted"
fi

if GH_FAKE_MERGE_BASE=3333333333333333333333333333333333333333 \
  run_handshake "$tmp_dir/unreachable.json" >/dev/null 2>&1; then
  fail "formula commit not reachable from master was accepted"
fi

if run_handshake "$verified" >/dev/null 2>&1; then
  fail "verified tap proof output was replaced"
fi

echo "tap handshake contract: ok"
