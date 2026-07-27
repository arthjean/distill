use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use distill::{
    ArtifactRef, Budget, ByteString, CL100K_PROFILE, CONTRACT_VERSION, CountUnit, Engine,
    EngineConfig, Request, Retention, Source,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    error::Error,
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        Arc, Barrier,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tiktoken_rs::cl100k_base_singleton;

const WARM_UP_RUNS: usize = 2;
const MEASURED_RUNS: usize = 20;

#[derive(Debug)]
struct Options {
    binary: PathBuf,
    corpus_root: PathBuf,
    fuzz_binary: PathBuf,
    fuzz_evidence: PathBuf,
    suite_evidence: PathBuf,
    output: PathBuf,
}

#[derive(Debug, Deserialize)]
struct Fixture {
    id: String,
    category: String,
    source: Value,
    source_sha256: String,
    budget_profile: String,
    annotations: Annotations,
}

#[derive(Debug, Deserialize)]
struct Annotations {
    p0: Vec<Fact>,
    p1: Vec<Fact>,
}

#[derive(Debug, Deserialize)]
struct Fact {
    needle_base64: String,
}

#[derive(Debug, Deserialize)]
struct BudgetFile {
    profiles: Vec<BudgetProfile>,
}

#[derive(Clone, Debug, Deserialize)]
struct BudgetProfile {
    id: String,
    unit: String,
    total_visible_limit: u64,
    reserved_envelope: u64,
    token_profile: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct FuzzEvidence {
    schema_version: String,
    target: String,
    mode: String,
    result: String,
    git_revision: String,
    binary_sha256: String,
    cpu_seconds: f64,
    wall_seconds: f64,
    timeout_seconds: u64,
    workers: u64,
    started_at: String,
    completed_at: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct SuiteEvidence {
    schema_version: String,
    target: String,
    profile: String,
    result: String,
    git_revision: String,
    source_tree: String,
    binary_sha256: String,
    commands: Vec<String>,
    completed_at: String,
}

#[derive(Debug, Serialize)]
struct Report {
    schema_version: &'static str,
    generated_at: String,
    git_revision: String,
    reference_machine: Machine,
    protocol: Protocol,
    corpus: CorpusReport,
    recovery: RecoveryReport,
    performance: PerformanceReport,
    fuzz: FuzzEvidence,
    linux_release: SuiteEvidence,
    critical_failures: CriticalFailures,
    status: &'static str,
}

#[derive(Debug, Serialize)]
struct Machine {
    os: &'static str,
    architecture: &'static str,
    kernel: String,
    cpu: String,
    memory_bytes: u64,
}

#[derive(Debug, Serialize)]
struct Protocol {
    warm_up_runs: usize,
    measured_runs: usize,
    percentile: &'static str,
    tokenizer: &'static str,
}

#[derive(Debug, Serialize)]
struct CorpusReport {
    fixtures: usize,
    p0_preserved: u64,
    p0_total: u64,
    p0_recall_percent: f64,
    p1_preserved: u64,
    p1_total: u64,
    p1_recall_percent: f64,
    qualifying_reduction_fixtures: usize,
    median_visible_token_reduction_percent: f64,
    budget_overruns: u64,
    invalid_artifact_references: u64,
    pass: bool,
}

#[derive(Debug, Serialize)]
struct RecoveryReport {
    restart_restored: usize,
    restart_total: usize,
    concurrent_writers_restored: usize,
    concurrent_writers_total: usize,
    sha256_mismatches: u64,
    pass: bool,
}

#[derive(Debug, Serialize)]
struct PerformanceReport {
    cold_start_p95_ms: f64,
    projection_1_mib_p95_ms: f64,
    projection_10_mib_p95_ms: f64,
    hook_1_mib_p95_ms: f64,
    peak_rss_10_mib_bytes: u64,
    cold_start_limit_ms: f64,
    projection_1_mib_limit_ms: f64,
    projection_10_mib_limit_ms: f64,
    hook_1_mib_limit_ms: f64,
    peak_rss_limit_bytes: u64,
    raw_leaks: u64,
    adapter_budget_overruns: u64,
    pass: bool,
}

#[derive(Debug, Serialize)]
struct CriticalFailures {
    p0_fact_losses: u64,
    invalid_artifact_references: u64,
    silent_budget_overruns: u64,
    supported_surface_raw_leaks: u64,
}

#[derive(Debug)]
struct ChildRun {
    elapsed_ms: f64,
    peak_rss_bytes: u64,
    stdout: Vec<u8>,
}

fn main() {
    match run() {
        Ok(true) => {}
        Ok(false) => std::process::exit(1),
        Err(error) => {
            eprintln!("release gate failed to run: {error}");
            std::process::exit(2);
        }
    }
}

fn run() -> Result<bool, Box<dyn Error>> {
    let options = parse_options()?;
    let corpus = evaluate_corpus(&options.corpus_root)?;
    let recovery = evaluate_recovery(&options.corpus_root)?;
    let performance = evaluate_performance(&options.binary)?;
    let git_revision = command_output("git", &["rev-parse", "HEAD"])?;
    let binary_sha256 = sha256_hex(&fs::read(&options.binary)?);
    let fuzz_binary_sha256 = sha256_hex(&fs::read(&options.fuzz_binary)?);
    let fuzz: FuzzEvidence = serde_json::from_slice(&fs::read(&options.fuzz_evidence)?)?;
    let fuzz_pass = fuzz.schema_version == "distill.release-fuzz/v1"
        && fuzz.target == "fuzz_engine"
        && fuzz.mode == "release"
        && fuzz.result == "clean"
        && fuzz.git_revision == git_revision
        && fuzz.binary_sha256 == fuzz_binary_sha256
        && fuzz.cpu_seconds >= 3_600.0
        && fuzz.timeout_seconds <= 5;
    let linux_release: SuiteEvidence = serde_json::from_slice(&fs::read(&options.suite_evidence)?)?;
    let linux_release_pass = linux_release.schema_version == "distill.release-suite/v2"
        && linux_release.target == "linux-x86_64"
        && linux_release.profile == "release"
        && linux_release.result == "passed"
        && linux_release.git_revision == git_revision
        && linux_release.source_tree.len() == 40
        && linux_release
            .source_tree
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
        && linux_release.binary_sha256 == binary_sha256
        && linux_release.commands
            == [
                "./scripts/check-native.sh",
                "cargo build --release",
                "cargo test --release",
            ]
        && cfg!(all(target_os = "linux", target_arch = "x86_64"));
    let critical_failures = CriticalFailures {
        p0_fact_losses: corpus.p0_total.saturating_sub(corpus.p0_preserved),
        invalid_artifact_references: corpus.invalid_artifact_references,
        silent_budget_overruns: corpus
            .budget_overruns
            .saturating_add(performance.adapter_budget_overruns),
        supported_surface_raw_leaks: performance.raw_leaks,
    };
    let status = if corpus.pass
        && recovery.pass
        && performance.pass
        && fuzz_pass
        && linux_release_pass
        && critical_failures.p0_fact_losses == 0
        && critical_failures.invalid_artifact_references == 0
        && critical_failures.silent_budget_overruns == 0
        && critical_failures.supported_surface_raw_leaks == 0
    {
        "GO"
    } else {
        "NO-GO"
    };
    let report = Report {
        schema_version: "distill.release-gate/v2",
        generated_at: timestamp(SystemTime::now())?,
        git_revision,
        reference_machine: machine()?,
        protocol: Protocol {
            warm_up_runs: WARM_UP_RUNS,
            measured_runs: MEASURED_RUNS,
            percentile: "nearest-rank P95",
            tokenizer: CL100K_PROFILE,
        },
        corpus,
        recovery,
        performance,
        fuzz,
        linux_release,
        critical_failures,
        status,
    };
    if let Some(parent) = options.output.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&options.output, serde_json::to_vec_pretty(&report)?)?;
    println!(
        "{}: {}",
        options.output.display(),
        report.status.to_ascii_lowercase()
    );
    Ok(report.status == "GO")
}

fn parse_options() -> Result<Options, Box<dyn Error>> {
    let mut arguments = std::env::args().skip(1);
    let mut binary = None;
    let mut corpus_root = None;
    let mut fuzz_binary = None;
    let mut fuzz_evidence = None;
    let mut suite_evidence = None;
    let mut output = None;
    while let Some(argument) = arguments.next() {
        let value = arguments
            .next()
            .ok_or_else(|| format!("{argument} requires a value"))?;
        match argument.as_str() {
            "--binary" => binary = Some(PathBuf::from(value)),
            "--corpus-root" => corpus_root = Some(PathBuf::from(value)),
            "--fuzz-binary" => fuzz_binary = Some(PathBuf::from(value)),
            "--fuzz-evidence" => fuzz_evidence = Some(PathBuf::from(value)),
            "--suite-evidence" => suite_evidence = Some(PathBuf::from(value)),
            "--output" => output = Some(PathBuf::from(value)),
            _ => return Err(format!("unknown argument: {argument}").into()),
        }
    }
    Ok(Options {
        binary: required_path(binary, "--binary")?,
        corpus_root: required_path(corpus_root, "--corpus-root")?,
        fuzz_binary: required_path(fuzz_binary, "--fuzz-binary")?,
        fuzz_evidence: required_path(fuzz_evidence, "--fuzz-evidence")?,
        suite_evidence: required_path(suite_evidence, "--suite-evidence")?,
        output: output.ok_or("--output is required")?,
    })
}

fn required_path(path: Option<PathBuf>, name: &str) -> Result<PathBuf, Box<dyn Error>> {
    let path = path.ok_or_else(|| format!("{name} is required"))?;
    Ok(fs::canonicalize(path)?)
}

fn evaluate_corpus(root: &Path) -> Result<CorpusReport, Box<dyn Error>> {
    let fixtures = load_fixtures(root)?;
    let budgets = load_budgets(root)?;
    let directory = tempfile::tempdir()?;
    let store_path = directory.path().join("store/store.sqlite");
    let engine = Engine::new(EngineConfig::local(store_path.clone()))?;
    let tokenizer = cl100k_base_singleton();
    let mut p0_total = 0_u64;
    let mut p0_preserved = 0_u64;
    let mut p1_total = 0_u64;
    let mut p1_preserved = 0_u64;
    let mut reductions = Vec::new();
    let mut budget_overruns = 0_u64;
    let mut invalid_artifact_references = 0_u64;

    for fixture in &fixtures {
        let bytes = materialize_source(&fixture.source)?;
        if sha256_hex(&bytes) != fixture.source_sha256 {
            return Err(format!("{} source digest mismatch", fixture.id).into());
        }
        let profile = budgets
            .get(&fixture.budget_profile)
            .ok_or_else(|| format!("{} has an unknown budget", fixture.id))?;
        let budget = budget(profile)?;
        let outcome = engine.handle(request(fixture, bytes.clone(), budget.clone()))?;
        let visible = outcome.visible.bytes.as_bytes();
        for fact in &fixture.annotations.p0 {
            p0_total += 1;
            if contains(visible, &BASE64.decode(&fact.needle_base64)?) {
                p0_preserved += 1;
            }
        }
        for fact in &fixture.annotations.p1 {
            p1_total += 1;
            if contains(visible, &BASE64.decode(&fact.needle_base64)?) {
                p1_preserved += 1;
            }
        }
        let actual_visible_count = match budget.unit {
            CountUnit::Bytes => outcome.visible.bytes.len() as u64,
            CountUnit::Tokens => tokenizer.encode_ordinary(&outcome.visible.bytes).len() as u64,
        };
        if actual_visible_count != outcome.receipt.visible_count
            || actual_visible_count.saturating_add(budget.reserved_envelope)
                > budget.total_visible_limit
        {
            budget_overruns += 1;
        }
        if !valid_artifact(&outcome.artifact, &fixture.source_sha256, bytes.len()) {
            invalid_artifact_references += 1;
        }
        let original_tokens = tokenizer
            .encode_ordinary(&String::from_utf8_lossy(&bytes))
            .len() as u64;
        let source_count = match budget.unit {
            CountUnit::Bytes => bytes.len() as u64,
            CountUnit::Tokens => original_tokens,
        };
        if source_count > budget.total_visible_limit.saturating_mul(4) && original_tokens > 0 {
            let visible_tokens = tokenizer.encode_ordinary(&outcome.visible.bytes).len() as f64;
            reductions.push(1.0 - visible_tokens / original_tokens as f64);
        }
    }
    let p0_recall = percent(p0_preserved, p0_total);
    let p1_recall = percent(p1_preserved, p1_total);
    let median_reduction = median(&mut reductions) * 100.0;
    Ok(CorpusReport {
        fixtures: fixtures.len(),
        p0_preserved,
        p0_total,
        p0_recall_percent: p0_recall,
        p1_preserved,
        p1_total,
        p1_recall_percent: p1_recall,
        qualifying_reduction_fixtures: reductions.len(),
        median_visible_token_reduction_percent: median_reduction,
        budget_overruns,
        invalid_artifact_references,
        pass: fixtures.len() >= 100
            && p0_recall == 100.0
            && p1_recall >= 95.0
            && !reductions.is_empty()
            && median_reduction >= 50.0
            && budget_overruns == 0
            && invalid_artifact_references == 0,
    })
}

fn evaluate_recovery(root: &Path) -> Result<RecoveryReport, Box<dyn Error>> {
    let fixtures = load_fixtures(root)?;
    let budgets = load_budgets(root)?;
    let directory = tempfile::tempdir()?;
    let store_path = directory.path().join("restart/store.sqlite");
    let engine = Engine::new(EngineConfig::local(store_path.clone()))?;
    let mut committed = Vec::new();
    for fixture in &fixtures {
        let bytes = materialize_source(&fixture.source)?;
        let profile = budgets
            .get(&fixture.budget_profile)
            .ok_or_else(|| format!("{} has an unknown budget", fixture.id))?;
        let outcome = engine.handle(request(fixture, bytes.clone(), budget(profile)?))?;
        committed.push((outcome.artifact, bytes));
    }
    drop(engine);
    let restarted = Engine::new(EngineConfig::local(store_path))?;
    let mut restart_restored = 0_usize;
    let mut sha256_mismatches = 0_u64;
    for (artifact, expected) in &committed {
        let restored = restarted.restore(artifact)?;
        if restored.bytes.0 == *expected && sha256_hex(&restored.bytes.0) == artifact.source_sha256
        {
            restart_restored += 1;
        } else {
            sha256_mismatches += 1;
        }
    }

    let concurrent_store = directory.path().join("concurrent/store.sqlite");
    Engine::new(EngineConfig::local(concurrent_store.clone()))?;
    let barrier = Arc::new(Barrier::new(8));
    let handles = (0..8)
        .map(|index| {
            let barrier = Arc::clone(&barrier);
            let store = concurrent_store.clone();
            thread::spawn(move || -> Result<(ArtifactRef, Vec<u8>), String> {
                let engine =
                    Engine::new(EngineConfig::local(store)).map_err(|error| error.to_string())?;
                let bytes = format!("writer-{index}-{}", "payload\n".repeat(128)).into_bytes();
                let fixture = Fixture {
                    id: format!("release-writer-{index}"),
                    category: "logs".to_owned(),
                    source: Value::Null,
                    source_sha256: sha256_hex(&bytes),
                    budget_profile: String::new(),
                    annotations: Annotations {
                        p0: Vec::new(),
                        p1: Vec::new(),
                    },
                };
                barrier.wait();
                let outcome = engine
                    .handle(request(
                        &fixture,
                        bytes.clone(),
                        Budget {
                            unit: CountUnit::Bytes,
                            total_visible_limit: 512,
                            reserved_envelope: 0,
                            token_profile: None,
                        },
                    ))
                    .map_err(|error| error.to_string())?;
                Ok((outcome.artifact, bytes))
            })
        })
        .collect::<Vec<_>>();
    let concurrent = handles
        .into_iter()
        .map(|handle| {
            handle
                .join()
                .map_err(|_| "concurrent writer panicked".to_owned())?
        })
        .collect::<Result<Vec<_>, _>>()?;
    let concurrent_restarted = Engine::new(EngineConfig::local(concurrent_store))?;
    let mut concurrent_writers_restored = 0_usize;
    for (artifact, expected) in &concurrent {
        let restored = concurrent_restarted.restore(artifact)?;
        if restored.bytes.0 == *expected {
            concurrent_writers_restored += 1;
        } else {
            sha256_mismatches += 1;
        }
    }
    let restart_total = committed.len();
    let concurrent_total = concurrent.len();
    Ok(RecoveryReport {
        restart_restored,
        restart_total,
        concurrent_writers_restored,
        concurrent_writers_total: concurrent_total,
        sha256_mismatches,
        pass: restart_restored == restart_total
            && concurrent_writers_restored == concurrent_total
            && sha256_mismatches == 0,
    })
}

fn evaluate_performance(binary: &Path) -> Result<PerformanceReport, Box<dyn Error>> {
    if !cfg!(target_os = "linux") {
        return Err("the automated performance gate requires Linux".into());
    }
    let directory = tempfile::tempdir()?;
    let mut cold = Vec::new();
    for index in 0..(WARM_UP_RUNS + MEASURED_RUNS) {
        let store = directory.path().join(format!("cold-{index}/store.sqlite"));
        let run = run_child(
            binary,
            &["--store", path_text(&store)?, "status", "--json"],
            &[],
            false,
        )?;
        if index >= WARM_UP_RUNS {
            cold.push(run.elapsed_ms);
        }
    }
    let one_mib = payload(1024 * 1024);
    let ten_mib = payload(10 * 1024 * 1024);
    let (projection_1_mib, _) = measure_projection(
        binary,
        &directory.path().join("projection-1/store.sqlite"),
        &one_mib,
    )?;
    let (projection_10_mib, peak_rss) = measure_projection(
        binary,
        &directory.path().join("projection-10/store.sqlite"),
        &ten_mib,
    )?;
    let marker = "DISTILL_RAW_LEAK_SENTINEL";
    let mut hook_bytes = payload(1024 * 1024);
    let marker_start = hook_bytes.len() / 2;
    hook_bytes[marker_start..marker_start + marker.len()].copy_from_slice(marker.as_bytes());
    let hook_response = String::from_utf8(hook_bytes)?;
    let hook_store = directory.path().join("hook/store.sqlite");
    let mut hook = Vec::new();
    let mut raw_leaks = 0_u64;
    let mut adapter_budget_overruns = 0_u64;
    for index in 0..(WARM_UP_RUNS + MEASURED_RUNS) {
        let event = hook_event("Bash", &format!("timed-{index}"), &hook_response)?;
        let run = run_child(
            binary,
            &[
                "--store",
                path_text(&hook_store)?,
                "codex-hook",
                "--mode",
                "active",
            ],
            &event,
            false,
        )?;
        let response: Value = serde_json::from_slice(&run.stdout)?;
        let reason = response["reason"].as_str().unwrap_or_default();
        if reason.contains(marker) {
            raw_leaks += 1;
        }
        if response["decision"] != "block"
            || cl100k_base_singleton().encode_ordinary(reason).len() > 2_250
        {
            adapter_budget_overruns += 1;
        }
        if index >= WARM_UP_RUNS {
            hook.push(run.elapsed_ms);
        }
    }
    for (index, tool) in [
        "Bash",
        "apply_patch",
        "mcp__filesystem__read_file",
        "update_plan",
    ]
    .into_iter()
    .enumerate()
    {
        let event = hook_event(tool, &format!("surface-{index}"), &hook_response)?;
        let run = run_child(
            binary,
            &[
                "--store",
                path_text(&hook_store)?,
                "codex-hook",
                "--mode",
                "active",
            ],
            &event,
            false,
        )?;
        let response: Value = serde_json::from_slice(&run.stdout)?;
        let reason = response["reason"].as_str().unwrap_or_default();
        if reason.contains(marker) {
            raw_leaks += 1;
        }
        if response["decision"] != "block"
            || cl100k_base_singleton().encode_ordinary(reason).len() > 2_250
        {
            adapter_budget_overruns += 1;
        }
    }
    let cold_start_p95 = p95(&mut cold);
    let projection_1_mib_p95 = p95(&mut projection_1_mib.clone());
    let projection_10_mib_p95 = p95(&mut projection_10_mib.clone());
    let hook_1_mib_p95 = p95(&mut hook);
    let pass = cold_start_p95 <= 50.0
        && projection_1_mib_p95 <= 100.0
        && projection_10_mib_p95 <= 500.0
        && hook_1_mib_p95 <= 150.0
        && peak_rss <= 128 * 1024 * 1024
        && raw_leaks == 0
        && adapter_budget_overruns == 0;
    Ok(PerformanceReport {
        cold_start_p95_ms: cold_start_p95,
        projection_1_mib_p95_ms: projection_1_mib_p95,
        projection_10_mib_p95_ms: projection_10_mib_p95,
        hook_1_mib_p95_ms: hook_1_mib_p95,
        peak_rss_10_mib_bytes: peak_rss,
        cold_start_limit_ms: 50.0,
        projection_1_mib_limit_ms: 100.0,
        projection_10_mib_limit_ms: 500.0,
        hook_1_mib_limit_ms: 150.0,
        peak_rss_limit_bytes: 128 * 1024 * 1024,
        raw_leaks,
        adapter_budget_overruns,
        pass,
    })
}

fn measure_projection(
    binary: &Path,
    store: &Path,
    input: &[u8],
) -> Result<(Vec<f64>, u64), Box<dyn Error>> {
    let mut measurements = Vec::new();
    let mut peak_rss = 0_u64;
    for index in 0..(WARM_UP_RUNS + MEASURED_RUNS) {
        let run = run_child(
            binary,
            &[
                "--store",
                path_text(store)?,
                "project",
                "--budget",
                "8192",
                "--json",
            ],
            input,
            true,
        )?;
        let response: Value = serde_json::from_slice(&run.stdout)?;
        let visible = response["result"]["visible"]["bytes"]
            .as_str()
            .ok_or("projection response omitted visible bytes")?;
        let reported = response["result"]["receipt"]["visible_count"]
            .as_u64()
            .unwrap_or(u64::MAX);
        if response["ok"] != true || reported != visible.len() as u64 || reported > 8_192 {
            return Err("projection performance fixture violated its contract".into());
        }
        if index >= WARM_UP_RUNS {
            measurements.push(run.elapsed_ms);
            peak_rss = peak_rss.max(run.peak_rss_bytes);
        }
    }
    Ok((measurements, peak_rss))
}

fn hook_event(tool: &str, tool_use_id: &str, response: &str) -> Result<Vec<u8>, Box<dyn Error>> {
    Ok(serde_json::to_vec(&json!({
        "schema_version": "codex.post-tool-use/v1",
        "session_id": "release-gate",
        "turn_id": "turn-1",
        "cwd": "/tmp",
        "hook_event_name": "PostToolUse",
        "tool_name": tool,
        "tool_use_id": tool_use_id,
        "tool_input": {"command": "synthetic"},
        "tool_response": response,
    }))?)
}

fn run_child(
    binary: &Path,
    arguments: &[&str],
    input: &[u8],
    sample_rss: bool,
) -> Result<ChildRun, Box<dyn Error>> {
    let start = Instant::now();
    let mut child = Command::new(binary)
        .args(arguments)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let stop = Arc::new(AtomicBool::new(false));
    let peak = Arc::new(AtomicU64::new(0));
    let sampler = if sample_rss {
        let pid = child.id();
        let stop = Arc::clone(&stop);
        let peak = Arc::clone(&peak);
        Some(thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                if let Ok(status) = fs::read_to_string(format!("/proc/{pid}/status"))
                    && let Some(kib) = parse_vm_rss_kib(&status)
                {
                    peak.fetch_max(kib.saturating_mul(1024), Ordering::Relaxed);
                }
                thread::sleep(Duration::from_millis(1));
            }
        }))
    } else {
        None
    };
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(input)?;
    }
    let output = child.wait_with_output()?;
    stop.store(true, Ordering::Relaxed);
    if let Some(sampler) = sampler {
        sampler
            .join()
            .map_err(|_| "RSS sampler panicked".to_owned())?;
    }
    if !output.status.success() {
        return Err(format!(
            "{} exited {}: {}",
            binary.display(),
            output.status,
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }
    Ok(ChildRun {
        elapsed_ms: start.elapsed().as_secs_f64() * 1_000.0,
        peak_rss_bytes: peak.load(Ordering::Relaxed),
        stdout: output.stdout,
    })
}

fn load_fixtures(root: &Path) -> Result<Vec<Fixture>, Box<dyn Error>> {
    fs::read_to_string(root.join("manifest.jsonl"))?
        .lines()
        .map(|line| serde_json::from_str(line).map_err(Into::into))
        .collect()
}

fn load_budgets(root: &Path) -> Result<BTreeMap<String, BudgetProfile>, Box<dyn Error>> {
    let file: BudgetFile = serde_json::from_slice(&fs::read(root.join("budget-profiles.json"))?)?;
    Ok(file
        .profiles
        .into_iter()
        .map(|profile| (profile.id.clone(), profile))
        .collect())
}

fn budget(profile: &BudgetProfile) -> Result<Budget, Box<dyn Error>> {
    Ok(Budget {
        unit: match profile.unit.as_str() {
            "bytes" => CountUnit::Bytes,
            "tokens" => CountUnit::Tokens,
            unit => return Err(format!("unknown budget unit: {unit}").into()),
        },
        total_visible_limit: profile.total_visible_limit,
        reserved_envelope: profile.reserved_envelope,
        token_profile: profile.token_profile.clone(),
    })
}

fn request(fixture: &Fixture, bytes: Vec<u8>, budget: Budget) -> Request {
    Request {
        contract_version: CONTRACT_VERSION.to_owned(),
        request_id: fixture.id.clone(),
        source: Source::Inline {
            bytes: ByteString(bytes),
            media_type: None,
        },
        budget,
        preservation_profile: preservation_profile(&fixture.category).to_owned(),
        retention: Retention::default(),
    }
}

fn preservation_profile(category: &str) -> &'static str {
    match category {
        "build-output" | "logs" => "build-log/v1",
        "test-output" => "test-log/v1",
        "diff" => "diff/v1",
        "diagnostics" => "diagnostic/v1",
        "stack-trace" => "stack-trace/v1",
        "source-code" => "source-code/v1",
        "json" => "json/v1",
        "unicode" => "unicode/v1",
        "malformed-bytes" => "binary/v1",
        "prompt-injection" => "untrusted-text/v1",
        _ => "plain-text/v1",
    }
}

fn materialize_source(source: &Value) -> Result<Vec<u8>, Box<dyn Error>> {
    match source.get("kind").and_then(Value::as_str) {
        Some("inline" | "file") => materialize_payload(&source["payload"]),
        Some("process") => {
            let mut bytes = Vec::new();
            for event in source["events"]
                .as_array()
                .ok_or("process events missing")?
            {
                bytes.extend(materialize_payload(&event["payload"])?);
            }
            Ok(bytes)
        }
        kind => Err(format!("unknown source kind: {kind:?}").into()),
    }
}

fn materialize_payload(payload: &Value) -> Result<Vec<u8>, Box<dyn Error>> {
    match payload.get("kind").and_then(Value::as_str) {
        Some("utf8") => Ok(payload["value"]
            .as_str()
            .ok_or("UTF-8 payload missing")?
            .as_bytes()
            .to_vec()),
        Some("base64") => {
            Ok(BASE64.decode(payload["value"].as_str().ok_or("base64 payload missing")?)?)
        }
        Some("padded") => {
            let mut bytes = BASE64.decode(
                payload["prefix_base64"]
                    .as_str()
                    .ok_or("padded prefix missing")?,
            )?;
            let length = usize::try_from(
                payload["byte_length"]
                    .as_u64()
                    .ok_or("padded length missing")?,
            )?;
            let fill = u8::try_from(payload["fill_byte"].as_u64().ok_or("padded fill missing")?)?;
            bytes.resize(length, fill);
            Ok(bytes)
        }
        kind => Err(format!("unknown payload kind: {kind:?}").into()),
    }
}

fn valid_artifact(artifact: &ArtifactRef, digest: &str, length: usize) -> bool {
    artifact.id.len() == 32
        && artifact
            .id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        && artifact.source_sha256 == digest
        && artifact.source_bytes == length as u64
        && artifact.expires_at > artifact.created_at
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    needle.is_empty()
        || haystack
            .windows(needle.len())
            .any(|window| window == needle)
}

fn payload(length: usize) -> Vec<u8> {
    let line = b"synthetic release projection payload without mandatory facts\n";
    line.iter().copied().cycle().take(length).collect()
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn percent(preserved: u64, total: u64) -> f64 {
    if total == 0 {
        100.0
    } else {
        preserved as f64 * 100.0 / total as f64
    }
}

fn median(values: &mut [f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    values.sort_by(f64::total_cmp);
    let middle = values.len() / 2;
    if values.len().is_multiple_of(2) {
        (values[middle - 1] + values[middle]) / 2.0
    } else {
        values[middle]
    }
}

fn p95(values: &mut [f64]) -> f64 {
    values.sort_by(f64::total_cmp);
    let rank = (values.len() * 95).div_ceil(100).saturating_sub(1);
    values.get(rank).copied().unwrap_or(f64::INFINITY)
}

fn parse_vm_rss_kib(status: &str) -> Option<u64> {
    status.lines().find_map(|line| {
        line.strip_prefix("VmRSS:")
            .and_then(|value| value.split_whitespace().next())
            .and_then(|value| value.parse().ok())
    })
}

fn path_text(path: &Path) -> Result<&str, Box<dyn Error>> {
    path.to_str()
        .ok_or_else(|| "release-gate path is not valid UTF-8".into())
}

fn command_output(command: &str, arguments: &[&str]) -> Result<String, Box<dyn Error>> {
    let output = Command::new(command).args(arguments).output()?;
    if !output.status.success() {
        return Err(format!("{command} failed").into());
    }
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

fn machine() -> Result<Machine, Box<dyn Error>> {
    let cpu = fs::read_to_string("/proc/cpuinfo")?
        .lines()
        .find_map(|line| line.strip_prefix("model name\t: "))
        .unwrap_or("unknown")
        .to_owned();
    let memory_kib = fs::read_to_string("/proc/meminfo")?
        .lines()
        .find_map(|line| line.strip_prefix("MemTotal:"))
        .and_then(|value| value.split_whitespace().next())
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(0);
    Ok(Machine {
        os: std::env::consts::OS,
        architecture: std::env::consts::ARCH,
        kernel: command_output("uname", &["-r"])?,
        cpu,
        memory_bytes: memory_kib.saturating_mul(1024),
    })
}

fn timestamp(time: SystemTime) -> Result<String, Box<dyn Error>> {
    let output = Command::new("date")
        .args([
            "-u",
            "-d",
            &format!("@{}", time.duration_since(UNIX_EPOCH)?.as_secs()),
            "+%Y-%m-%dT%H:%M:%SZ",
        ])
        .output()?;
    if !output.status.success() {
        return Err("date failed".into());
    }
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}
