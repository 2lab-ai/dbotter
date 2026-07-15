#!/bin/sh
set -eu

fixture_sql="$(mktemp)"
trap 'rm -f "$fixture_sql"' EXIT

{
  printf '%s\n' \
    'CREATE DATABASE IF NOT EXISTS dbotter_allowed CHARACTER SET utf8mb4 COLLATE utf8mb4_bin;' \
    'CREATE DATABASE IF NOT EXISTS dbotter_forbidden CHARACTER SET utf8mb4 COLLATE utf8mb4_bin;' \
    'USE dbotter_allowed;' \
    'CREATE TABLE catalog_anchor (id BIGINT PRIMARY KEY, label VARCHAR(64) NOT NULL);' \
    'CREATE VIEW catalog_view AS SELECT id, label FROM catalog_anchor;'

  printf 'CREATE TABLE wide_catalog (id INT PRIMARY KEY'
  column=1
  while [ "$column" -le 130 ]; do
    printf ', column_%03d TINYINT' "$column"
    column=$((column + 1))
  done
  printf ');\n'

  relation=0
  while [ "$relation" -lt 2005 ]; do
    printf 'CREATE TABLE bulk_%04d (id INT PRIMARY KEY);\n' "$relation"
    relation=$((relation + 1))
  done
  printf '%s\n' 'CREATE TABLE tail_after_cap (id INT PRIMARY KEY);'

  padding='xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx'
  enum_values=''
  enum_index=0
  while [ "$enum_index" -lt 240 ]; do
    if [ -n "$enum_values" ]; then
      enum_values="$enum_values,"
    fi
    enum_values="${enum_values}'value_${enum_index}_${padding}'"
    enum_index=$((enum_index + 1))
  done
  metadata_table=0
  while [ "$metadata_table" -lt 90 ]; do
    printf 'CREATE TABLE meta_%03d (payload ENUM(%s) NOT NULL);\n' \
      "$metadata_table" "$enum_values"
    metadata_table=$((metadata_table + 1))
  done

  printf '%s\n' \
    "CREATE USER IF NOT EXISTS 'dbotter_catalog'@'%' IDENTIFIED BY 'dbotter-local-only';" \
    "GRANT SELECT, SHOW VIEW ON dbotter_allowed.* TO 'dbotter_catalog'@'%';" \
    "CREATE USER IF NOT EXISTS 'dbotter_denied'@'%' IDENTIFIED BY 'dbotter-local-only';" \
    "GRANT USAGE ON *.* TO 'dbotter_denied'@'%';" \
    'FLUSH PRIVILEGES;'
} >"$fixture_sql"

MYSQL_PWD="$MYSQL_ROOT_PASSWORD" mysql \
  --protocol=socket \
  --user=root \
  --max-allowed-packet=64M \
  <"$fixture_sql"
