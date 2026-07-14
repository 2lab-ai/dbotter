def sha256_fingerprint:
  type == "string" and test("^sha256:[0-9a-f]{64}$");

def input_descriptor:
  type == "object"
  and (.kind | type == "string" and length > 0)
  and (.fixture_statement | type == "string" and length > 0)
  and (.sha256 | sha256_fingerprint)
  and (keys | sort == ["fixture_statement", "kind", "sha256"]);

def stream_descriptor:
  type == "object"
  and (.present | type == "boolean")
  and (.sha256 | sha256_fingerprint)
  and (keys | sort == ["present", "sha256"]);

def captured_step:
  type == "object"
  and (.input | input_descriptor)
  and (.streams.stdout | stream_descriptor)
  and (.streams.stderr | stream_descriptor)
  and (has("stdout") | not)
  and (has("stderr") | not)
  and (has("argv") | not)
  and (has("command") | not)
  and (has("sql") | not)
  and (has("text") | not);

def source_shape:
  type == "object"
  and (.repository_root | type == "string" and length > 0)
  and (.repository_root_matches | type == "boolean")
  and (.git_head | type == "string")
  and (.branch == null or (.branch | type == "string" and length > 0))
  and (.detached | type == "boolean")
  and (.dirty | type == "boolean")
  and (.required_files_tracked | type == "boolean")
  and (.clean_committed | type == "boolean")
  and (.clean_committed == (
    .repository_root_matches
    and ((.git_head | test("^[0-9a-f]{40}([0-9a-f]{24})?$")))
    and (.detached | not)
    and (.dirty | not)
    and .required_files_tracked
  ));

def all_inputs:
  [
    .mysql.app_steps[].input,
    .mysql.official_readback.input,
    .redis.app_steps[].input,
    .redis.official_readback.get.input,
    .redis.official_readback.ttl.input
  ];

def receipt_contract:
  .schema_version == 2
  and (.source | source_shape)
  and (all_inputs | length == 11 and all(input_descriptor))
  and ([
    .mysql.app_steps[],
    .mysql.official_readback,
    .redis.app_steps[],
    .redis.official_readback.get,
    .redis.official_readback.ttl
  ] | length == 11 and all(captured_step))
  and (.assertions.mysql | type == "boolean")
  and (.assertions.redis | type == "boolean")
  and (.assertions.source_provenance | type == "boolean")
  and (.assertions.credential_leak | type == "boolean")
  and (.assertions.source_provenance == .source.clean_committed)
  and (.assertions.overall == (
    .assertions.mysql
    and .assertions.redis
    and .assertions.source_provenance
    and (.assertions.credential_leak | not)
  ))
  and (if .assertions.overall then
    .source.clean_committed and (.assertions.credential_leak | not)
  else true end);

receipt_contract
