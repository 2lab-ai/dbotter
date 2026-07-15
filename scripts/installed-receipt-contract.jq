def exact_keys($expected):
  . as $object
  | ($object | type == "object")
  and (($object | keys | sort) == ($expected | sort));

def sha256: type == "string" and test("^[0-9a-f]{64}$");
def git_sha: type == "string" and test("^[0-9a-f]{40}$");
def timestamp: type == "string" and test("^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}Z$");
def absolute_path: type == "string" and startswith("/") and (test("[[:cntrl:]]") | not);

def config_contract:
  exact_keys(["read_versions", "write_version", "migration_backup_suffix"])
  and .read_versions == [1, 2]
  and .write_version == 2
  and .migration_backup_suffix == ".v1.bak";

def binary_identity:
  exact_keys(["package_version", "channel", "build_id", "source_sha", "target", "arch"])
  and (.package_version | type == "string")
  and .channel == "preview"
  and (.build_id | type == "string")
  and (.source_sha | git_sha)
  and (.target == "aarch64-apple-darwin" or .target == "x86_64-apple-darwin")
  and (.arch == "aarch64" or .arch == "x86_64");

def all_true:
  type == "object" and all(.[]; . == true);

def safe_ids($pattern):
  type == "array" and length > 0 and all(.[]; type == "string" and test($pattern));

. as $receipt
| $manifest[0] as $release_manifest
| ($release_manifest.artifacts | map(select(.target == $receipt.identity.target))) as $matching_artifacts
| ($matching_artifacts[0] // {}) as $artifact
| (
  exact_keys([
    "schema", "started_at", "finished_at", "source", "build", "release", "formula",
    "install", "identity", "config_contract", "checks", "ax",
    "external_export_verifier", "assertions"
  ])
  and .schema == "dbotter.installed-receipt.v1"
  and (.started_at | timestamp)
  and (.finished_at | timestamp)
  and .started_at <= .finished_at
  and (.source | exact_keys(["kind", "commit", "expected_sha", "run_id", "run_attempt"]))
  and .source.kind == "ci_expected_sha"
  and (.source.commit | git_sha)
  and .source.commit == .source.expected_sha
  and .source.commit == $release_manifest.source_sha
  and .source.run_id == $release_manifest.run_id
  and .source.run_attempt == $release_manifest.run_attempt
  and (.build | exact_keys([
    "target", "arch", "profile", "features", "rustc_version", "cargo_version",
    "unsigned_executable_sha256"
  ]))
  and .build.target == .identity.target
  and .build.arch == .identity.arch
  and .build.profile == "release"
  and .build.features == ["desktop", "mongodb"]
  and (.build.rustc_version | type == "string" and test("^rustc [0-9]+\\.[0-9]+\\.[0-9]+"))
  and (.build.cargo_version | type == "string" and test("^cargo [0-9]+\\.[0-9]+\\.[0-9]+"))
  and (.build.unsigned_executable_sha256 | sha256)
  and .build.unsigned_executable_sha256 != $artifact.embedded_executable_sha256
  and .build.unsigned_executable_sha256 != $artifact.sha256
  and (.release | exact_keys(["tag", "manifest_url", "manifest_sha256", "version"]))
  and .release.tag == $release_manifest.tag
  and .release.version == $release_manifest.version
  and .release.manifest_url == (
    "https://github.com/2lab-ai/dbotter/releases/download/" + .release.tag + "/preview-manifest.json"
  )
  and .release.manifest_sha256 == $manifest_sha256
  and (.formula | exact_keys(["repository", "commit", "name", "version", "prefix"]))
  and .formula.repository == "2lab-ai/homebrew-tap"
  and (.formula.commit | git_sha)
  and .formula.name == "dbotter-preview"
  and .formula.version == .release.version
  and (.formula.prefix | absolute_path)
  and (.formula.prefix | endswith("/") | not)
  and (.identity | binary_identity)
  and .identity.package_version == $release_manifest.package_version
  and .identity.build_id == ($release_manifest.tag | sub("^preview-"; ""))
  and .identity.source_sha == $release_manifest.source_sha
  and .identity.arch == $artifact.arch
  and (.config_contract | config_contract)
  and .config_contract == $release_manifest.config_contract
  and (.install | exact_keys([
    "requested_app_path", "resolved_app_path", "bundle_id", "arch", "executable", "cli_shim"
  ]))
  and .install.requested_app_path == (.formula.prefix + "/Dbotter Preview.app")
  and .install.resolved_app_path == .install.requested_app_path
  and .install.bundle_id == "ai.2lab.dbotter.preview"
  and .install.bundle_id == $artifact.bundle_id
  and .install.arch == .identity.arch
  and (.install.executable | exact_keys([
    "path", "realpath", "device", "inode", "bytes", "sha256", "codesign_valid"
  ]))
  and (.install.executable.path | absolute_path)
  and .install.executable.path == (.install.requested_app_path + "/Contents/MacOS/dbotter")
  and .install.executable.realpath == (.install.resolved_app_path + "/Contents/MacOS/dbotter")
  and (.install.executable.device | type == "number" and . > 0 and floor == .)
  and (.install.executable.inode | type == "number" and . > 0 and floor == .)
  and (.install.executable.bytes | type == "number" and . > 0 and floor == .)
  and .install.executable.sha256 == $artifact.embedded_executable_sha256
  and .install.executable.codesign_valid == true
  and (.install.cli_shim | exact_keys(["path", "realpath", "device", "inode", "sha256"]))
  and (.install.cli_shim.path | absolute_path)
  and .install.cli_shim.realpath == .install.executable.realpath
  and .install.cli_shim.device == .install.executable.device
  and .install.cli_shim.inode == .install.executable.inode
  and .install.cli_shim.sha256 == .install.executable.sha256
  and (.checks | exact_keys([
    "version", "config_contract", "shim_identity", "bundle_identity", "executable_hash",
    "codesign", "check", "exec", "mysql_browse", "redis_browse", "redis_inspect"
  ]))
  and (.checks | all_true)
  and (.ax | exact_keys([
    "app_path", "pid", "stale_process_disposition", "pid_executable",
    "author_ids", "action_ids", "public_codes"
  ]))
  and .ax.app_path == .install.requested_app_path
  and (.ax.pid | type == "number" and . > 0 and floor == .)
  and (.ax.stale_process_disposition == "none" or .ax.stale_process_disposition == "terminated")
  and (.ax.pid_executable | exact_keys(["realpath", "device", "inode", "sha256"]))
  and .ax.pid_executable.realpath == .install.executable.realpath
  and .ax.pid_executable.device == .install.executable.device
  and .ax.pid_executable.inode == .install.executable.inode
  and .ax.pid_executable.sha256 == .install.executable.sha256
  and (.ax.author_ids | safe_ids("^[a-z0-9_.:-]+$"))
  and (.ax.action_ids | safe_ids("^[a-zA-Z0-9_.:-]+$"))
  and (.ax.public_codes | safe_ids("^[A-Z0-9_]+$"))
  and (.external_export_verifier | type == "array" and length == 3)
  and ([.external_export_verifier[].fixture_id] | sort == ["seeded.csv", "seeded.json", "seeded.tsv"])
  and all(.external_export_verifier[];
    exact_keys(["fixture_id", "expected_sha256", "actual_sha256", "verdict"])
    and (.expected_sha256 | sha256)
    and (.actual_sha256 | sha256)
    and .expected_sha256 == .actual_sha256
    and .verdict == true
  )
  and (.assertions | exact_keys([
    "source_match", "build_match", "manifest_valid", "release_match", "formula_match",
    "app_path_exact", "pid_identity", "identity_exact", "config_contract_exact",
    "shim_same_executable", "executable_hash_match", "codesign_valid", "cli_contracts",
    "live_contracts", "accessibility", "contrast", "recovery_totality", "clipboard",
    "disclosure", "export", "credential_leak", "user_content_leak", "overall"
  ]))
  and .assertions.overall == (
    ([.assertions | to_entries[] | select(.key != "overall" and .key != "credential_leak" and .key != "user_content_leak") | .value] | all)
    and (.assertions.credential_leak | not)
    and (.assertions.user_content_leak | not)
  )
  and .assertions.overall == true
)
