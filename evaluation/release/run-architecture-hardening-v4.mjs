#!/usr/bin/env bun

import { createHash } from "node:crypto";
import { spawnSync } from "node:child_process";
import {
  closeSync,
  existsSync,
  lstatSync,
  mkdirSync,
  openSync,
  readFileSync,
  readlinkSync,
  readdirSync,
  writeFileSync,
} from "node:fs";
import { join, relative } from "node:path";
import { fileURLToPath } from "node:url";

const root = fileURLToPath(new URL("../..", import.meta.url));
const protocolPath = join(
  root,
  "evaluation/release/architecture-hardening-v4-protocol.json",
);
const ledgerPath = join(
  root,
  "evaluation/release/architecture-hardening-v4-ledger.json",
);
const runnerPath = fileURLToPath(import.meta.url);
const protocol = readJson(protocolPath);
const ledger = readJson(ledgerPath);
const mode = parseArguments(process.argv.slice(2));

function fail(message) {
  throw new Error(message);
}

function readJson(fileName) {
  return JSON.parse(readFileSync(fileName, "utf8"));
}

function sha256(fileNameOrBytes) {
  const bytes = Buffer.isBuffer(fileNameOrBytes)
    ? fileNameOrBytes
    : readFileSync(fileNameOrBytes);
  return createHash("sha256").update(bytes).digest("hex");
}

function git(arguments_) {
  const result = spawnSync("git", arguments_, {
    cwd: root,
    encoding: "utf8",
    stdio: ["ignore", "pipe", "pipe"],
  });
  if (result.status !== 0) {
    const message = result.stderr.trim().split("\n")[0];
    fail(`git ${arguments_.join(" ")} failed: ${message}`);
  }
  return result.stdout.trim();
}

function parseArguments(arguments_) {
  if (arguments_.length === 1 && arguments_[0] === "--validate-only") {
    return { action: "validate" };
  }
  if (
    arguments_.length === 2 &&
    arguments_[0] === "--execute" &&
    ["linux-x86_64", "macos-arm64"].includes(arguments_[1])
  ) {
    return { action: "execute", target: arguments_[1] };
  }
  if (arguments_.length === 1 && arguments_[0] === "--aggregate") {
    return { action: "aggregate" };
  }
  fail(
    "usage: run-architecture-hardening-v4.mjs --validate-only | --execute <linux-x86_64|macos-arm64> | --aggregate",
  );
}

function checkProtocolIdentity() {
  if (
    protocol.schema_version !==
      "distill.architecture-hardening-qualification/v1" ||
    protocol.qualification_id !== "architecture-hardening-v4-20260731" ||
    protocol.status !== "PREREGISTERED" ||
    ledger.schema_version !==
      "distill.architecture-hardening-qualification-ledger/v1" ||
    ledger.qualification_id !== protocol.qualification_id ||
    ledger.status !== "PREREGISTERED" ||
    ledger.protocol.path !==
      "evaluation/release/architecture-hardening-v4-protocol.json" ||
    ledger.protocol.schema_version !== protocol.schema_version ||
    ledger.authorization.status !== "REQUIRED" ||
    ledger.authorization.external_record !==
      protocol.authorization.external_record
  ) {
    fail("qualification protocol or ledger identity is invalid");
  }
  if (sha256(ledgerPath) !== protocol.tooling.ledger.sha256) {
    fail("qualification ledger hash changed");
  }
  if (sha256(runnerPath) !== protocol.tooling.runner.sha256) {
    fail("qualification runner hash changed");
  }
  if (
    !/^[0-9a-f]{40}$/.test(protocol.source.source_tree) ||
    !Array.isArray(protocol.platforms) ||
    protocol.platforms.join(",") !== "linux-x86_64,macos-arm64" ||
    protocol.destinations.closed_evidence_destination !== null ||
    !protocol.destinations.default_root.startsWith("/tmp/") ||
    !protocol.authorization.external_record.startsWith("/tmp/") ||
    protocol.authorization.external_record.startsWith(
      `${protocol.destinations.default_root}/`,
    ) ||
    protocol.limits.maximum_subscription_invocations !== 0 ||
    protocol.limits.incremental_dollar_ceiling_usd !== 0 ||
    protocol.limits.retry_permitted !== false
  ) {
    fail("qualification protocol constraints are invalid");
  }
  const declaredGates = Object.keys(protocol.gates).sort();
  for (const target of protocol.platforms) {
    const proved = new Set(
      protocol.commands[target].flatMap((command) => command.proves),
    );
    const missing = declaredGates.filter((gate) => !proved.has(gate));
    if (missing.length > 0) {
      fail(`${target} command vector omits gates: ${missing.join(", ")}`);
    }
  }
}

function validatePinnedFile(label, declaration) {
  if (
    declaration === null ||
    typeof declaration !== "object" ||
    typeof declaration.path !== "string" ||
    !/^[0-9a-f]{64}$/.test(declaration.sha256)
  ) {
    fail(`${label} guard is invalid`);
  }
  const fileName = join(root, declaration.path);
  if (!existsSync(fileName) || sha256(fileName) !== declaration.sha256) {
    fail(`${label} changed: ${declaration.path}`);
  }
}

function validateDeclaredFiles(label, declaration) {
  if (
    declaration === null ||
    typeof declaration !== "object" ||
    typeof declaration.path !== "string" ||
    declaration.files === null ||
    typeof declaration.files !== "object"
  ) {
    fail(`${label} declaration is invalid`);
  }
  for (const [name, digest] of Object.entries(declaration.files)) {
    validatePinnedFile(`${label} ${name}`, {
      path: join(declaration.path, name),
      sha256: digest,
    });
  }
}

function validateHistoricalClosure() {
  const direct = new Map(
    ledger.historical_integrity.direct.map((entry) => [entry.path, entry]),
  );
  for (const entry of direct.values()) {
    validatePinnedFile("historical file", entry);
  }

  const nativeProtocol = readJson(
    join(root, "evaluation/release/native-distribution-v2-protocol.json"),
  );
  validatePinnedFile(
    "native v1 protocol",
    nativeProtocol.reconciliation.predecessor.protocol,
  );
  validatePinnedFile("native v1 checker", {
    path: nativeProtocol.reconciliation.predecessor.checker.path,
    sha256: nativeProtocol.reconciliation.predecessor.checker.sha256,
  });
  validatePinnedFile(
    "native v1 aggregate",
    nativeProtocol.reconciliation.predecessor.aggregate,
  );
  validatePinnedFile("native coverage", nativeProtocol.coverage.evidence);
  for (const [name, entry] of Object.entries(
    nativeProtocol.linux.evidence,
  )) {
    validatePinnedFile(`native Linux ${name}`, entry);
  }
  validatePinnedFile("native macOS", nativeProtocol.macos.evidence);
  validateDeclaredFiles(
    "legacy evidence",
    nativeProtocol.historical_integrity.legacy_evidence,
  );

  const pairedLedger = readJson(
    join(root, "evaluation/release/paired-qualification-v5-ledger.json"),
  );
  if (!Array.isArray(pairedLedger.historical_evidence)) {
    fail("paired historical evidence registry is invalid");
  }
  for (const entry of pairedLedger.historical_evidence) {
    validatePinnedFile("paired historical evidence", entry);
  }

  const expectedClosedEvidence = new Set(
    pairedLedger.historical_evidence
      .map((entry) => entry.path)
      .filter((name) => name.startsWith("evaluation/release/evidence/")),
  );
  for (const entry of Object.values(nativeProtocol.linux.evidence)) {
    expectedClosedEvidence.add(entry.path);
  }
  expectedClosedEvidence.add(nativeProtocol.coverage.evidence.path);
  expectedClosedEvidence.add(nativeProtocol.macos.evidence.path);
  expectedClosedEvidence.add(
    nativeProtocol.reconciliation.predecessor.aggregate.path,
  );
  for (const entry of direct.values()) {
    if (entry.path.startsWith("evaluation/release/evidence/")) {
      expectedClosedEvidence.add(entry.path);
    }
  }
  const evidenceDirectory = join(root, "evaluation/release/evidence");
  const actualClosedEvidence = readdirSync(evidenceDirectory)
    .map((name) => `evaluation/release/evidence/${name}`)
    .sort();
  const expected = [...expectedClosedEvidence].sort();
  if (JSON.stringify(actualClosedEvidence) !== JSON.stringify(expected)) {
    fail("closed qualification evidence file set changed");
  }
}

function gitObjectId(bytes) {
  const header = Buffer.from(`blob ${bytes.length}\0`);
  return createHash("sha1").update(header).update(bytes).digest("hex");
}

function currentWorktreeSourceTree() {
  const files = git([
    "ls-files",
    "--cached",
    "--others",
    "--exclude-standard",
    "--",
    ...protocol.source.included_paths,
  ])
    .split("\n")
    .filter(Boolean)
    .filter((name) => existsSync(join(root, name)))
    .sort();
  const listing = files
    .map((name) => {
      const fileName = join(root, name);
      const metadata = lstatSync(fileName);
      let mode = "100644";
      let bytes;
      if (metadata.isSymbolicLink()) {
        mode = "120000";
        bytes = Buffer.from(readlinkSync(fileName));
      } else {
        mode = metadata.mode & 0o111 ? "100755" : "100644";
        bytes = readFileSync(fileName);
      }
      return `${mode} blob ${gitObjectId(bytes)}\t${name}\n`;
    })
    .join("");
  return gitObjectId(Buffer.from(listing));
}

function validatePreregisteredWorktree() {
  if (currentWorktreeSourceTree() !== protocol.source.source_tree) {
    fail("current root-crate worktree differs from the preregistered source_tree");
  }
}

function validatePrivateOwner(metadata, label) {
  if (
    typeof process.getuid === "function" &&
    metadata.uid !== process.getuid()
  ) {
    fail(`${label} is not owned by the current user`);
  }
}

function validatePrivateFile(fileName, label) {
  const metadata = lstatSync(fileName);
  if (metadata.isSymbolicLink() || !metadata.isFile()) {
    fail(`${label} must be a regular file, not a symlink`);
  }
  validatePrivateOwner(metadata, label);
  if ((metadata.mode & 0o077) !== 0) {
    fail(`${label} must not be accessible by group or other users`);
  }
}

function validatePrivateDirectory(fileName, label) {
  const metadata = lstatSync(fileName);
  if (metadata.isSymbolicLink() || !metadata.isDirectory()) {
    fail(`${label} must be a directory, not a symlink`);
  }
  validatePrivateOwner(metadata, label);
  if ((metadata.mode & 0o077) !== 0) {
    fail(`${label} must not be accessible by group or other users`);
  }
}

function authorization() {
  const fileName = protocol.authorization.external_record;
  if (!existsSync(fileName)) {
    fail(`qualification authorization is missing: ${fileName}`);
  }
  validatePrivateFile(fileName, "qualification authorization");
  const value = readJson(fileName);
  if (
    value === null ||
    typeof value !== "object" ||
    value.schema_version !==
      "distill.architecture-hardening-authorization/v1" ||
    typeof value.authorized_by !== "string" ||
    value.authorized_by.length === 0 ||
    typeof value.authorized_at !== "string" ||
    !/^[0-9a-f]{40}$/.test(value.source_revision) ||
    value.protocol_schema_version !== protocol.schema_version ||
    value.qualification_id !== protocol.qualification_id ||
    value.source_tree !== protocol.source.source_tree ||
    value.protocol_sha256 !== sha256(protocolPath) ||
    value.ledger_sha256 !== sha256(ledgerPath) ||
    value.runner_sha256 !== sha256(runnerPath) ||
    value.maximum_cpu_seconds !== protocol.limits.maximum_cpu_seconds ||
    value.maximum_subscription_invocations !==
      protocol.limits.maximum_subscription_invocations ||
    value.incremental_dollar_ceiling_usd !==
      protocol.limits.incremental_dollar_ceiling_usd ||
    JSON.stringify(value.destinations) !==
      JSON.stringify(protocol.destinations) ||
    JSON.stringify(value.permitted_external_actions) !==
      JSON.stringify(protocol.permitted_external_actions)
  ) {
    fail("qualification authorization is missing or does not match the protocol");
  }
  return value;
}

function validateCleanCandidate(value) {
  if (git(["status", "--porcelain=v1", "--untracked-files=all"]) !== "") {
    fail("qualification execution requires a clean source worktree");
  }
  if (git(["rev-parse", "HEAD"]) !== value.source_revision) {
    fail("HEAD differs from the authorized source revision");
  }
  const sourceTree = git([
    "ls-tree",
    "-r",
    "--full-tree",
    value.source_revision,
    "--",
    ...protocol.source.included_paths,
  ]);
  if (gitObjectId(Buffer.from(`${sourceTree}\n`)) !== protocol.source.source_tree) {
    fail("authorized revision differs from the preregistered source_tree");
  }
}

function validateTargetPlatform(target) {
  const actual =
    process.platform === "linux" && process.arch === "x64"
      ? "linux-x86_64"
      : process.platform === "darwin" && process.arch === "arm64"
        ? "macos-arm64"
        : `${process.platform}-${process.arch}`;
  if (actual !== target) {
    fail(`target ${target} cannot execute on ${actual}`);
  }
}

function targetDirectory(target) {
  return join(
    protocol.destinations.default_root,
    target === "linux-x86_64"
      ? protocol.destinations.linux_directory
      : protocol.destinations.macos_directory,
  );
}

function createOneShotDirectory(name) {
  const outputRoot = protocol.destinations.default_root;
  if (existsSync(outputRoot)) {
    validatePrivateDirectory(outputRoot, "qualification output root");
  } else {
    mkdirSync(outputRoot, { recursive: false, mode: 0o700 });
    validatePrivateDirectory(outputRoot, "qualification output root");
  }
  if (name.includes("/") || name === "." || name === "..") {
    fail(`qualification destination name is invalid: ${name}`);
  }
  const destination = join(outputRoot, name);
  if (existsSync(destination)) {
    fail(`qualification destination already exists: ${destination}`);
  }
  mkdirSync(destination, { recursive: false, mode: 0o700 });
  validatePrivateDirectory(destination, "qualification destination");
  return destination;
}

function qualificationEnvironment() {
  const environment = {
    ...process.env,
    DISTILL_COVERAGE_TOOLCHAIN: "nightly-2026-07-19",
    GIT_TERMINAL_PROMPT: "0",
  };
  for (const name of [
    "ANTHROPIC_API_KEY",
    "AWS_ACCESS_KEY_ID",
    "AWS_SECRET_ACCESS_KEY",
    "AZURE_OPENAI_API_KEY",
    "CARGO_ENCODED_RUSTFLAGS",
    "CARGO_TARGET_DIR",
    "CLAUDE_CODE_OAUTH_TOKEN",
    "GOOGLE_API_KEY",
    "OPENAI_API_KEY",
    "RUSTC_WRAPPER",
    "RUSTC_WORKSPACE_WRAPPER",
    "RUSTFLAGS",
  ]) {
    delete environment[name];
  }
  return environment;
}

function executeTarget(target, value) {
  validateTargetPlatform(target);
  const directoryName =
    target === "linux-x86_64"
      ? protocol.destinations.linux_directory
      : protocol.destinations.macos_directory;
  const destination = createOneShotDirectory(directoryName);
  const commands = [];
  const passedGates = new Set();
  let status = "GO";
  let firstDefect = null;
  const startedAt = new Date().toISOString();

  for (const command of protocol.commands[target]) {
    const arguments_ = command.argv.map((argument) =>
      argument.replaceAll("{target_directory}", destination),
    );
    const logName = join(destination, `${command.id}.log`);
    const log = openSync(logName, "wx", 0o600);
    const started = performance.now();
    const result = spawnSync(arguments_[0], arguments_.slice(1), {
      cwd: root,
      env: qualificationEnvironment(),
      stdio: ["ignore", log, log],
      timeout: command.maximum_wall_seconds * 1000,
      killSignal: "SIGKILL",
    });
    closeSync(log);
    const elapsedWallMs = Math.round(performance.now() - started);
    const passed = result.status === 0 && result.error === undefined;
    commands.push({
      id: command.id,
      argv: arguments_,
      maximum_wall_seconds: command.maximum_wall_seconds,
      elapsed_wall_ms: elapsedWallMs,
      exit_code: result.status,
      signal: result.signal,
      log_sha256: sha256(logName),
      status: passed ? "PASS" : "FAIL",
    });
    if (!passed) {
      status = "NO-GO";
      firstDefect = result.error
        ? `${command.id}: ${result.error.message}`
        : `${command.id}: exit ${result.status ?? "unknown"} signal ${result.signal ?? "none"}`;
      break;
    }
    for (const gate of command.proves) {
      passedGates.add(gate);
    }
  }

  const binaryPath = join(root, protocol.binary.path);
  const binarySha256 = existsSync(binaryPath) ? sha256(binaryPath) : null;
  if (status === "GO" && !/^[0-9a-f]{64}$/.test(binarySha256 ?? "")) {
    status = "NO-GO";
    firstDefect = "release binary digest is unavailable";
  }
  const requiredGates = Object.keys(protocol.gates);
  const missingGates = requiredGates.filter((gate) => !passedGates.has(gate));
  if (status === "GO" && missingGates.length > 0) {
    status = "NO-GO";
    firstDefect = `missing gate evidence: ${missingGates.join(", ")}`;
  }

  const receipt = {
    schema_version: "distill.architecture-hardening-receipt/v1",
    qualification_id: protocol.qualification_id,
    target,
    git_revision: value.source_revision,
    source_tree: protocol.source.source_tree,
    source_worktree_clean: true,
    binary_sha256: binarySha256,
    limits: protocol.limits,
    commands,
    gates: Object.fromEntries(
      requiredGates.map((gate) => [gate, passedGates.has(gate) ? "PASS" : "FAIL"]),
    ),
    first_defect: firstDefect,
    started_at: startedAt,
    completed_at: new Date().toISOString(),
    status,
  };
  writeFileSync(
    join(destination, "receipt.json"),
    `${JSON.stringify(receipt, null, 2)}\n`,
    { flag: "wx", mode: 0o600 },
  );
  process.stdout.write(`${join(destination, "receipt.json")}: ${status}\n`);
  if (status !== "GO") {
    process.exitCode = 1;
  }
}

function loadPlatformReceipt(target) {
  const directory = targetDirectory(target);
  const fileName = join(directory, "receipt.json");
  if (!existsSync(fileName)) {
    return { receipt: null, defect: `${target} receipt is missing` };
  }
  validatePrivateDirectory(directory, `${target} receipt directory`);
  validatePrivateFile(fileName, `${target} receipt`);
  try {
    return { receipt: readJson(fileName), defect: null };
  } catch (error) {
    return {
      receipt: null,
      defect: `${target} receipt is invalid JSON: ${error.message}`,
    };
  }
}

function aggregate(value) {
  const destination = createOneShotDirectory(
    protocol.destinations.aggregate_directory,
  );
  const loaded = Object.fromEntries(
    protocol.platforms.map((target) => [target, loadPlatformReceipt(target)]),
  );
  const defects = Object.values(loaded)
    .map((entry) => entry.defect)
    .filter(Boolean);
  const requiredGates = Object.keys(protocol.gates);
  for (const target of protocol.platforms) {
    const receipt = loaded[target].receipt;
    if (receipt === null) {
      continue;
    }
    if (
      receipt.schema_version !==
        "distill.architecture-hardening-receipt/v1" ||
      receipt.qualification_id !== protocol.qualification_id ||
      receipt.target !== target ||
      receipt.git_revision !== value.source_revision ||
      receipt.source_tree !== protocol.source.source_tree ||
      receipt.source_worktree_clean !== true ||
      receipt.status !== "GO" ||
      !/^[0-9a-f]{64}$/.test(receipt.binary_sha256 ?? "")
    ) {
      defects.push(`${target} receipt binding or status is invalid`);
    }
    const missingGates = requiredGates.filter(
      (gate) => receipt.gates?.[gate] !== "PASS",
    );
    if (missingGates.length > 0) {
      defects.push(`${target} missing gates: ${missingGates.join(", ")}`);
    }
  }

  const receipt = {
    schema_version: "distill.architecture-hardening-aggregate/v1",
    qualification_id: protocol.qualification_id,
    git_revision: value.source_revision,
    source_tree: protocol.source.source_tree,
    platform_receipts: Object.fromEntries(
      protocol.platforms.map((target) => [
        target,
        loaded[target].receipt === null
          ? null
          : {
              path: relative(
                protocol.destinations.default_root,
                join(targetDirectory(target), "receipt.json"),
              ),
              sha256: sha256(join(targetDirectory(target), "receipt.json")),
              binary_sha256: loaded[target].receipt.binary_sha256,
              status: loaded[target].receipt.status,
            },
      ]),
    ),
    decision_rule: protocol.aggregate_decision,
    defects,
    completed_at: new Date().toISOString(),
    status: defects.length === 0 ? "GO" : "NO-GO",
  };
  const output = join(destination, "receipt.json");
  writeFileSync(output, `${JSON.stringify(receipt, null, 2)}\n`, {
    flag: "wx",
    mode: 0o600,
  });
  process.stdout.write(`${output}: ${receipt.status}\n`);
  if (receipt.status !== "GO") {
    process.exitCode = 1;
  }
}

checkProtocolIdentity();
validateHistoricalClosure();

if (mode.action === "validate") {
  validatePreregisteredWorktree();
  process.stdout.write(
    `${protocol.qualification_id}: PREREGISTERED, execution blocked pending exact authorization\n`,
  );
} else {
  const value = authorization();
  validateCleanCandidate(value);
  if (mode.action === "execute") {
    executeTarget(mode.target, value);
  } else {
    aggregate(value);
  }
}
