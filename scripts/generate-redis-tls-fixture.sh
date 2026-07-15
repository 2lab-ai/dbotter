#!/bin/sh
set -eu

repo_dir=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
output=${1:-"$repo_dir/artifacts/redis-tls"}

if ! command -v openssl >/dev/null 2>&1; then
  echo "error: openssl is required to generate the Redis TLS fixture" >&2
  exit 1
fi

mkdir -p "$output"
work=$(mktemp -d "${TMPDIR:-/tmp}/dbotter-redis-tls.XXXXXX")
cleanup() {
  rm -rf "$work"
}
trap cleanup EXIT HUP INT TERM

openssl req -x509 -newkey rsa:2048 -sha256 -nodes -days 3 \
  -subj "/CN=dbotter test CA" \
  -keyout "$work/ca-key.pem" -out "$work/ca.pem" >/dev/null 2>&1
openssl req -newkey rsa:2048 -sha256 -nodes \
  -subj "/CN=localhost" \
  -keyout "$work/server-key.pem" -out "$work/server.csr" >/dev/null 2>&1
printf '%s\n' \
  'basicConstraints=CA:FALSE' \
  'keyUsage=digitalSignature,keyEncipherment' \
  'extendedKeyUsage=serverAuth' \
  'subjectAltName=DNS:localhost' >"$work/server.ext"
openssl x509 -req -sha256 -days 3 \
  -in "$work/server.csr" -CA "$work/ca.pem" -CAkey "$work/ca-key.pem" \
  -CAcreateserial -extfile "$work/server.ext" -out "$work/server.pem" >/dev/null 2>&1

openssl req -x509 -newkey rsa:2048 -sha256 -nodes -days 3 \
  -subj "/CN=dbotter wrong test CA" \
  -keyout "$work/wrong-ca-key.pem" -out "$work/wrong-ca.pem" >/dev/null 2>&1

install -m 0644 "$work/ca.pem" "$output/ca.pem"
install -m 0644 "$work/server.pem" "$output/server.pem"
install -m 0644 "$work/server-key.pem" "$output/server-key.pem"
install -m 0644 "$work/wrong-ca.pem" "$output/wrong-ca.pem"

echo "Redis TLS fixture generated at $output"
