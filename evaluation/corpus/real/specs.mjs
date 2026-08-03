import { mkdirSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";

/**
 * Capture specifications for the real tool-output corpus. Every fixture is the
 * exact output of one executable invoked with literal argv: no shell parsing,
 * no synthesized text. `{work}` expands to the ephemeral capture work root.
 *
 * Repository-scoped git commands name explicit revisions so the originating
 * command stays reproducible from any clone.
 */
export const SPECS = [
  // ----- build-output -------------------------------------------------------
  {
    id: "build-cargo-release-verbose",
    shape: "build-output",
    description: "Verbose locked release build of the Distill root crate",
    command: ["cargo", "build", "--locked", "--release", "--verbose"],
    cwd: "repo",
  },
  {
    id: "build-cargo-check-short",
    shape: "build-output",
    description: "Cold locked check across every target and dependency, in short message format",
    command: [
      "cargo",
      "check",
      "--locked",
      "--offline",
      "--all-targets",
      "--message-format",
      "short",
    ],
    cwd: "repo",
    env: { CARGO_TARGET_DIR: "{work}/cargo-target" },
  },
  {
    id: "build-cargo-broken-crate",
    shape: "build-output",
    description: "Cargo build of a crate with a type error and a missing function",
    command: ["cargo", "build", "--offline"],
    cwd: "rust-broken",
  },
  {
    id: "build-cargo-broken-short",
    shape: "build-output",
    description: "Same failing build rendered in short message format",
    command: ["cargo", "build", "--offline", "--message-format", "short"],
    cwd: "rust-broken",
  },
  {
    id: "build-cargo-release-crate",
    shape: "build-output",
    description: "Release build of a small crate that compiles cleanly",
    command: ["cargo", "build", "--offline", "--release"],
    cwd: "rust-tests",
  },
  {
    id: "build-bun-bundle",
    shape: "build-output",
    description: "Bun bundler output for a two-module entry point",
    command: ["bun", "build", "./index.js", "--outdir", "./dist"],
    cwd: "js",
  },

  // ----- test-output --------------------------------------------------------
  {
    id: "test-cargo-failing",
    shape: "test-output",
    description: "Cargo test run with three passing and one failing assertion",
    command: ["cargo", "test", "--offline"],
    cwd: "rust-tests",
  },
  {
    id: "test-cargo-failing-nocapture",
    shape: "test-output",
    description: "Same cargo test run with captured output disabled",
    command: ["cargo", "test", "--offline", "--", "--nocapture"],
    cwd: "rust-tests",
  },
  {
    id: "test-cargo-list",
    shape: "test-output",
    description: "Distill library test inventory listed by the harness",
    command: ["cargo", "test", "--locked", "--lib", "--", "--list"],
    cwd: "repo",
  },
  {
    id: "test-bun-failing",
    shape: "test-output",
    description: "Bun test run with a failing expectation",
    command: ["bun", "test", "budget.test.js"],
    cwd: "js",
  },
  {
    id: "test-node-failing",
    shape: "test-output",
    description: "Node test runner TAP output with a failing assertion",
    command: ["node", "--test", "node-tests"],
    cwd: "js",
  },
  {
    id: "test-python-unittest",
    shape: "test-output",
    description: "Verbose Python unittest run with a failing tolerance check",
    command: ["python3", "-m", "unittest", "-v", "test_budget"],
    cwd: "py",
  },

  // ----- typecheck-lint -----------------------------------------------------
  {
    id: "lint-cargo-clippy-repo",
    shape: "typecheck-lint",
    description: "Cold clippy pass across every Distill target, in short message format",
    command: [
      "cargo",
      "clippy",
      "--locked",
      "--offline",
      "--all-targets",
      "--message-format",
      "short",
    ],
    cwd: "repo",
    env: { CARGO_TARGET_DIR: "{work}/clippy-target" },
  },
  {
    id: "lint-cargo-clippy-crate",
    shape: "typecheck-lint",
    description: "Clippy diagnostics for a crate with three lint violations",
    command: ["cargo", "clippy", "--offline"],
    cwd: "rust-lintable",
  },
  {
    id: "lint-cargo-clippy-crate-short",
    shape: "typecheck-lint",
    description: "Same clippy diagnostics in short message format",
    command: ["cargo", "clippy", "--offline", "--message-format", "short"],
    cwd: "rust-lintable",
  },
  {
    id: "lint-rustc-diagnostics",
    shape: "typecheck-lint",
    description: "Direct rustc diagnostics with spans, carets, and help notes",
    command: ["rustc", "--edition", "2024", "--emit", "metadata", "src/main.rs"],
    cwd: "rust-broken",
  },
  {
    id: "lint-cargo-fmt-check",
    shape: "typecheck-lint",
    description: "Formatting violations reported by cargo fmt --check",
    command: ["cargo", "fmt", "--check"],
    cwd: "rust-misformatted",
  },
  {
    id: "lint-node-check",
    shape: "typecheck-lint",
    description: "Node syntax check rejecting a malformed module",
    command: ["node", "--check", "bad-syntax.js"],
    cwd: "js",
  },

  // ----- stack-trace --------------------------------------------------------
  {
    id: "trace-rust-panic",
    shape: "stack-trace",
    description: "Rust panic with a single-level backtrace",
    command: ["cargo", "run", "--offline", "--quiet"],
    cwd: "rust-panic",
    env: { RUST_BACKTRACE: "1" },
  },
  {
    id: "trace-rust-panic-full",
    shape: "stack-trace",
    description: "Rust panic with a full backtrace including runtime frames",
    command: ["cargo", "run", "--offline", "--quiet"],
    cwd: "rust-panic",
    env: { RUST_BACKTRACE: "full" },
  },
  {
    id: "trace-node-throw",
    shape: "stack-trace",
    description: "Node TypeError with a three-frame stack",
    command: ["node", "throwing.js"],
    cwd: "js",
  },
  {
    id: "trace-node-async",
    shape: "stack-trace",
    description: "Node rejection carrying a cause chain",
    command: ["node", "async-throwing.mjs"],
    cwd: "js",
  },
  {
    id: "trace-bun-throw",
    shape: "stack-trace",
    description: "Bun runtime error with its own stack rendering",
    command: ["bun", "run", "throwing.js"],
    cwd: "js",
  },
  {
    id: "trace-python",
    shape: "stack-trace",
    description: "Python chained traceback across a comprehension frame",
    command: ["python3", "traceback_demo.py"],
    cwd: "py",
  },

  // ----- unified-diff -------------------------------------------------------
  {
    id: "diff-git-two-commits",
    shape: "unified-diff",
    description: "Unified diff between two Distill refactor commits",
    command: ["git", "--no-pager", "diff", "8e6626d..5f228cd"],
    cwd: "repo",
  },
  {
    id: "diff-git-show",
    shape: "unified-diff",
    description: "Commit metadata followed by its unified diff",
    command: ["git", "--no-pager", "show", "5f228cd"],
    cwd: "repo",
  },
  {
    id: "diff-git-log-patch",
    shape: "unified-diff",
    description: "Single-commit log rendered with its patch",
    command: ["git", "--no-pager", "log", "-p", "-1", "e06dbf9"],
    cwd: "repo",
  },
  {
    id: "diff-git-scoped",
    shape: "unified-diff",
    description: "Diff of four commits restricted to the source tree",
    command: ["git", "--no-pager", "diff", "2272231..5f228cd", "--", "src/"],
    cwd: "repo",
  },
  {
    id: "diff-git-format-patch",
    shape: "unified-diff",
    description: "Mail-formatted patch for one commit",
    command: ["git", "--no-pager", "format-patch", "--stdout", "-1", "5f228cd"],
    cwd: "repo",
  },
  {
    id: "diff-unified-files",
    shape: "unified-diff",
    description: "Plain diff -u between two configuration revisions",
    command: ["diff", "-u", "before.txt", "after.txt"],
    cwd: "diff",
  },

  // ----- api-json -----------------------------------------------------------
  {
    id: "json-cargo-metadata-shallow",
    shape: "api-json",
    description: "Cargo metadata for the root package without dependencies",
    command: ["cargo", "metadata", "--locked", "--format-version", "1", "--no-deps"],
    cwd: "repo",
  },
  {
    id: "json-cargo-metadata-full",
    shape: "api-json",
    description: "Cargo metadata including the resolved dependency graph",
    command: ["cargo", "metadata", "--locked", "--format-version", "1"],
    cwd: "repo",
  },
  {
    id: "json-cargo-build-messages",
    shape: "api-json",
    description: "Cargo JSON diagnostic stream for a failing build",
    command: ["cargo", "build", "--offline", "--message-format", "json"],
    cwd: "rust-broken",
  },
  {
    id: "json-cli-conformance",
    shape: "api-json",
    description: "Frozen CLI conformance matrix document",
    command: ["cat", "docs/integrations/cli-conformance-v2.json"],
    cwd: "repo",
  },
  {
    id: "json-release-protocol",
    shape: "api-json",
    description: "Closed architecture-hardening qualification protocol",
    command: ["cat", "evaluation/release/architecture-hardening-v6-protocol.json"],
    cwd: "repo",
  },
  {
    id: "json-distill-status",
    shape: "api-json",
    description: "Distill store status envelope from a temporary store",
    command: [
      "target/release/distill",
      "--store",
      "{work}/store/artifacts.db",
      "status",
      "--json",
    ],
    cwd: "repo",
  },

  // ----- source-file --------------------------------------------------------
  {
    id: "source-artifact-rs",
    shape: "source-file",
    description: "Distill artifact store module, the PRD reference source file",
    command: ["cat", "src/artifact.rs"],
    cwd: "repo",
  },
  {
    id: "source-projection-rs",
    shape: "source-file",
    description: "Distill projection module",
    command: ["cat", "src/projection.rs"],
    cwd: "repo",
  },
  {
    id: "source-mcp-rs",
    shape: "source-file",
    description: "Distill MCP surface adapter",
    command: ["cat", "src/mcp.rs"],
    cwd: "repo",
  },
  {
    id: "source-cli-rs",
    shape: "source-file",
    description: "Distill CLI surface adapter",
    command: ["cat", "src/cli.rs"],
    cwd: "repo",
  },
  {
    id: "source-corpus-lib-mjs",
    shape: "source-file",
    description: "JavaScript corpus validator module",
    command: ["cat", "evaluation/corpus/lib.mjs"],
    cwd: "repo",
  },
  {
    id: "source-check-native-sh",
    shape: "source-file",
    description: "Consolidated native gate shell script",
    command: ["cat", "scripts/check-native.sh"],
    cwd: "repo",
  },

  // ----- terminal-log -------------------------------------------------------
  {
    id: "log-git-log-stat",
    shape: "terminal-log",
    description: "Forty commits with diffstats, the PRD reference command log",
    command: ["git", "--no-pager", "log", "--stat", "-40", "5f228cd"],
    cwd: "repo",
  },
  {
    id: "log-git-log-oneline",
    shape: "terminal-log",
    description: "One hundred commit subjects in oneline format",
    command: ["git", "--no-pager", "log", "--oneline", "-100", "5f228cd"],
    cwd: "repo",
  },
  {
    id: "log-ls-recursive",
    shape: "terminal-log",
    description: "Recursive long listing of the source tree",
    command: ["ls", "-laR", "src"],
    cwd: "repo",
  },
  {
    id: "log-find-sources",
    shape: "terminal-log",
    description: "Path listing of every Rust source file",
    command: ["find", "src", "tests", "-name", "*.rs", "-type", "f"],
    cwd: "repo",
  },
  {
    id: "log-wc-sources",
    shape: "terminal-log",
    description: "Line counts across the largest Distill modules",
    command: [
      "wc",
      "-l",
      "src/artifact.rs",
      "src/projection.rs",
      "src/cli.rs",
      "src/mcp.rs",
      "src/codex.rs",
    ],
    cwd: "repo",
  },
  {
    id: "log-cargo-tree",
    shape: "terminal-log",
    description: "Resolved dependency tree of the root crate",
    command: ["cargo", "tree", "--locked", "--offline"],
    cwd: "repo",
  },
];

const CARGO_MANIFEST = (name) => `[package]
name = "${name}"
version = "0.1.0"
edition = "2024"

[workspace]
`;

const FILES = {
  "rust-broken/Cargo.toml": CARGO_MANIFEST("broken-build"),
  "rust-broken/src/main.rs": `fn total(values: &[u32]) -> u32 {
    values.iter().sum()
}

fn main() {
    let values = vec![1_u32, 2, 3];
    let label: u32 = "three";
    println!("{} {}", total(&values), label);
    println!("{}", missing_helper(&values));
}
`,
  "rust-lintable/Cargo.toml": CARGO_MANIFEST("lintable-crate"),
  "rust-lintable/src/main.rs": `fn describe(label: &String) -> String {
    label.clone()
}

fn main() {
    let values: Vec<u32> = vec![1, 2, 3];
    if values.len() == 0 {
        println!("no values");
    }
    let label = String::from("distill");
    println!("{}", describe(&label));
    let mut total = 0;
    for index in 0..values.len() {
        total += values[index];
    }
    println!("{total}");
}
`,
  "rust-tests/Cargo.toml": CARGO_MANIFEST("checksum-crate"),
  "rust-tests/src/lib.rs": `pub fn checksum(values: &[u8]) -> u32 {
    values.iter().map(|value| u32::from(*value)).sum()
}

pub fn label(value: u32) -> String {
    format!("checksum-{value}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checksum_of_empty_input_is_zero() {
        assert_eq!(checksum(&[]), 0);
    }

    #[test]
    fn checksum_sums_every_byte() {
        assert_eq!(checksum(&[1, 2, 3]), 6);
    }

    #[test]
    fn label_names_its_checksum() {
        assert_eq!(label(6), "checksum-6");
    }

    #[test]
    fn checksum_saturates_high_bytes() {
        println!("checking high byte saturation");
        assert_eq!(checksum(&[255, 1]), 1);
    }
}
`,
  "rust-panic/Cargo.toml": CARGO_MANIFEST("panicking-crate"),
  "rust-panic/src/main.rs": `fn parse_budget(raw: &str) -> u64 {
    raw.parse()
        .expect("budget must be an unsigned integer of visible units")
}

fn payload_budget(raw: &str) -> u64 {
    parse_budget(raw) - 450
}

fn main() {
    println!("{}", payload_budget("2250"));
    println!("{}", payload_budget("2250 tokens"));
}
`,
  "rust-misformatted/Cargo.toml": CARGO_MANIFEST("misformatted-crate"),
  "rust-misformatted/src/main.rs": `fn main( ) {
let budgets = vec![ 2250,1800,450 ];
    for budget in budgets { println!("{}",budget); }
}
`,
  "js/index.js": `import { payloadBudget } from "./plan.js";

console.log(payloadBudget(2250, 450));
`,
  "js/plan.js": `export function payloadBudget(total, reserved) {
  if (reserved > total) {
    throw new RangeError("reserved envelope exceeds the total visible limit");
  }
  return total - reserved;
}
`,
  "js/throwing.js": `function loadBudget(raw) {
  if (typeof raw !== "number") {
    throw new TypeError(\`budget must be a number, received \${typeof raw}\`);
  }
  return raw;
}

function planProjection(request) {
  return loadBudget(request.budget);
}

console.log(planProjection({ budget: "1800" }));
`,
  "js/async-throwing.mjs": `async function readManifest() {
  throw new Error("real corpus manifest digest mismatch", {
    cause: new Error("sha256 disagreed for fixture source-artifact-rs"),
  });
}

async function verifyCorpus() {
  await readManifest();
}

await verifyCorpus();
`,
  "js/bad-syntax.js": `function payloadBudget(total, reserved) {
  if (reserved > total) {
    return 0;

  return total - reserved;
}

module.exports = { payloadBudget };
`,
  "js/budget.test.js": `import { expect, test } from "bun:test";

test("payload budget subtracts the reserved envelope", () => {
  expect(2250 - 450).toBe(1800);
});

test("a source file can overflow the payload budget", () => {
  expect(2799).toBeGreaterThan(1800);
});

test("the frozen baseline spends its payload budget", () => {
  expect(7).toBe(1800);
});
`,
  "js/node-tests/budget.test.mjs": `import assert from "node:assert/strict";
import { test } from "node:test";

test("payload budget subtracts the reserved envelope", () => {
  assert.equal(2250 - 450, 1800);
});

test("a source file can overflow the payload budget", () => {
  assert.ok(2799 > 1800);
});

test("the frozen baseline spends its payload budget", () => {
  assert.equal(Math.round((7 / 1800) * 100), 85);
});
`,
  "py/traceback_demo.py": `def decode(record):
    return record["visible_count"]


def summarize(records):
    return [decode(record) for record in records]


def main():
    try:
        summarize([{"visible_count": 7}, {"original_count": 2799}])
    except KeyError as error:
        raise RuntimeError("projection record is incomplete") from error


main()
`,
  "py/test_budget.py": `import unittest


class BudgetTest(unittest.TestCase):
    def test_payload_budget(self):
        self.assertEqual(2250 - 450, 1800)

    def test_reserved_envelope_is_positive(self):
        self.assertGreater(450, 0)

    def test_baseline_utilization(self):
        self.assertAlmostEqual(7 / 1800 * 100, 85.0, places=1)
`,
  "diff/before.txt": `[budget]
total_visible_limit = 2250
reserved_envelope = 450
unit = "tokens"
profile = "plain-text/v1"

[retention]
ttl_seconds = 3600

[store]
path = "artifacts.db"
mode = "0600"
`,
  "diff/after.txt": `[budget]
total_visible_limit = 2250
reserved_envelope = 450
unit = "tokens"
profile = "auto/v1"
focus = "why did the build fail"

[retention]
ttl_seconds = 7200

[store]
path = "artifacts.db"
mode = "0600"
wal = true
`,
};

export function scaffold(workRoot) {
  for (const [relative, contents] of Object.entries(FILES)) {
    const path = join(workRoot, relative);
    mkdirSync(dirname(path), { recursive: true, mode: 0o700 });
    writeFileSync(path, contents, { encoding: "utf8", mode: 0o600 });
  }
}
