use std::fs;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use slim_core::runtime::CancellationToken;
use slim_core::skills::{
    default_skill_script, fallback_skill_body, invoke_script, invoke_script_with_limits,
    resolve_script_path, SkillInvocationError, DEFAULT_SKILL_SCRIPT,
};
use slim_core::OperatingMode;

#[test]
fn skill_script_path_rejects_absolute_and_parent_traversal() {
    let root = std::env::temp_dir().join(format!(
        "slim-skill-path-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    fs::create_dir_all(&root).expect("root");
    fs::write(root.join("run.ps1"), "Write-Output ok").expect("script");
    // `\\` separates components only on Windows.
    let (parent, absolute) = if cfg!(windows) {
        ("..\\run.ps1", "C:\\outside.ps1")
    } else {
        ("../run.ps1", "/outside.ps1")
    };
    assert_eq!(
        resolve_script_path(&root, parent),
        Err(SkillInvocationError::InvalidScriptPath)
    );
    assert_eq!(
        resolve_script_path(&root, absolute),
        Err(SkillInvocationError::InvalidScriptPath)
    );
    assert!(resolve_script_path(&root, "run.ps1")
        .expect("valid script")
        .starts_with(root.canonicalize().expect("canonical root")));
    assert!(resolve_script_path(&root, "./run.ps1")
        .expect("dot-slash script")
        .starts_with(root.canonicalize().expect("canonical root")));
    #[cfg(windows)]
    assert!(resolve_script_path(&root, ".\\run.ps1")
        .expect("dot-backslash script")
        .starts_with(root.canonicalize().expect("canonical root")));
    assert!(matches!(
        invoke_script(&root, parent, OperatingMode::Auto, true),
        Err(SkillInvocationError::InvalidScriptPath)
    ));
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    assert!(matches!(
        invoke_script_with_limits(
            &root,
            "run.ps1",
            OperatingMode::Auto,
            true,
            std::time::Duration::from_secs(1),
            128,
            Some(&cancellation),
        ),
        Err(SkillInvocationError::Cancelled)
    ));
    fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn missing_run_script_falls_back_to_skill_body() {
    assert_eq!(default_skill_script(None), DEFAULT_SKILL_SCRIPT);
    assert_eq!(default_skill_script(Some("")), DEFAULT_SKILL_SCRIPT);
    assert_eq!(default_skill_script(Some("./run.ps1")), "run.ps1");
    assert_eq!(default_skill_script(Some(".\\run.ps1")), "run.ps1");

    let root = std::env::temp_dir().join(format!(
        "slim-skill-body-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    fs::create_dir_all(&root).expect("root");
    fs::write(
        root.join("SKILL.md"),
        "---\nname: body-only\ndescription: instruction skill\n---\ninstruction body\n",
    )
    .expect("skill metadata");
    let body = fallback_skill_body(&root, default_skill_script(Some(""))).expect("body");
    assert!(body.contains("instruction body"), "{body}");
    fs::write(root.join(DEFAULT_SKILL_SCRIPT), "Write-Output ok").expect("script");
    assert_eq!(fallback_skill_body(&root, DEFAULT_SKILL_SCRIPT), None);
    fs::remove_dir_all(root).expect("cleanup");
}

#[cfg(windows)]
#[test]
fn skill_script_powershell_arg_is_local_dos_path() {
    let root = std::env::temp_dir().join(format!(
        "slim-skill-dos-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    fs::create_dir_all(&root).expect("root");
    fs::write(root.join("run.ps1"), "Write-Output skill-local-ok").expect("script");
    let canonical = resolve_script_path(&root, "run.ps1").expect("canonical jail path");
    let launched = slim_core::skills::launch_path_for_powershell(&canonical);
    let launched_text = launched.to_string_lossy();
    assert!(
        !launched_text.starts_with(r"\\?\"),
        "PowerShell -File must not receive a verbatim path: {launched_text}"
    );
    let output = invoke_script(&root, "run.ps1", OperatingMode::Auto, true).expect("invoke");
    assert!(
        output.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("skill-local-ok"),
        "stdout={}",
        String::from_utf8_lossy(&output.stdout)
    );
    fs::remove_dir_all(root).expect("cleanup");
}

#[cfg(not(windows))]
#[test]
fn run_sh_runs_under_bash_without_an_execute_bit() {
    let root = std::env::temp_dir().join(format!(
        "slim-skill-sh-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    fs::create_dir_all(&root).expect("root");
    fs::write(
        root.join("run.sh"),
        "[[ -n \"$BASH_VERSION\" ]] && echo \"skill-local-ok $(basename \"$PWD\")\"",
    )
    .expect("script");
    let output = invoke_script(&root, "run.sh", OperatingMode::Auto, true).expect("invoke");
    assert!(
        output.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let directory = root.file_name().expect("name").to_string_lossy();
    assert!(
        stdout.contains(&format!("skill-local-ok {directory}")),
        "stdout={stdout}"
    );
    fs::remove_dir_all(root).expect("cleanup");
}

#[cfg(not(windows))]
#[test]
fn cancelling_a_skill_terminates_its_process_tree_outside_windows() {
    let root = std::env::temp_dir().join(format!(
        "slim-skill-cancel-tree-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    fs::create_dir_all(&root).expect("root");
    let started = root.join("child-pid.txt");
    fs::write(
        root.join("run.sh"),
        format!(
            "sleep 31 &\necho $! > '{}'\nwait",
            started.display().to_string().replace('\'', "'\\''")
        ),
    )
    .expect("script");

    let cancellation = CancellationToken::new();
    let trigger = cancellation.clone();
    let started_for_canceller = started;
    let canceller = std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let child_pid = loop {
            if let Ok(pid) = fs::read_to_string(&started_for_canceller) {
                if let Ok(pid) = pid.trim().parse::<u32>() {
                    break pid;
                }
            }
            assert!(
                std::time::Instant::now() < deadline,
                "descendant did not start before cancellation deadline"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        };
        trigger.cancel();
        child_pid
    });
    let result = invoke_script_with_limits(
        &root,
        "run.sh",
        OperatingMode::Auto,
        true,
        std::time::Duration::from_secs(8),
        128,
        Some(&cancellation),
    );
    let child_pid = canceller.join().expect("canceller");
    assert_eq!(result, Err(SkillInvocationError::Cancelled));
    // The group is killed with SIGKILL; give the kernel a moment to reap.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    let alive = loop {
        let alive = Command::new("kill")
            .args(["-0", &child_pid.to_string()])
            .output()
            .expect("kill -0")
            .status
            .success();
        if !alive || std::time::Instant::now() >= deadline {
            break alive;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    };
    assert!(
        !alive,
        "a cancelled skill left descendant PID {child_pid} running"
    );
    fs::remove_dir_all(root).expect("cleanup");
}

#[cfg(windows)]
#[test]
fn cancelling_a_skill_terminates_its_process_tree() {
    let root = std::env::temp_dir().join(format!(
        "slim-skill-cancel-tree-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    fs::create_dir_all(&root).expect("root");
    let started = root.join("child-pid.txt");
    let escaped_started = started.display().to_string().replace('\'', "''");
    let script = format!(
        "$child = Start-Process -FilePath 'ping.exe' -WindowStyle Hidden -ArgumentList '-n','31','127.0.0.1' -PassThru; \n\
         Set-Content -LiteralPath '{escaped_started}' -Value $child.Id; \n\
         Wait-Process -Id $child.Id"
    );
    fs::write(root.join("run.ps1"), script).expect("script");

    let cancellation = CancellationToken::new();
    let trigger = cancellation.clone();
    let started_for_canceller = started;
    let canceller = std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let child_pid = loop {
            if let Ok(pid) = fs::read_to_string(&started_for_canceller) {
                if let Ok(pid) = pid.trim().parse::<u32>() {
                    break pid;
                }
            }
            assert!(
                std::time::Instant::now() < deadline,
                "descendant did not start before cancellation deadline"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        };
        trigger.cancel();
        child_pid
    });
    let result = invoke_script_with_limits(
        &root,
        "run.ps1",
        OperatingMode::Auto,
        true,
        std::time::Duration::from_secs(8),
        128,
        Some(&cancellation),
    );
    let child_pid = canceller.join().expect("canceller");
    assert_eq!(result, Err(SkillInvocationError::Cancelled));
    let filter = format!("PID eq {child_pid}");
    let tasklist = Command::new("tasklist.exe")
        .args(["/FI", &filter, "/FO", "CSV", "/NH"])
        .output()
        .expect("tasklist");
    assert!(tasklist.status.success(), "tasklist failed");
    let listing = String::from_utf8_lossy(&tasklist.stdout);
    assert!(
        !listing.contains(&format!("\"{child_pid}\"")),
        "a cancelled skill left descendant PID {child_pid} running: {listing}"
    );
    fs::remove_dir_all(root).expect("cleanup");
}
