#!/bin/sh
set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)

fail() {
  echo "installed verifier contract: $*" >&2
  exit 1
}

for verifier in scripts/verify-installed.sh scripts/verify-installed-gui.sh; do
  [ -x "$root/$verifier" ] || fail "$verifier is missing or not executable"
  "$root/$verifier" --help >/dev/null
done

cli="$root/scripts/verify-installed.sh"
for literal in \
  'brew --prefix dbotter-preview' \
  'Dbotter Preview.app/Contents/MacOS/dbotter' \
  'version --format json' \
  'config-contract --format json' \
  'codesign --verify --deep --strict' \
  'browse mysql schemas' \
  'browse redis keys' \
  'inspect redis key'; do
  grep -Fq -- "$literal" "$cli" || fail "CLI verifier is missing: $literal"
done
grep -Fq -- 'expected_shim="$(brew --prefix)/bin/dbotter"' "$cli" \
  || fail "CLI verifier does not require the exact Homebrew shim path"
grep -Fq -- 'receipt_candidate_has_static_leak "$temporary"' "$cli" \
  || fail "CLI verifier does not leak-scan generated evidence"

gui="$root/scripts/verify-installed-gui.sh"
for required_id in \
  connection.new \
  connection.new.mysql \
  connection.new.redis \
  connection.mongodb.planned \
  profile.connection_id \
  profile.host \
  profile.redis_tls.ca_file \
  profile.redis_tls.ca_file.pick \
  profile.credential.session.keep \
  profile.credential.session.replace \
  profile.credential.session.forget \
  profile.delete.active_warning \
  editor.target \
  editor.input \
  editor.row_limit \
  editor.timeout \
  editor.execute \
  editor.cancel \
  result.table \
  result.copy.cell \
  result.copy.row \
  result.copy.all \
  result.export.csv \
  result.export.tsv \
  result.export.json; do
  grep -Fq -- "$required_id" "$gui" || fail "GUI verifier omits AXIdentifier: $required_id"
done

for fail_closed in \
  src/export.rs \
  src/export_file.rs \
  src/ui/result_view.rs \
  DBOTTER_AX_DRIVER \
  'lsof -a -p' \
  'ax_identifier_readback' \
  'clipboard_contracts' \
  'export_contracts'; do
  grep -Fq -- "$fail_closed" "$gui" || fail "GUI verifier omits fail-closed contract: $fail_closed"
done

for provenance in \
  DBOTTER_AX_DRIVER_SHA256 \
  DBOTTER_AX_DRIVER_SOURCE \
  DBOTTER_AX_DRIVER_SOURCE_SHA256 \
  'git ls-files --error-unmatch' \
  'receipt_sha256_file "$ax_driver"' \
  'source_repo_path: $source_repo_path' \
  'receipt_candidate_has_static_leak "$output_temporary"'; do
  grep -Fq -- "$provenance" "$gui" \
    || fail "GUI verifier omits reviewed driver provenance/evidence safety: $provenance"
done

launch_line="$(grep -n -- '--phase launch' "$gui" | cut -d: -f1)"
identity_line="$(grep -n -- 'lsof -a -p' "$gui" | cut -d: -f1)"
journey_line="$(grep -n -- '--phase journey' "$gui" | cut -d: -f1)"
[ "$launch_line" -lt "$identity_line" ] && [ "$identity_line" -lt "$journey_line" ] \
  || fail "GUI verifier does not prove PID identity before the AX journey"

echo "installed verifier contract: ok"
