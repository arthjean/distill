#![cfg(unix)]
#![allow(clippy::expect_used)]

use distill::{
    Budget, ByteString, CONTRACT_VERSION, CountUnit, Engine, EngineConfig, FailureCode, Request,
    Retention, Source,
};
use std::{
    collections::BTreeMap,
    env,
    fs::{self, File, OpenOptions},
    io::{self, Read},
    os::{
        fd::AsRawFd,
        unix::{ffi::OsStrExt, process::CommandExt},
    },
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

const DIRECT_PID_ENV: &str = "DISTILL_PROCESS_FIXTURE_DIRECT_PID";
const ESCAPED_PID_ENV: &str = "DISTILL_PROCESS_FIXTURE_ESCAPED_PID";
const HOLD_MS_ENV: &str = "DISTILL_PROCESS_FIXTURE_HOLD_MS";
const PROBE_ROOT_ENV: &str = "DISTILL_PROCESS_PROBE_ROOT";
const PROBE_STORE_ENV: &str = "DISTILL_PROCESS_PROBE_STORE";
const PROBE_REPORT_ENV: &str = "DISTILL_PROCESS_PROBE_REPORT";
const PROCESS_TIMEOUT: Duration = Duration::from_millis(100);
const DEADLINE_TOLERANCE: Duration = Duration::from_millis(250);
const EXTERNAL_WATCHDOG: Duration = Duration::from_secs(2);
const ESCAPED_PIPE_HOLD: Duration = Duration::from_millis(750);
const PROBE_RUNS: usize = 100;

#[derive(Debug)]
struct ProbeRecord {
    elapsed: Duration,
    direct_child_reaped: bool,
    reader_completed: bool,
    escaped_pid_survived_timeout: bool,
    timed_out: bool,
    signal: Option<i32>,
}

#[test]
fn escaped_descendant_process_lifecycle_is_bounded_by_external_watchdog() {
    let directory = tempfile::tempdir().expect("probe directory");
    let mut records = Vec::with_capacity(PROBE_RUNS);
    for run in 0..PROBE_RUNS {
        records.push(run_probe(directory.path(), run).expect("runtime probe"));
    }

    assert!(
        records.iter().all(|record| record.direct_child_reaped),
        "a direct child was not reaped: {records:#?}"
    );
    assert!(
        records.iter().all(|record| record.reader_completed),
        "reader teardown did not complete: {records:#?}"
    );
    assert!(
        records
            .iter()
            .all(|record| record.escaped_pid_survived_timeout),
        "the escaped descendant assumption was not reproduced: {records:#?}"
    );
    assert!(
        records.iter().all(|record| record.timed_out),
        "a fixture did not reach the Distill timeout: {records:#?}"
    );
    assert!(
        records.iter().all(|record| record.signal.is_some()),
        "a direct child did not record termination: {records:#?}"
    );

    let mut elapsed = records
        .iter()
        .map(|record| record.elapsed)
        .collect::<Vec<_>>();
    elapsed.sort_unstable();
    let p99_index = (PROBE_RUNS.saturating_mul(99).div_ceil(100)).saturating_sub(1);
    let p99 = elapsed[p99_index];
    assert!(
        p99 <= PROCESS_TIMEOUT + DEADLINE_TOLERANCE,
        "process lifecycle p99 {p99:?} exceeded {:?}: {records:#?}",
        PROCESS_TIMEOUT + DEADLINE_TOLERANCE
    );
}

#[test]
fn signaled_process_preserves_all_buffered_output_and_event_spans() {
    let directory = tempfile::tempdir().expect("signal probe directory");
    let workspace = directory.path().join("root");
    fs::create_dir(&workspace).expect("signal probe root");
    let mut config = EngineConfig::local(directory.path().join("store/store.sqlite"));
    config.roots.insert("probe".to_owned(), workspace);
    let engine = Engine::new(config).expect("signal probe engine");
    let shell = ["/bin/sh", "/usr/bin/sh"]
        .into_iter()
        .find(|path| Path::new(path).is_file())
        .expect("POSIX shell");
    let request = Request {
        contract_version: CONTRACT_VERSION.to_owned(),
        request_id: "signaled-output-probe".to_owned(),
        source: Source::Process {
            executable: ByteString::from_utf8(shell),
            argv: vec![
                ByteString::from_utf8("-c"),
                ByteString::from_utf8(
                    "i=0; while [ \"$i\" -lt 2048 ]; do printf 12345678; i=$((i + 1)); done; kill -TERM $$",
                ),
            ],
            cwd_root_id: "probe".to_owned(),
            cwd_relative_path: ByteString::default(),
            timeout_ms: Some(2_000),
            environment_profile: None,
        },
        budget: Budget {
            unit: CountUnit::Bytes,
            total_visible_limit: 64,
            reserved_envelope: 0,
            token_profile: None,
        },
        preservation_profile: "plain-text/v1".to_owned(),
        retention: Retention::default(),
        focus: None,
    };

    let failure = engine.handle(request).expect_err("signal failure");
    assert_eq!(failure.code, FailureCode::AcquisitionFailed);
    let process = failure
        .acquisition
        .expect("signal acquisition")
        .process
        .expect("signal process receipt");
    assert!(process.signal.is_some());
    assert!(!process.timed_out);
    let artifact = failure.artifact.expect("signal partial artifact");
    let restored = engine.restore(&artifact).expect("restore signal capture");
    assert_eq!(restored.bytes.0, b"12345678".repeat(2048));

    let mut cursor = 0_u64;
    for (order, event) in process.events.iter().enumerate() {
        assert_eq!(event.order, order as u64);
        assert_eq!(event.span.start, cursor);
        assert!(event.span.end > event.span.start);
        cursor = event.span.end;
    }
    assert_eq!(cursor, restored.bytes.0.len() as u64);
}

#[test]
fn escaped_descendant_runtime_probe_process() {
    let Ok(root) = env::var(PROBE_ROOT_ENV) else {
        return;
    };
    let store = PathBuf::from(env::var(PROBE_STORE_ENV).expect("probe store"));
    let report = PathBuf::from(env::var(PROBE_REPORT_ENV).expect("probe report"));
    let root = PathBuf::from(root);
    fs::create_dir_all(&root).expect("probe root");
    enable_child_subreaping().expect("enable child subreaping");

    let direct_pid_path = root.join("direct.pid");
    let escaped_pid_path = root.join("escaped.pid");
    let mut config = EngineConfig::local(store);
    config.roots.insert("probe".to_owned(), root);
    config.environment_profiles.insert(
        "fixture".to_owned(),
        BTreeMap::from([
            (
                DIRECT_PID_ENV.to_owned(),
                direct_pid_path.to_string_lossy().into_owned(),
            ),
            (
                ESCAPED_PID_ENV.to_owned(),
                escaped_pid_path.to_string_lossy().into_owned(),
            ),
            (
                HOLD_MS_ENV.to_owned(),
                ESCAPED_PIPE_HOLD.as_millis().to_string(),
            ),
        ]),
    );
    let engine = Engine::new(config).expect("probe engine");
    let executable = env::current_exe().expect("probe executable");
    let request = Request {
        contract_version: CONTRACT_VERSION.to_owned(),
        request_id: "escaped-descendant-probe".to_owned(),
        source: Source::Process {
            executable: ByteString(executable.as_os_str().as_bytes().to_vec()),
            argv: vec![
                ByteString::from_utf8("--exact"),
                ByteString::from_utf8("escaped_descendant_fixture_process"),
                ByteString::from_utf8("--nocapture"),
            ],
            cwd_root_id: "probe".to_owned(),
            cwd_relative_path: ByteString::default(),
            timeout_ms: Some(PROCESS_TIMEOUT.as_millis() as u64),
            environment_profile: Some("fixture".to_owned()),
        },
        budget: Budget {
            unit: CountUnit::Bytes,
            total_visible_limit: 64,
            reserved_envelope: 0,
            token_profile: None,
        },
        preservation_profile: "plain-text/v1".to_owned(),
        retention: Retention::default(),
        focus: None,
    };

    let started = Instant::now();
    let failure = engine.handle(request).expect_err("fixture must time out");
    let elapsed = started.elapsed();
    assert_eq!(failure.code, FailureCode::AcquisitionFailed);
    let process = failure
        .acquisition
        .expect("partial acquisition")
        .process
        .expect("process receipt");
    let direct_pid = read_pid(
        &direct_pid_path,
        Instant::now() + Duration::from_millis(100),
    )
    .expect("direct fixture pid");
    let escaped_pid = read_pid(
        &escaped_pid_path,
        Instant::now() + Duration::from_millis(100),
    )
    .expect("escaped fixture pid");
    let record = ProbeRecord {
        elapsed,
        direct_child_reaped: !process_exists(direct_pid),
        reader_completed: true,
        escaped_pid_survived_timeout: process_exists(escaped_pid),
        timed_out: process.timed_out,
        signal: process.signal,
    };
    fs::write(&report, encode_record(&record)).expect("probe report");

    terminate_pid(escaped_pid);
    reap_descendant(escaped_pid);
    assert!(
        !process_exists(direct_pid),
        "fixture direct child {direct_pid} survived"
    );
    assert!(
        !process_exists(escaped_pid),
        "escaped fixture {escaped_pid} survived cleanup"
    );
}

#[test]
fn escaped_descendant_fixture_process() {
    let Ok(direct_pid_path) = env::var(DIRECT_PID_ENV) else {
        return;
    };
    let escaped_pid_path = PathBuf::from(env::var(ESCAPED_PID_ENV).expect("escaped pid path"));
    let hold_ms = env::var(HOLD_MS_ENV)
        .expect("fixture hold")
        .parse::<u64>()
        .expect("fixture hold milliseconds");
    fs::write(direct_pid_path, format!("{}\n", std::process::id())).expect("direct pid");
    let escaped_pid_file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(escaped_pid_path)
        .expect("escaped pid file");

    // SAFETY: the forked branch uses only async-signal-safe libc operations
    // before _exit. The test process is disposable and externally watched.
    let escaped_pid = unsafe { libc::fork() };
    assert!(escaped_pid >= 0, "fixture fork failed");
    if escaped_pid == 0 {
        escaped_descendant(escaped_pid_file, hold_ms);
    }
    drop(escaped_pid_file);

    // The owned direct child stays in Distill's process group until Distill
    // terminates it. Its descendant has already escaped into a new session.
    loop {
        // SAFETY: pause blocks this disposable fixture until a signal arrives.
        unsafe {
            libc::pause();
        }
    }
}

fn run_probe(parent: &Path, run: usize) -> Result<ProbeRecord, String> {
    let run_root = parent.join(format!("run-{run}"));
    let workspace = run_root.join("root");
    let store = run_root.join("store/store.sqlite");
    let report = run_root.join("report.txt");
    fs::create_dir_all(&workspace)
        .map_err(|error| format!("run {run} workspace could not be created: {error}"))?;

    let executable =
        env::current_exe().map_err(|error| format!("test executable is unavailable: {error}"))?;
    let mut child = Command::new(executable);
    child
        .args([
            "--exact",
            "escaped_descendant_runtime_probe_process",
            "--nocapture",
        ])
        .env(PROBE_ROOT_ENV, &workspace)
        .env(PROBE_STORE_ENV, &store)
        .env(PROBE_REPORT_ENV, &report)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .process_group(0);
    let mut child = child
        .spawn()
        .map_err(|error| format!("run {run} probe could not be spawned: {error}"))?;
    let status = wait_under_watchdog(&mut child, Instant::now() + EXTERNAL_WATCHDOG);
    if status.is_none() {
        terminate_process_group(child.id());
        let _ = child.kill();
        let _ = child.wait();
        cleanup_fixture_files(&workspace);
        return Err(format!(
            "run {run} exceeded the external two-second watchdog"
        ));
    }
    let status = status.expect("watchdog status");
    if !status.success() {
        let mut diagnostics = String::new();
        if let Some(mut stderr) = child.stderr.take() {
            let _ = stderr.read_to_string(&mut diagnostics);
        }
        cleanup_fixture_files(&workspace);
        return Err(format!("run {run} failed with {status}: {diagnostics}"));
    }
    let encoded = fs::read_to_string(&report)
        .map_err(|error| format!("run {run} report could not be read: {error}"))?;
    decode_record(&encoded).map_err(|error| format!("run {run} report is invalid: {error}"))
}

fn wait_under_watchdog(child: &mut Child, deadline: Instant) -> Option<std::process::ExitStatus> {
    loop {
        if let Some(status) = child.try_wait().expect("inspect runtime probe") {
            return Some(status);
        }
        if Instant::now() >= deadline {
            return None;
        }
        thread::sleep(Duration::from_millis(5));
    }
}

fn cleanup_fixture_files(root: &Path) {
    for name in ["direct.pid", "escaped.pid"] {
        if let Ok(pid) = read_pid(&root.join(name), Instant::now()) {
            terminate_process_group(pid as u32);
            terminate_pid(pid);
        }
    }
}

fn encode_record(record: &ProbeRecord) -> String {
    format!(
        "elapsed_micros={}\ndirect_child_reaped={}\nreader_completed={}\nescaped_pid_survived_timeout={}\ntimed_out={}\nsignal={}\n",
        record.elapsed.as_micros(),
        record.direct_child_reaped,
        record.reader_completed,
        record.escaped_pid_survived_timeout,
        record.timed_out,
        record
            .signal
            .map_or_else(String::new, |signal| signal.to_string()),
    )
}

fn decode_record(encoded: &str) -> Result<ProbeRecord, String> {
    let values = encoded
        .lines()
        .filter_map(|line| line.split_once('='))
        .collect::<BTreeMap<_, _>>();
    let parse_bool = |key: &str| {
        values
            .get(key)
            .ok_or_else(|| format!("missing {key}"))?
            .parse::<bool>()
            .map_err(|_| format!("invalid {key}"))
    };
    let elapsed_micros = values
        .get("elapsed_micros")
        .ok_or_else(|| "missing elapsed_micros".to_owned())?
        .parse::<u64>()
        .map_err(|_| "invalid elapsed_micros".to_owned())?;
    let signal = match values.get("signal").copied() {
        Some("") | None => None,
        Some(value) => Some(
            value
                .parse::<i32>()
                .map_err(|_| "invalid signal".to_owned())?,
        ),
    };
    Ok(ProbeRecord {
        elapsed: Duration::from_micros(elapsed_micros),
        direct_child_reaped: parse_bool("direct_child_reaped")?,
        reader_completed: parse_bool("reader_completed")?,
        escaped_pid_survived_timeout: parse_bool("escaped_pid_survived_timeout")?,
        timed_out: parse_bool("timed_out")?,
        signal,
    })
}

fn read_pid(path: &Path, deadline: Instant) -> io::Result<libc::pid_t> {
    loop {
        match fs::read_to_string(path) {
            Ok(value) => {
                let pid = value
                    .trim()
                    .parse::<libc::pid_t>()
                    .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error));
                return pid.and_then(|pid| {
                    if pid > 1 {
                        Ok(pid)
                    } else {
                        Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "fixture PID must be greater than one",
                        ))
                    }
                });
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound && Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(1));
            }
            Err(error) => return Err(error),
        }
    }
}

fn process_exists(pid: libc::pid_t) -> bool {
    // SAFETY: signal zero only checks whether the positive PID exists.
    let result = unsafe { libc::kill(pid, 0) };
    result == 0 || io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

fn terminate_pid(pid: libc::pid_t) {
    if pid > 0 {
        // SAFETY: fixture PID values come from files under the private probe root.
        unsafe {
            libc::kill(pid, libc::SIGKILL);
        }
    }
}

fn terminate_process_group(pid: u32) {
    if let Ok(group) = libc::pid_t::try_from(pid) {
        // SAFETY: every watched probe starts in a new process group.
        unsafe {
            libc::kill(-group, libc::SIGKILL);
        }
    }
}

#[cfg(target_os = "linux")]
fn enable_child_subreaping() -> io::Result<()> {
    // SAFETY: PR_SET_CHILD_SUBREAPER changes only this disposable probe process.
    let result = unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(not(target_os = "linux"))]
fn enable_child_subreaping() -> io::Result<()> {
    Ok(())
}

#[cfg(target_os = "linux")]
fn reap_descendant(pid: libc::pid_t) {
    let deadline = Instant::now() + Duration::from_millis(500);
    loop {
        let mut status = 0;
        // SAFETY: the probe enabled subreaping before the fixture was spawned.
        let waited = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
        if waited == pid || waited == -1 || Instant::now() >= deadline {
            break;
        }
        thread::sleep(Duration::from_millis(1));
    }
}

#[cfg(not(target_os = "linux"))]
fn reap_descendant(pid: libc::pid_t) {
    let deadline = Instant::now() + Duration::from_millis(500);
    while process_exists(pid) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(1));
    }
}

fn escaped_descendant(pid_file: File, hold_ms: u64) -> ! {
    // SAFETY: this function runs only in the post-fork fixture branch and uses
    // async-signal-safe session, write, sleep, close, and exit operations.
    unsafe {
        if libc::setsid() < 0 {
            libc::_exit(2);
        }
        write_pid(pid_file.as_raw_fd(), libc::getpid());
        let mut remaining = libc::timespec {
            tv_sec: (hold_ms / 1_000) as libc::time_t,
            tv_nsec: ((hold_ms % 1_000) * 1_000_000) as libc::c_long,
        };
        while libc::nanosleep(&remaining, &mut remaining) != 0
            && io::Error::last_os_error().raw_os_error() == Some(libc::EINTR)
        {}
        libc::_exit(0);
    }
}

unsafe fn write_pid(fd: libc::c_int, pid: libc::pid_t) {
    let mut digits = [0_u8; 32];
    let mut cursor = digits.len() - 1;
    digits[cursor] = b'\n';
    let mut value = pid as u32;
    while value > 0 {
        cursor -= 1;
        digits[cursor] = b'0' + (value % 10) as u8;
        value /= 10;
    }
    let mut written = cursor;
    while written < digits.len() {
        // SAFETY: written stays within the live stack buffer.
        let count = unsafe {
            libc::write(
                fd,
                digits[written..].as_ptr().cast(),
                digits.len() - written,
            )
        };
        if count <= 0 {
            break;
        }
        written += count as usize;
    }
}
