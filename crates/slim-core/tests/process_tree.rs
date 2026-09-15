#![cfg(windows)]

use std::ffi::OsString;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use slim_core::process::{ProcessOutputBudget, ProcessRequest, ProcessRunner};
use slim_core::runtime::CancellationToken;

#[test]
#[ignore = "subprocess fixture"]
fn descendant() {
    std::fs::write("ready", "ready").unwrap();
    std::thread::sleep(Duration::from_millis(1500));
    std::fs::write("late", "unexpected descendant effect").unwrap();
}

#[test]
#[ignore = "subprocess fixture"]
fn parent() {
    use std::io::Write;
    use std::os::windows::process::CommandExt;
    println!("OUTPUT_BEFORE_INTERRUPTION_42");
    eprintln!("ERROR_BEFORE_INTERRUPTION_42");
    std::io::stdout().flush().unwrap();
    std::io::stderr().flush().unwrap();
    // Intentionally exit while the descendant retains the inherited pipes.
    #[allow(clippy::zombie_processes)]
    let _child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "descendant", "--ignored", "--nocapture"])
        .creation_flags(0x08000000)
        .spawn()
        .unwrap();
}

fn interrupted_tree(cancel: bool) {
    let root = std::env::temp_dir().join(format!(
        "slim-process-tree-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir(&root).unwrap();
    let token = CancellationToken::new();
    let worker_token = token.clone();
    let canceller = cancel.then(|| {
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(500));
            worker_token.cancel();
        })
    });
    let result = ProcessRunner::default().run(ProcessRequest {
        cwd: root.clone(),
        program: std::env::current_exe().unwrap(),
        args: ["--exact", "parent", "--ignored", "--nocapture"]
            .into_iter()
            .map(OsString::from)
            .collect(),
        timeout: Duration::from_millis(if cancel { 5000 } else { 500 }),
        cancellation: Some(token),
        output_budget: ProcessOutputBudget::per_stream(4096),
    });
    if let Some(canceller) = canceller {
        canceller.join().unwrap();
    }
    let ready = root.join("ready").exists();
    std::thread::sleep(Duration::from_millis(1800));
    let late = root.join("late").exists();
    std::fs::remove_dir_all(&root).unwrap();
    assert!(ready, "descendant must actually have started");
    assert!(!late, "descendant survived interruption: {result:?}");
    let result = result.expect("tree termination must be confirmed");
    assert_eq!(result.cancelled, cancel);
    assert_eq!(result.timed_out, !cancel);
    assert!(
        String::from_utf8_lossy(&result.output.stdout).contains("OUTPUT_BEFORE_INTERRUPTION_42")
    );
    assert!(String::from_utf8_lossy(&result.output.stderr).contains("ERROR_BEFORE_INTERRUPTION_42"));
}

#[test]
fn timeout_terminates_descendant_after_parent_exits() {
    interrupted_tree(false);
}

#[test]
fn cancellation_terminates_descendant_after_parent_exits() {
    interrupted_tree(true);
}

#[test]
#[ignore = "subprocess fixture"]
fn stdin_reader() {
    use std::io::Read;
    let mut input = Vec::new();
    std::io::stdin().read_to_end(&mut input).unwrap();
    assert!(input.is_empty(), "tool consumed its runtime's stdin");
}

#[test]
#[ignore = "subprocess fixture"]
fn stdin_owner() {
    use std::io::Read;
    let result = ProcessRunner::default()
        .run(ProcessRequest {
            cwd: std::env::current_dir().unwrap(),
            program: std::env::current_exe().unwrap(),
            args: ["--exact", "stdin_reader", "--ignored", "--nocapture"]
                .into_iter()
                .map(OsString::from)
                .collect(),
            timeout: Duration::from_secs(5),
            cancellation: None,
            output_budget: ProcessOutputBudget::per_stream(4096),
        })
        .unwrap();
    assert!(result.output.status.success(), "{result:?}");
    assert!(!result.timed_out && !result.cancelled);
    let mut input = Vec::new();
    std::io::stdin().read_to_end(&mut input).unwrap();
    assert_eq!(input, b"FIXTURE_RUNTIME_INPUT\n");
}

#[test]
fn tools_do_not_consume_runtime_stdin() {
    use std::io::Write;
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};
    use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;

    let root = std::env::temp_dir().join(format!(
        "slim-stdin-isolation-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir(&root).unwrap();
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "stdin_owner", "--ignored", "--nocapture"])
        .current_dir(&root)
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"FIXTURE_RUNTIME_INPUT\n")
        .unwrap();
    let output = child.wait_with_output().unwrap();
    std::fs::remove_dir(&root).unwrap();
    assert!(
        output.status.success(),
        "stdin isolation failed: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

const QUOTED_ARGUMENT: &str = "ação \"literal\" C:\\日本語\\tail\\";
const STDOUT_PATTERN: &str = "stdout ação 日本語\n";
const STDERR_PATTERN: &str = "stderr ação 日本語\n";

#[test]
#[ignore = "subprocess fixture"]
fn output_after_parent_exit() {
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use windows_sys::Win32::Foundation::{ERROR_INVALID_PARAMETER, WAIT_OBJECT_0};
    use windows_sys::Win32::System::Threading::{
        OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE,
    };
    let parent = std::env::var("SLIM_FIXTURE_PARENT_PID")
        .unwrap()
        .parse()
        .unwrap();
    let raw = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, parent) };
    if raw.is_null() {
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(ERROR_INVALID_PARAMETER as i32)
        );
    } else {
        let handle = unsafe { OwnedHandle::from_raw_handle(raw) };
        assert_eq!(
            unsafe { WaitForSingleObject(handle.as_raw_handle(), 5000) },
            WAIT_OBJECT_0
        );
    }
    println!("STDOUT_AFTER_PARENT_EXIT_42");
    eprintln!("STDERR_AFTER_PARENT_EXIT_42");
}

#[test]
#[ignore = "subprocess fixture"]
fn parent_of_late_output() {
    use std::os::windows::process::CommandExt;
    let child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "output_after_parent_exit",
            "--ignored",
            "--nocapture",
        ])
        .env("SLIM_FIXTURE_PARENT_PID", std::process::id().to_string())
        .creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW)
        .spawn()
        .unwrap();
    // The descendant waits for this parent to exit before writing. Waiting here
    // would deadlock that handshake. Close our handle; the runner owns the job
    // that contains both processes and drains the inherited output pipes.
    drop(std::os::windows::io::OwnedHandle::from(child));
}

#[test]
fn runner_drains_output_written_after_the_direct_child_exits() {
    let result = ProcessRunner::default()
        .run(ProcessRequest {
            cwd: std::env::temp_dir(),
            program: std::env::current_exe().unwrap(),
            args: [
                "--exact",
                "parent_of_late_output",
                "--ignored",
                "--nocapture",
            ]
            .into_iter()
            .map(Into::into)
            .collect(),
            timeout: Duration::from_secs(10),
            cancellation: None,
            output_budget: ProcessOutputBudget::per_stream(4096),
        })
        .unwrap();
    assert!(result.output.status.success());
    assert!(!result.cancelled && !result.timed_out);
    assert!(String::from_utf8_lossy(&result.output.stdout).contains("STDOUT_AFTER_PARENT_EXIT_42"));
    assert!(String::from_utf8_lossy(&result.output.stderr).contains("STDERR_AFTER_PARENT_EXIT_42"));
    assert_eq!(result.stdout_discarded_bytes, 0);
    assert_eq!(result.stderr_discarded_bytes, 0);
}

#[test]
#[ignore = "subprocess fixture"]
fn simultaneous_output_fixture() {
    use std::io::Write;
    assert_eq!(std::env::args().next_back().unwrap(), QUOTED_ARGUMENT);
    assert_eq!(std::fs::read("cwd.marker").unwrap(), b"fixture");
    assert!(std::env::var_os("PYTHONIOENCODING").is_some());
    let stdout = std::thread::spawn(|| {
        std::io::stdout()
            .write_all(STDOUT_PATTERN.repeat(8192).as_bytes())
            .unwrap();
        std::io::stdout().flush().unwrap();
    });
    let stderr = std::thread::spawn(|| {
        std::io::stderr()
            .write_all(STDERR_PATTERN.repeat(8192).as_bytes())
            .unwrap();
        std::io::stderr().flush().unwrap();
    });
    stdout.join().unwrap();
    stderr.join().unwrap();
    std::process::exit(7);
}

#[test]
fn runner_preserves_parallel_output_unicode_quoting_cwd_and_error_exit() {
    let root = std::env::temp_dir().join(format!(
        "slim saída 日本語 {}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("cwd.marker"), b"fixture").unwrap();
    let program = root.join("child 日本語.exe");
    std::fs::copy(std::env::current_exe().unwrap(), &program).unwrap();
    let result = ProcessRunner::default().run(ProcessRequest {
        cwd: root.clone(),
        program,
        args: [
            "--exact",
            "simultaneous_output_fixture",
            "--ignored",
            "--nocapture",
            "--skip",
            QUOTED_ARGUMENT,
        ]
        .into_iter()
        .map(Into::into)
        .collect(),
        timeout: Duration::from_secs(10),
        cancellation: None,
        output_budget: ProcessOutputBudget::per_stream(1024 * 1024),
    });
    std::fs::remove_dir_all(root).unwrap();
    let result = result.unwrap();
    assert_eq!(result.output.status.code(), Some(7));
    assert!(!result.cancelled && !result.timed_out);
    assert_eq!(result.stdout_discarded_bytes, 0);
    assert_eq!(result.stderr_discarded_bytes, 0);
    assert_eq!(
        String::from_utf8(result.output.stderr).unwrap(),
        STDERR_PATTERN.repeat(8192)
    );
    assert_eq!(
        String::from_utf8(result.output.stdout)
            .unwrap()
            .matches(STDOUT_PATTERN)
            .count(),
        8192
    );
}
