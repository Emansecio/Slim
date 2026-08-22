use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use fff_search::grep::{parse_grep_query, GrepMode, GrepSearchOptions};
use fff_search::{FilePickerOptions, SharedFilePicker, SharedFrecency};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let root = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .ok_or("usage: slim-poc-fff-search <root>")?;
    let watch = std::env::args().any(|arg| arg == "--watch");
    let hold_ms = std::env::args()
        .position(|arg| arg == "--hold-ms")
        .and_then(|index| std::env::args().nth(index + 1))
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(0);
    let shared = SharedFilePicker::default();
    let frecency = SharedFrecency::default();
    fff_search::file_picker::FilePicker::new_with_shared_state(
        shared.clone(),
        frecency,
        FilePickerOptions {
            base_path: root.to_string_lossy().into_owned(),
            enable_mmap_cache: false,
            watch,
            ..Default::default()
        },
    )?;
    if !shared.wait_for_scan(Duration::from_secs(10)) {
        return Err("fff scan did not complete".into());
    }
    if watch && !shared.wait_for_watcher(Duration::from_secs(10)) {
        return Err("fff watcher did not become ready".into());
    }

    let options = grep_options();
    let query = parse_grep_query("needle");
    let first_start = Instant::now();
    let (first_ms, first_matches, first_paths) = {
        let guard = shared.read()?;
        let picker = guard.as_ref().ok_or("picker unavailable")?;
        let result = picker.grep(&query, &options);
        let paths = result
            .files
            .iter()
            .map(|file| file.relative_path(picker).to_string())
            .collect::<Vec<_>>();
        (
            first_start.elapsed().as_secs_f64() * 1000.0,
            result.matches.len(),
            paths,
        )
    };
    let warm_start = Instant::now();
    let mut last_matches = 0usize;
    for _ in 0..100 {
        let guard = shared.read()?;
        let picker = guard.as_ref().ok_or("picker unavailable")?;
        last_matches = picker.grep(&query, &options).matches.len();
    }
    let warm_ms = warm_start.elapsed().as_secs_f64() * 1000.0;
    println!(
        "scan_ready=true first_ms={first_ms:.3} hundred_ms={warm_ms:.3} warm_avg_ms={:.3} first_matches={first_matches} warm_matches={last_matches} first_paths={first_paths:?}",
        warm_ms / 100.0
    );

    if watch {
        let added = root.join("src").join("watch-added.txt");
        let renamed = root.join("src").join("watch-renamed.txt");
        std::fs::write(&added, "watch-token\n")?;
        let added_seen = wait_for_match(&shared, "watch-token", true);
        std::fs::rename(&added, &renamed)?;
        let renamed_seen = wait_for_match(&shared, "watch-token", true);
        std::fs::remove_file(&renamed)?;
        let deleted_gone = wait_for_match(&shared, "watch-token", false);
        println!(
            "watch_added={added_seen} watch_renamed={renamed_seen} watch_deleted_gone={deleted_gone}"
        );
    }
    if hold_ms > 0 {
        std::thread::sleep(Duration::from_millis(hold_ms));
    }
    Ok(())
}

fn grep_options() -> GrepSearchOptions {
    GrepSearchOptions {
        mode: GrepMode::PlainText,
        max_file_size: 10 * 1024 * 1024,
        page_limit: 200,
        ..Default::default()
    }
}

fn wait_for_match(shared: &SharedFilePicker, query_text: &str, expected: bool) -> bool {
    let query = parse_grep_query(query_text);
    let options = grep_options();
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let found = shared
            .read()
            .ok()
            .and_then(|guard| guard.as_ref().map(|picker| !picker.grep(&query, &options).matches.is_empty()))
            .unwrap_or(false);
        if found == expected {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[allow(dead_code)]
fn _assert_root(path: &Path) -> bool {
    path.is_dir()
}
