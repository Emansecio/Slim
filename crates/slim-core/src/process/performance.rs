//! Opt-in, test-only timestamps on the production runner; no telemetry in builds.
use super::*;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static ENABLED: AtomicBool = AtomicBool::new(false);
static POINTS: Mutex<Vec<(&'static str, Instant)>> = Mutex::new(Vec::new());

pub(super) fn mark(label: &'static str) {
    if ENABLED.load(Ordering::Relaxed) {
        let at = Instant::now();
        POINTS.lock().unwrap().push((label, at));
    }
}

pub(super) fn mark_exit(child: &std::process::Child) {
    if !ENABLED.load(Ordering::Relaxed) {
        return;
    }
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::{Foundation::FILETIME, System::Threading::GetProcessTimes};
    let mut times = [FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    }; 4];
    unsafe {
        assert_ne!(
            GetProcessTimes(
                child.as_raw_handle(),
                &mut times[0],
                &mut times[1],
                &mut times[2],
                &mut times[3]
            ),
            0
        );
    }
    let exit = (u64::from(times[1].dwHighDateTime) << 32) | u64::from(times[1].dwLowDateTime);
    let exit = UNIX_EPOCH + Duration::from_nanos((exit - 116_444_736_000_000_000) * 100);
    let lag = SystemTime::now().duration_since(exit).unwrap_or_default();
    POINTS
        .lock()
        .unwrap()
        .push(("os_exit", Instant::now() - lag));
}

fn request() -> ProcessRequest {
    ProcessRequest {
        cwd: std::env::temp_dir(),
        program: PathBuf::from(std::env::var_os("ComSpec").unwrap()),
        args: ["/d", "/c", "exit 0"].into_iter().map(Into::into).collect(),
        timeout: Duration::from_secs(5),
        cancellation: None,
        output_budget: ProcessOutputBudget::per_stream(4096),
    }
}

#[test]
#[ignore = "manual release runner timeline; hidden children, run serially"]
fn runner_timeline() {
    use std::os::windows::process::CommandExt;
    let runner = ProcessRunner::default();
    for sample in 0..21 {
        let request = request();
        // Same shell, args, cwd, inherited environment plus UTF-8 override,
        // null stdin and captured empty stdout/stderr. Direct lacks containment.
        let start = Instant::now();
        let output = std::process::Command::new(&request.program)
            .args(&request.args)
            .current_dir(&request.cwd)
            .env(
                "PYTHONIOENCODING",
                std::env::var_os("PYTHONIOENCODING").unwrap_or_else(|| "utf-8".into()),
            )
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .creation_flags(0x08000000)
            .output()
            .unwrap();
        let direct = start.elapsed();
        assert!(output.status.success());
        assert!(output.stdout.is_empty() && output.stderr.is_empty());
        POINTS.lock().unwrap().clear();
        ENABLED.store(true, Ordering::Relaxed);
        mark("call");
        let output = runner.run(request).unwrap();
        mark("returned");
        ENABLED.store(false, Ordering::Relaxed);
        assert!(output.output.status.success());
        assert!(output.output.stdout.is_empty() && output.output.stderr.is_empty());
        assert!(!output.cancelled && !output.timed_out);
        if sample == 0 {
            continue;
        }
        let points = POINTS.lock().unwrap();
        let origin = points[0].1;
        eprint!(
            "sample={sample} direct_ms={:.3}",
            direct.as_secs_f64() * 1000.0
        );
        for (label, at) in points.iter() {
            eprint!(
                " {label}={:.3}",
                at.duration_since(origin).as_secs_f64() * 1000.0
            );
        }
        eprintln!();
    }
}

#[test]
#[ignore = "manual capture allocation comparison; hidden offline child, run serially"]
fn capture_budget_retained_allocation() {
    let runner = ProcessRunner::default();
    let program = runner.resolve_powershell().unwrap().unwrap();
    const PRODUCED: usize = 9 * 1024 * 1024;
    // Identical raw bytes on both streams; no files, provider or mutable command.
    let script = "$b = [byte[]]::new(65536); for ($i=0; $i -lt $b.Length; $i++) { $b[$i] = $i % 251 }; $o = [Console]::OpenStandardOutput(); $e = [Console]::OpenStandardError(); for ($i=0; $i -lt 144; $i++) { $o.Write($b,0,$b.Length); $e.Write($b,0,$b.Length) }";
    for (label, budget) in [("native", 8192), ("raw", 8 * 1024 * 1024)] {
        POINTS.lock().unwrap().clear();
        ENABLED.store(true, Ordering::Relaxed);
        let started = Instant::now();
        let output = runner
            .run(ProcessRequest {
                cwd: std::env::temp_dir(),
                program: program.clone(),
                args: [
                    "-NoLogo",
                    "-NoProfile",
                    "-NonInteractive",
                    "-Command",
                    script,
                ]
                .into_iter()
                .map(Into::into)
                .collect(),
                timeout: Duration::from_secs(30),
                cancellation: None,
                output_budget: ProcessOutputBudget::per_stream(budget),
            })
            .unwrap();
        let elapsed = started.elapsed();
        ENABLED.store(false, Ordering::Relaxed);
        let finalization = {
            let points = POINTS.lock().unwrap();
            let at = |label| points.iter().find(|(name, _)| *name == label).unwrap().1;
            at("handles_closed").duration_since(at("wait_complete"))
        };
        assert!(output.output.status.success());
        assert!(!output.cancelled && !output.timed_out);
        assert_eq!(output.output.stdout.len(), budget);
        assert_eq!(output.output.stderr.len(), budget);
        assert_eq!(output.stdout_discarded_bytes, PRODUCED - budget);
        assert_eq!(output.stderr_discarded_bytes, PRODUCED - budget);
        assert_eq!(output.output.stdout, output.output.stderr);
        let half = budget / 2;
        for (index, byte) in output.output.stdout.iter().enumerate() {
            let source_index = if index < half {
                index
            } else {
                PRODUCED - budget + index
            };
            assert_eq!(*byte, ((source_index % 65536) % 251) as u8);
        }
        let retained_capacity = output.output.stdout.capacity() + output.output.stderr.capacity();
        eprintln!("capture={label} bytes_per_stream={PRODUCED} budget_per_stream={budget} retained_vec_capacity_bytes={retained_capacity} runner_ms={:.3} finalization_ms={:.3}", elapsed.as_secs_f64() * 1000.0, finalization.as_secs_f64() * 1000.0);
    }
}
