#!/usr/bin/env ruby
# frozen_string_literal: true

require "optparse"
require "psych"

class ContractError < StandardError; end

def fail_contract(message)
  raise ContractError, message
end

def reject_duplicate_keys(node, location)
  case node
  when Psych::Nodes::Mapping
    seen = {}
    node.children.each_slice(2) do |key_node, value_node|
      fail_contract("#{location}: mapping key is not scalar") unless key_node.is_a?(Psych::Nodes::Scalar)

      key = key_node.value
      fail_contract("#{location}: duplicate YAML key #{key.inspect}") if seen[key]

      seen[key] = true
      reject_duplicate_keys(value_node, "#{location}.#{key}")
    end
  when Psych::Nodes::Sequence
    node.children.each_with_index do |child, index|
      reject_duplicate_keys(child, "#{location}[#{index}]")
    end
  when Psych::Nodes::Document, Psych::Nodes::Stream
    node.children.each { |child| reject_duplicate_keys(child, location) }
  end
end

def load_workflow(path)
  text = File.read(path, encoding: "UTF-8")
  reject_duplicate_keys(Psych.parse_stream(text, filename: path.to_s), path.to_s)
  value = Psych.safe_load(text, permitted_classes: [], permitted_symbols: [], aliases: false, filename: path.to_s)
  fail_contract("#{path}: root is not a mapping") unless value.is_a?(Hash)

  value
rescue Errno::ENOENT, Psych::Exception => e
  fail_contract("#{path}: unreadable YAML: #{e.message}")
end

def exact_keys(value, keys, location)
  fail_contract("#{location}: expected mapping") unless value.is_a?(Hash)
  actual = value.keys
  return value if actual.sort == keys.sort

  fail_contract("#{location}: wrong keys actual=#{actual.sort.inspect} expected=#{keys.sort.inspect}")
end

def job(workflow, name, file)
  jobs = workflow["jobs"]
  fail_contract("#{file}: jobs is not a mapping") unless jobs.is_a?(Hash)
  value = jobs[name]
  fail_contract("#{file}: missing job #{name}") unless value.is_a?(Hash)

  value
end

def needs(value)
  raw = value["needs"]
  raw.is_a?(Array) ? raw : [raw].compact
end

def steps(value, location)
  result = value["steps"]
  fail_contract("#{location}: steps is not an array") unless result.is_a?(Array)
  fail_contract("#{location}: step is not a mapping") unless result.all? { |step| step.is_a?(Hash) }

  result
end

def unique_named_step(value, name, location)
  matches = steps(value, location).select { |step| step["name"] == name }
  fail_contract("#{location}: expected exactly one step named #{name.inspect}") unless matches.length == 1

  matches.first
end

def require_substrings(text, substrings, location)
  fail_contract("#{location}: expected string") unless text.is_a?(String)
  substrings.each do |substring|
    fail_contract("#{location}: missing #{substring.inspect}") unless text.include?(substring)
  end
end

options = { workflow_dir: ".github/workflows" }
OptionParser.new do |parser|
  parser.on("--workflow-dir PATH") { |path| options[:workflow_dir] = path }
end.parse!

begin
  directory = File.expand_path(options[:workflow_dir])
  workflows = {}
  %w[verify ci preview release].each do |name|
    workflows[name] = load_workflow(File.join(directory, "#{name}.yml"))
  end

  verify_on = workflows["verify"]["on"] || workflows["verify"][true]
  workflow_call = verify_on.is_a?(Hash) ? verify_on["workflow_call"] : nil
  fail_contract("verify.yml: workflow_call is missing") unless workflow_call.is_a?(Hash)
  candidate = workflow_call.dig("inputs", "candidate_sha")
  exact_keys(candidate, %w[description required type], "verify.yml candidate_sha")
  fail_contract("verify.yml: candidate_sha is not required string input") unless candidate["required"] == true && candidate["type"] == "string" && candidate["description"].is_a?(String) && !candidate["description"].empty?

  candidate_expressions = {
    "ci" => "${{ github.event.pull_request.head.sha || github.sha }}",
    "preview" => "${{ github.sha }}",
    "release" => "${{ github.sha }}"
  }
  candidate_expressions.each do |name, expression|
    verify_job = job(workflows[name], "verify", "#{name}.yml")
    fail_contract("#{name}.yml: verify job must call the local reusable gate") unless verify_job["uses"] == "./.github/workflows/verify.yml"
    fail_contract("#{name}.yml: verify candidate_sha is not wired") unless verify_job.dig("with", "candidate_sha") == expression
  end

  macos_package = job(workflows["verify"], "macos-package", "verify.yml")
  fail_contract("verify.yml: macOS package job must depend on hermetic") unless needs(macos_package) == ["hermetic"]
  fail_contract("verify.yml: macOS package job must use the pinned native runner") unless macos_package["runs-on"] == "macos-15"
  macos_checkout = unique_named_step(macos_package, "Checkout exact candidate", "verify.yml.macos-package")
  fail_contract("verify.yml: macOS package checkout is not exact") unless macos_checkout["uses"] == "actions/checkout@v4" && macos_checkout["with"] == {
    "ref" => "${{ inputs.candidate_sha }}",
    "fetch-depth" => 0,
    "persist-credentials" => false
  }
  macos_live = unique_named_step(macos_package, "Build and verify real macOS package", "verify.yml.macos-package")
  require_substrings(
    macos_live["run"],
    [
      "./scripts/test-macos-package-live.sh",
      '--expected-source-sha "${{ inputs.candidate_sha }}"',
      '--expected-tag "${{ steps.package.outputs.tag }}"'
    ],
    "verify.yml macOS package live test"
  )

  live = job(workflows["verify"], "live", "verify.yml")
  fail_contract("verify.yml: live job must depend on hermetic") unless needs(live) == ["hermetic"]
  fail_contract("verify.yml: live job must use the pinned Linux runner") unless live["runs-on"] == "ubuntu-24.04"
  live_checkout = unique_named_step(live, "Checkout exact candidate", "verify.yml.live")
  fail_contract("verify.yml: live checkout is not exact") unless live_checkout["uses"] == "actions/checkout@v4" && live_checkout["with"] == {
    "ref" => "${{ inputs.candidate_sha }}",
    "fetch-depth" => 0,
    "persist-credentials" => false
  }
  live_run = unique_named_step(live, "Run mandatory live contracts", "verify.yml.live")
  fail_contract("verify.yml: live run identity is not explicit") unless live_run["env"] == {
    "DBOTTER_COMPOSE_PROJECT" => "dbotter-e2e",
    "DBOTTER_MYSQL_PASSWORD" => "dbotter-local-only",
    "DBOTTER_REDIS_PASSWORD" => "dbotter-redis-local-only",
    "GITHUB_RUN_ID" => "${{ github.run_id }}",
    "GITHUB_RUN_ATTEMPT" => "${{ github.run_attempt }}"
  }
  require_substrings(
    live_run["run"],
    ["./scripts/verify-live-contracts.sh", '--expected-sha "${{ inputs.candidate_sha }}"'],
    "verify.yml live run"
  )
  live_upload = unique_named_step(live, "Upload live verification receipt", "verify.yml.live")
  fail_contract("verify.yml: live receipt upload action is wrong") unless live_upload["uses"] == "actions/upload-artifact@v4"
  fail_contract("verify.yml: live receipt upload is not exact") unless live_upload["with"] == {
    "name" => "live-contract-verification",
    "path" => "artifacts/live-contract-receipt.json",
    "if-no-files-found" => "error"
  }

  preview = workflows["preview"]
  preview_jobs = preview["jobs"]
  exact_keys(preview_jobs, %w[verify plan build publish tap], "preview.yml.jobs")
  expected_needs = {
    "plan" => ["verify"],
    "build" => %w[verify plan],
    "publish" => %w[verify plan build],
    "tap" => %w[verify plan publish]
  }
  expected_needs.each do |name, expected|
    actual = needs(job(preview, name, "preview.yml"))
    fail_contract("preview.yml: #{name}.needs=#{actual.inspect}, expected #{expected.inspect}") unless actual == expected
  end

  build = job(preview, "build", "preview.yml")
  matrix = build.dig("strategy", "matrix", "include")
  expected_matrix = [
    { "target" => "aarch64-apple-darwin", "arch" => "aarch64", "host_arch" => "arm64", "os" => "macos-15", "kind" => "macos" },
    { "target" => "x86_64-apple-darwin", "arch" => "x86_64", "host_arch" => "x86_64", "os" => "macos-15-intel", "kind" => "macos" },
    { "target" => "aarch64-unknown-linux-gnu", "arch" => "aarch64", "host_arch" => "aarch64", "os" => "ubuntu-24.04-arm", "kind" => "linux" },
    { "target" => "x86_64-unknown-linux-gnu", "arch" => "x86_64", "host_arch" => "x86_64", "os" => "ubuntu-24.04", "kind" => "linux" }
  ]
  fail_contract("preview.yml: build matrix is not the exact four native targets") unless matrix == expected_matrix

  macos_build = unique_named_step(build, "Package signed macOS app", "preview.yml.build")
  require_substrings(
    macos_build["run"],
    [
      "./scripts/build-macos-app.sh",
      '--expected-source-sha "${{ needs.plan.outputs.commit }}"',
      '--expected-tag "${{ needs.plan.outputs.tag }}"'
    ],
    "preview.yml.build macOS package run"
  )

  linux_package = unique_named_step(build, "Package Linux native executable", "preview.yml.build")
  require_substrings(
    linux_package["run"],
    [
      "./scripts/build-linux-artifact.sh",
      '--expected-source-sha "${{ needs.plan.outputs.commit }}"',
      '--expected-tag "${{ needs.plan.outputs.tag }}"'
    ],
    "preview.yml.build Linux package run"
  )
  linux_upload = unique_named_step(build, "Upload Linux package", "preview.yml.build")
  require_substrings(
    linux_upload.dig("with", "path"),
    [
      "out/dbotter-preview-linux-${{ matrix.arch }}",
      "out/preview-artifact-linux-${{ matrix.arch }}.json",
      "out/package-receipt-linux-${{ matrix.arch }}.json"
    ],
    "preview.yml.build Linux upload paths"
  )

  publish = job(preview, "publish", "preview.yml")
  publish_steps = steps(publish, "preview.yml.publish")
  assemble = unique_named_step(publish, "Assemble and validate immutable release files", "preview.yml.publish")
  assemble_run = assemble["run"]
  require_substrings(
    assemble_run,
    [
      "--release-dir release",
      "artifacts/package-aarch64/preview-artifact-aarch64.json",
      "artifacts/package-x86_64/preview-artifact-x86_64.json",
      "artifacts/package-linux-aarch64/preview-artifact-linux-aarch64.json",
      "artifacts/package-linux-x86_64/preview-artifact-linux-x86_64.json"
    ],
    "preview.yml.publish assembly"
  )
  fail_contract("preview.yml.publish: assembly does not pass exactly four descriptors") unless assemble_run.scan(/--artifact\b/).length == 4
  chmod_index = assemble_run.index("chmod 0755 release/dbotter-preview-linux-")
  assemble_index = assemble_run.index("./scripts/assemble-preview-manifest.py")
  fail_contract("preview.yml.publish: Linux mode is not restored before remeasurement") unless chmod_index && assemble_index && chmod_index < assemble_index

  release_index = publish_steps.index { |step| step["uses"] == "softprops/action-gh-release@v2" }
  fail_contract("preview.yml.publish: immutable release action is missing") unless release_index
  fail_contract("preview.yml.publish: final remeasurement must immediately precede release") unless release_index.positive? && publish_steps[release_index - 1]["name"] == "Revalidate final release bytes"
  final_remeasure = publish_steps[release_index - 1]["run"]
  require_substrings(
    final_remeasure,
    ["./scripts/assemble-preview-manifest.py", "--release-dir release", "cmp"],
    "preview.yml.publish final remeasurement"
  )

  tap = job(preview, "tap", "preview.yml")
  tap_steps = steps(tap, "preview.yml.tap")
  tap_checkout = unique_named_step(tap, "Checkout verified candidate", "preview.yml.tap")
  fail_contract("preview.yml.tap: checkout action is wrong") unless tap_checkout["uses"] == "actions/checkout@v4"
  fail_contract("preview.yml.tap: checkout is not pinned to the planned commit") unless tap_checkout["with"] == {
    "ref" => "${{ needs.plan.outputs.commit }}",
    "fetch-depth" => 0,
    "persist-credentials" => false
  }
  dispatch = unique_named_step(tap, "Dispatch and verify exact immutable tap inputs", "preview.yml.tap")
  fail_contract("preview.yml.tap: exact checkout must precede handshake") unless tap_steps.index(tap_checkout) < tap_steps.index(dispatch)
  require_substrings(
    dispatch["run"],
    [
      "./scripts/dispatch-and-verify-tap.sh",
      '--tag "${{ needs.plan.outputs.tag }}"',
      '--source-sha "${{ needs.plan.outputs.commit }}"',
      '--version "${{ needs.plan.outputs.version }}"',
      '--manifest-url "${{ needs.plan.outputs.manifest_url }}"',
      '--manifest-sha256 "${{ needs.publish.outputs.manifest_sha256 }}"',
      "--output tap-evidence/dbotter-tap-dispatch.json"
    ],
    "preview.yml.tap handshake"
  )
  proof_upload = unique_named_step(tap, "Upload verified tap proof", "preview.yml.tap")
  fail_contract("preview.yml.tap: proof upload action is wrong") unless proof_upload["uses"] == "actions/upload-artifact@v4"
  fail_contract("preview.yml.tap: proof upload path is wrong") unless proof_upload.dig("with", "path") == "tap-evidence/dbotter-tap-dispatch.json"

  all_run_text = preview_jobs.values.flat_map do |value|
    value.is_a?(Hash) && value["steps"].is_a?(Array) ? value["steps"].map { |step| step["run"] }.compact : []
  end.join("\n")
  fail_contract("preview.yml: legacy queue-only gh workflow run remains") if all_run_text.include?("gh workflow run")
  %w[gh\ release\ delete delete-release delete-ref --cleanup-tag].each do |forbidden|
    fail_contract("preview.yml: forbidden destructive command #{forbidden.inspect}") if all_run_text.include?(forbidden.tr("\\", ""))
  end

  puts "workflow graph: ok"
rescue ContractError => e
  warn "workflow graph: #{e.message}"
  exit 1
end
