//! Structural observation typing.
//!
//! A preservation policy derived from literal needles only recognizes the
//! fixtures it was calibrated on. This module derives the policy from the
//! *shape* of the observation instead: a bounded prefix is scored against
//! structural markers of the developer tool-output families, and the winning
//! shape selects a line policy that generalizes to inputs the reducer has never
//! seen.
//!
//! Detection inspects a bounded prefix rather than the full source, so a 10 MiB
//! observation costs the same classification work as a small one.

/// The observation families the projector recognizes. The labels match the real
/// corpus manifest (`evaluation/corpus/real/manifest.jsonl`), so classification
/// agreement is measurable against captured tool output.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Shape {
    BuildOutput,
    TestOutput,
    TypecheckLint,
    StackTrace,
    UnifiedDiff,
    ApiJson,
    SourceFile,
    /// The line-structured fallback: every ambiguous or unrecognized input.
    TerminalLog,
}

/// How a line ranks inside its shape. Selection accepts mandatory lines first,
/// then preferred lines, and only expansion reaches ordinary lines.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LineClass {
    Mandatory,
    Preferred,
    Ordinary,
}

/// The prefix classification inspects. Tool output announces its family in its
/// first screens: reading further cannot change a decision this bounded.
pub(crate) const CLASSIFICATION_PREFIX_BYTES: usize = 64 * 1024;
const CLASSIFICATION_PREFIX_LINES: usize = 512;

impl Shape {
    /// The versioned policy identifier this shape applies, which is also what
    /// the receipt records as the applied profile.
    pub(crate) fn profile(self) -> &'static str {
        match self {
            Self::BuildOutput => "build-output/v1",
            Self::TestOutput => "test-output/v1",
            Self::TypecheckLint => "typecheck-lint/v1",
            Self::StackTrace => "stack-trace/v1",
            Self::UnifiedDiff => "unified-diff/v1",
            Self::ApiJson => "api-json/v1",
            Self::SourceFile => "source-file/v1",
            Self::TerminalLog => "terminal-log/v1",
        }
    }

    /// Whether redundant lines of this shape may be collapsed by template.
    /// Source files, diffs, and JSON documents carry their meaning in structure
    /// that repeats by construction, so collapsing it would destroy the answer
    /// rather than the redundancy.
    pub(crate) fn aggregates(self) -> bool {
        matches!(
            self,
            Self::BuildOutput
                | Self::TestOutput
                | Self::TypecheckLint
                | Self::StackTrace
                | Self::TerminalLog
        )
    }

    /// Ranks one line under this shape. `raw` keeps the original bytes, because
    /// leading whitespace and the first character carry diff and source
    /// structure; `lowercase` is the normalized form the markers match.
    pub(crate) fn classify(self, raw: &str, lowercase: &str) -> LineClass {
        let trimmed = raw.trim_end_matches(['\n', '\r']);
        let text = lowercase.trim();
        match self {
            Self::BuildOutput | Self::TypecheckLint => {
                if is_error_diagnostic(text) || severity(text) == Some(Severity::Error) {
                    LineClass::Mandatory
                } else if is_warning_diagnostic(text)
                    || is_diagnostic_detail(text)
                    || severity(text) == Some(Severity::Warning)
                {
                    LineClass::Preferred
                } else {
                    LineClass::Ordinary
                }
            }
            Self::TestOutput => {
                if is_test_failure(text) {
                    LineClass::Mandatory
                } else if is_test_summary(text) {
                    LineClass::Preferred
                } else {
                    LineClass::Ordinary
                }
            }
            Self::StackTrace => {
                if is_exception_header(text) {
                    LineClass::Mandatory
                } else if is_frame(text) && !is_library_path(text) {
                    LineClass::Preferred
                } else {
                    LineClass::Ordinary
                }
            }
            // A diff carries no fact whose loss makes the projection a lie, and
            // the headers of a large diff exceed any budget on their own, so
            // structure is preferred rather than mandatory: headers and changed
            // lines are kept before unchanged context.
            Self::UnifiedDiff => {
                if is_diff_header(trimmed) || is_hunk_header(trimmed) || is_diff_change(trimmed) {
                    LineClass::Preferred
                } else {
                    LineClass::Ordinary
                }
            }
            Self::ApiJson => {
                if is_json_skeleton(trimmed, text) {
                    LineClass::Preferred
                } else {
                    LineClass::Ordinary
                }
            }
            Self::SourceFile => {
                if is_declaration(trimmed) {
                    LineClass::Preferred
                } else {
                    LineClass::Ordinary
                }
            }
            Self::TerminalLog => {
                if severity(text).is_some() {
                    LineClass::Preferred
                } else {
                    LineClass::Ordinary
                }
            }
        }
    }
}

/// A JSON line longer than this carries a value rather than skeleton, so it is
/// elided before shorter structural lines.
const MAX_JSON_STRUCTURAL_BYTES: usize = 120;
/// The nesting a line may sit at and still describe the document rather than one
/// of its records. Two spaces is the outer level of the dominant convention;
/// a document indented more deeply keeps its brackets and falls back to
/// boundary expansion, which is what an unrecognized structure gets anyway.
const MAX_JSON_SKELETON_INDENT: usize = 2;

/// The skeleton of a JSON document: its outer keys and containers, short enough
/// to be structure rather than payload.
fn is_json_skeleton(line: &str, lowercase: &str) -> bool {
    let body = line.trim_start();
    line.len() - body.len() <= MAX_JSON_SKELETON_INDENT
        && body.len() <= MAX_JSON_STRUCTURAL_BYTES
        && is_json_structural(lowercase)
}

/// Detects the shape of an observation from a bounded prefix.
///
/// The order of the tests is the discrimination order, from the least ambiguous
/// structure to the most: a document that opens as JSON is JSON, a hunk header
/// is only ever a diff, and everything that matches nothing is line-structured.
pub(crate) fn detect(text: &str) -> Shape {
    let prefix = bounded_prefix(text);
    let features = Features::of(prefix);
    let lines = features.lines.max(1);

    if starts_document(prefix, ['{', '[']) && features.json_structural * 5 >= lines * 3 {
        return Shape::ApiJson;
    }
    if features.hunk_headers >= 1 && features.diff_headers >= 1 {
        return Shape::UnifiedDiff;
    }
    if features.frames >= 2
        && features.exceptions >= 1
        && features.tests < 2
        && (features.frames >= 3 || features.project_frames >= 1)
    {
        return Shape::StackTrace;
    }
    if features.tests >= 2 || (features.tests >= 1 && features.tests * 4 >= lines) {
        return Shape::TestOutput;
    }
    if features.diagnostics >= 1 || features.path_headers >= 1 {
        // A build that stopped on an error is build output; the same diagnostic
        // grammar without a build is a type check or a lint run.
        if features.progress >= 1 && features.errors >= 1 {
            return Shape::BuildOutput;
        }
        return Shape::TypecheckLint;
    }
    // A formatter reports changed lines without hunk headers.
    if features.hunk_headers == 0 && features.diff_changes * 2 >= lines {
        return Shape::TypecheckLint;
    }
    if features.progress * 3 >= lines {
        return Shape::BuildOutput;
    }
    if prefix.starts_with("#!") || features.code * 5 >= lines * 2 {
        return Shape::SourceFile;
    }
    Shape::TerminalLog
}

/// The longest prefix of `text` that stays inside the classification bound and
/// on a character boundary.
fn bounded_prefix(text: &str) -> &str {
    let mut end = CLASSIFICATION_PREFIX_BYTES.min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

fn starts_document(text: &str, openers: [char; 2]) -> bool {
    text.trim_start()
        .starts_with(|first: char| openers.contains(&first))
}

#[derive(Debug, Default)]
struct Features {
    lines: usize,
    json_structural: usize,
    hunk_headers: usize,
    diff_headers: usize,
    diff_changes: usize,
    frames: usize,
    project_frames: usize,
    exceptions: usize,
    tests: usize,
    diagnostics: usize,
    errors: usize,
    path_headers: usize,
    progress: usize,
    code: usize,
}

impl Features {
    fn of(prefix: &str) -> Self {
        let mut features = Self::default();
        for raw in prefix.lines().take(CLASSIFICATION_PREFIX_LINES) {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                continue;
            }
            features.lines += 1;
            let text = trimmed.to_ascii_lowercase();
            let text = text.as_str();

            if is_json_structural(text) {
                features.json_structural += 1;
            }
            if is_hunk_header(raw) {
                features.hunk_headers += 1;
            }
            if is_diff_header(raw) {
                features.diff_headers += 1;
            } else if is_diff_change(raw) {
                features.diff_changes += 1;
            }
            if is_frame(text) {
                features.frames += 1;
                if !is_library_path(text) {
                    features.project_frames += 1;
                }
            }
            if is_exception_header(text) {
                features.exceptions += 1;
            }
            if is_test_failure(text) || is_test_summary(text) || is_test_case(text) {
                features.tests += 1;
            }
            if is_error_diagnostic(text) {
                features.diagnostics += 1;
                features.errors += 1;
            } else if is_warning_diagnostic(text) || is_diagnostic_detail(text) {
                features.diagnostics += 1;
            }
            if is_path_header(trimmed) {
                features.path_headers += 1;
            }
            if is_progress(text) {
                features.progress += 1;
            }
            if is_code(trimmed) {
                features.code += 1;
            }
        }
        features
    }
}

fn is_json_structural(text: &str) -> bool {
    text.starts_with('{')
        || text.starts_with('[')
        || matches!(text, "}" | "]" | "}," | "],")
        || (text.starts_with('"') && text.contains("\": "))
}

fn is_hunk_header(raw: &str) -> bool {
    let text = raw.trim_end();
    text.starts_with("@@ ") && text.matches("@@").count() >= 2
}

fn is_diff_header(raw: &str) -> bool {
    let text = raw.trim_end();
    text.starts_with("diff --git ")
        || text.starts_with("--- ")
        || text.starts_with("+++ ")
        || text.starts_with("index ")
}

fn is_diff_change(raw: &str) -> bool {
    let text = raw.trim_end();
    text.len() > 1 && (text.starts_with('+') || text.starts_with('-'))
}

/// A stack frame in the grammars the supported runtimes emit: `at symbol
/// (path:line)`, `File "path", line N`, and the numbered Rust backtrace form.
fn is_frame(text: &str) -> bool {
    if let Some(rest) = text.strip_prefix("at ") {
        return !rest.trim_start().is_empty();
    }
    if let Some(rest) = text.strip_prefix("file \"")
        && rest.contains("\", line ")
    {
        return true;
    }
    let digits = text.bytes().take_while(u8::is_ascii_digit).count();
    digits > 0 && text[digits..].starts_with(": ")
}

/// Package, runtime, and toolchain directories: a frame inside one of them is
/// library context rather than the code under a configured acquisition root.
const LIBRARY_MARKERS: &[&str] = &[
    "node_modules",
    "node:internal",
    "node:",
    "/rustc/",
    ".cargo/registry",
    "site-packages",
    "dist-packages",
    "/usr/lib",
    "<frozen",
    "library/std",
    "library/core",
    "library/alloc",
    "internal/",
];

fn is_library_path(text: &str) -> bool {
    LIBRARY_MARKERS.iter().any(|marker| text.contains(marker))
}

fn is_exception_header(text: &str) -> bool {
    if text.starts_with("traceback (most recent call last)")
        || text.starts_with("stack backtrace:")
        || text.starts_with("uncaught ")
        || (text.starts_with("thread ") && text.contains("panicked at"))
    {
        return true;
    }
    // `SomeError: message` and `module.SomeException` name the failure itself.
    let head = text.split_once(": ").map_or(text, |(head, _)| head);
    !head.is_empty()
        && head
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.'))
        && (head.ends_with("error") || head.ends_with("exception"))
}

fn is_test_failure(text: &str) -> bool {
    text.starts_with("not ok ")
        || text.starts_with("(fail)")
        || text.starts_with("failures:")
        || text.starts_with("failed (")
        || (text.contains(" ... ") && ends_with_outcome(text, &["fail", "failed", "error"]))
        || (text.starts_with("test ") && text.ends_with("failed"))
        || text.starts_with("assertionerror")
}

fn is_test_summary(text: &str) -> bool {
    text.starts_with("test result:")
        || text.starts_with("tap version")
        || (text.starts_with("running ") && text.contains(" test"))
        || (text.starts_with("ran ") && text.contains(" test"))
        || text.starts_with("# tests")
        || text.starts_with("# pass")
        || text.starts_with("# fail")
        || text.starts_with("# subtest")
        || text
            .strip_prefix("1..")
            .is_some_and(|count| !count.is_empty() && count.bytes().all(|b| b.is_ascii_digit()))
}

fn is_test_case(text: &str) -> bool {
    text.starts_with("(pass)")
        || text
            .strip_prefix("ok ")
            .is_some_and(|rest| rest.starts_with(|first: char| first.is_ascii_digit()))
        || text.ends_with(": test")
        || (text.contains(" ... ") && ends_with_outcome(text, &["ok", "ignored", "skipped"]))
}

fn ends_with_outcome(text: &str, outcomes: &[&str]) -> bool {
    text.rsplit(' ')
        .next()
        .is_some_and(|last| outcomes.contains(&last))
}

fn is_error_diagnostic(text: &str) -> bool {
    severity_prefixed(text, "error") || short_diagnostic(text, ": error")
}

fn is_warning_diagnostic(text: &str) -> bool {
    severity_prefixed(text, "warning") || short_diagnostic(text, ": warning")
}

/// `error: message`, `error[E0308]: message`, and the same forms for warnings.
fn severity_prefixed(text: &str, severity: &str) -> bool {
    let Some(rest) = text.strip_prefix(severity) else {
        return false;
    };
    let rest = match rest.split_once(']') {
        Some((code, tail)) if code.starts_with('[') => tail,
        _ => rest,
    };
    rest.starts_with(':')
}

/// `path:line:column: error[E0308]: message`, the short diagnostic format.
fn short_diagnostic(text: &str, severity: &str) -> bool {
    let Some(index) = text.find(severity) else {
        return false;
    };
    let head = &text[..index];
    head.split(':')
        .skip(1)
        .any(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()))
}

fn is_diagnostic_detail(text: &str) -> bool {
    text.starts_with("--> ") || text.starts_with("= help:") || text.starts_with("= note:")
}

/// A bare `path.ext:line` or `path.ext:line:column` location line, which opens a
/// diagnostic that the tool prints without a severity prefix.
fn is_path_header(trimmed: &str) -> bool {
    let mut parts = trimmed.trim_end_matches(':').split(':');
    let Some(path) = parts.next() else {
        return false;
    };
    if path.contains(' ') || !path.contains('.') {
        return false;
    }
    let mut positions = 0;
    for part in parts {
        if part.is_empty() || !part.bytes().all(|byte| byte.is_ascii_digit()) {
            return false;
        }
        positions += 1;
    }
    (1..=2).contains(&positions)
}

const PROGRESS_VERBS: &[&str] = &[
    "compiling",
    "checking",
    "fresh",
    "finished",
    "building",
    "built",
    "compiled",
    "downloading",
    "downloaded",
    "installing",
    "packaging",
    "bundled",
    "bundling",
    "updating",
    "removing",
    "generated",
];

fn is_progress(text: &str) -> bool {
    text.split_once(' ')
        .is_some_and(|(verb, rest)| !rest.is_empty() && PROGRESS_VERBS.contains(&verb))
}

const DECLARATION_KEYWORDS: &[&str] = &[
    "fn ",
    "pub ",
    "impl ",
    "struct ",
    "enum ",
    "trait ",
    "mod ",
    "type ",
    "const ",
    "static ",
    "use ",
    "class ",
    "def ",
    "function ",
    "export ",
    "import ",
    "interface ",
    "package ",
    "namespace ",
    "async fn",
    "#!",
];

/// The indentation a declaration may carry and still describe the outline of a
/// file rather than the body of one item.
const MAX_DECLARATION_INDENT: usize = 4;

/// A declaration at the outer levels of a source file: the lines that answer
/// "what does this file define". Deeper nesting is body detail, which expansion
/// reaches contiguously around the declarations it already retained.
fn is_declaration(line: &str) -> bool {
    let body = line.trim_start();
    if line.len() - body.len() > MAX_DECLARATION_INDENT {
        return false;
    }
    let lowercase = body.to_ascii_lowercase();
    DECLARATION_KEYWORDS
        .iter()
        .any(|keyword| lowercase.starts_with(keyword))
}

const CODE_KEYWORDS: &[&str] = &[
    "fn ",
    "pub ",
    "use ",
    "let ",
    "const ",
    "var ",
    "impl ",
    "struct ",
    "enum ",
    "mod ",
    "import ",
    "export ",
    "function ",
    "class ",
    "def ",
    "return",
    "if ",
    "elif ",
    "else",
    "for ",
    "while ",
    "match ",
    "async ",
    "await ",
    "try",
    "catch",
    "throw ",
    "case ",
    "switch ",
    "type ",
    "trait ",
    "interface ",
    "package ",
    "namespace ",
    "local ",
    "echo ",
    "set -",
    "exit ",
    "then",
    "done",
    "fi",
    "esac",
    "do ",
    "cd ",
    "#[",
    "#!",
    "//",
    "/*",
    "* ",
    "*/",
    "@",
];

/// Whether a line reads as program text rather than as a report line.
fn is_code(trimmed: &str) -> bool {
    if trimmed.ends_with(['{', '}', ';', '(', ')', ',', '\\', '=', ':', '[', ']', '|']) {
        return true;
    }
    let lowercase = trimmed.to_ascii_lowercase();
    if is_assignment(&lowercase) {
        return true;
    }
    CODE_KEYWORDS
        .iter()
        .any(|keyword| lowercase.starts_with(keyword))
}

/// `name = value`, `name: value`, and `name() = value`: the assignment forms
/// that dominate configuration-shaped source.
fn is_assignment(lowercase: &str) -> bool {
    let Some((name, value)) = lowercase.split_once('=') else {
        return false;
    };
    let name = name.trim_end_matches(['(', ')', '+', ':']).trim_end();
    !name.is_empty()
        && !value.starts_with('=')
        && !name.contains(' ')
        && name.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'[' | b']' | b'"')
        })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Severity {
    Error,
    Warning,
}

/// The tokens a severity may sit behind in a log line: a timestamp, a level
/// prefix, or a stream label. Beyond that the word is prose, not a level.
const MAX_SEVERITY_TOKEN: usize = 3;

/// The severity a line announces as a bare level token, which is how logs mark
/// what went wrong when they carry no diagnostic grammar.
fn severity(text: &str) -> Option<Severity> {
    text.split_whitespace()
        .take(MAX_SEVERITY_TOKEN)
        .find_map(
            |token| match token.trim_matches(['[', ']', ':', '(', ')']) {
                "error" | "errors" | "err" | "fatal" | "failed" | "failure" | "panic"
                | "panicked" | "abort" | "aborted" => Some(Severity::Error),
                "warn" | "warning" | "warnings" => Some(Severity::Warning),
                _ => None,
            },
        )
}
