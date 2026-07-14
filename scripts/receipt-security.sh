#!/bin/sh

# Shared by the live verifier and its static contract test. This file defines
# functions only; sourcing it must not execute verification work.

receipt_sha256_file() {
  receipt_sha_file=$1
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$receipt_sha_file" | awk '{print $1}'
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$receipt_sha_file" | awk '{print $1}'
  elif command -v openssl >/dev/null 2>&1; then
    openssl dgst -sha256 "$receipt_sha_file" | awk '{print $NF}'
  else
    echo "error: sha256sum, shasum, or openssl is required" >&2
    return 1
  fi
}

receipt_sha256_text() {
  receipt_sha_text=$1
  if command -v sha256sum >/dev/null 2>&1; then
    printf '%s' "$receipt_sha_text" | sha256sum | awk '{print "sha256:" $1}'
  elif command -v shasum >/dev/null 2>&1; then
    printf '%s' "$receipt_sha_text" | shasum -a 256 | awk '{print "sha256:" $1}'
  elif command -v openssl >/dev/null 2>&1; then
    printf '%s' "$receipt_sha_text" | openssl dgst -sha256 | awk '{print "sha256:" $NF}'
  else
    echo "error: sha256sum, shasum, or openssl is required" >&2
    return 1
  fi
}

receipt_candidate_contains_secret() {
  receipt_secret_candidate=$1
  receipt_secret_value=$2
  [ -n "$receipt_secret_value" ] || return 1

  # Keep the secret in shell memory. In particular, do not pass it to grep as
  # argv or write a secret-pattern file that could survive a failed run.
  receipt_secret_content=$(cat "$receipt_secret_candidate")
  case "$receipt_secret_content" in
    *"$receipt_secret_value"*) return 0 ;;
    *) return 1 ;;
  esac
}

receipt_candidate_has_static_leak() {
  receipt_static_candidate=$1

  # These are public local-fixture values, but their presence still proves that
  # a receipt serialized credential material instead of evidence about it.
  if receipt_candidate_contains_secret "$receipt_static_candidate" 'dbotter-local-only' \
    || receipt_candidate_contains_secret "$receipt_static_candidate" 'root-local-only'; then
    return 0
  fi

  # Reject common database/cache/web URIs containing user:password@ authority.
  # Redacted endpoints such as mysql://127.0.0.1:33306 do not match.
  LC_ALL=C grep -E -q \
    '(mysql|mariadb|redis|rediss|mongodb(\+srv)?|postgres|postgresql|https?)://[^[:space:]/:@]+:[^[:space:]@/]+@' \
    "$receipt_static_candidate"
}
