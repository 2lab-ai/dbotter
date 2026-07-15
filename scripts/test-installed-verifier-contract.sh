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

echo "installed verifier contract: ok"
