//! Deterministic semantic folding for verbose build/test/lint shell outputs.
//!
//! Keeps raw outputs intact in session artifacts when materialized, while
//! collapsing repetitive compiler/passing-test noise on the provider wire.

use std::fmt::Write as _;

const MIN_FOLD_BYTES: usize = 1_536;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CommandFlavor {
    Cargo,
    Pytest,
    JsTest,
    GoTest,
    Tsc,
    Eslint,
    SwiftXcode,
    PythonLint,
    JsBuild,
    Docker,
    Git,
    Generic,
}

fn detect_flavor(command: &str, body: &str) -> CommandFlavor {
    let lower_cmd = command.to_ascii_lowercase();
    if lower_cmd.contains("cargo ")
        && (lower_cmd.contains(" test")
            || lower_cmd.contains(" check")
            || lower_cmd.contains(" clippy")
            || lower_cmd.contains(" build"))
    {
        return CommandFlavor::Cargo;
    }
    if lower_cmd.contains("pytest") || lower_cmd.contains("py.test") {
        return CommandFlavor::Pytest;
    }
    if lower_cmd.contains("jest")
        || lower_cmd.contains("vitest")
        || lower_cmd.contains("npm test")
        || lower_cmd.contains("pnpm test")
        || lower_cmd.contains("bun test")
        || lower_cmd.contains("playwright")
        || lower_cmd.contains("mocha")
    {
        return CommandFlavor::JsTest;
    }
    if lower_cmd.contains("go test") {
        return CommandFlavor::GoTest;
    }
    if lower_cmd.contains("tsc") || body.contains("): error TS") {
        return CommandFlavor::Tsc;
    }
    if lower_cmd.contains("eslint") {
        return CommandFlavor::Eslint;
    }
    if lower_cmd.contains("swift test")
        || lower_cmd.contains("swift build")
        || lower_cmd.contains("xcodebuild")
    {
        return CommandFlavor::SwiftXcode;
    }
    if lower_cmd.contains("ruff")
        || lower_cmd.contains("mypy")
        || lower_cmd.contains("pip ")
        || lower_cmd.contains("uv ")
    {
        return CommandFlavor::PythonLint;
    }
    if lower_cmd.contains("npm ")
        || lower_cmd.contains("pnpm ")
        || lower_cmd.contains("yarn ")
        || lower_cmd.contains("bun ")
        || lower_cmd.contains("vite")
        || lower_cmd.contains("next ")
        || lower_cmd.contains("webpack")
        || lower_cmd.contains("turbo")
    {
        return CommandFlavor::JsBuild;
    }
    if lower_cmd.contains("docker ") {
        return CommandFlavor::Docker;
    }
    if lower_cmd.starts_with("git ") || lower_cmd.contains(" git ") {
        return CommandFlavor::Git;
    }
    CommandFlavor::Generic
}

/// Attempts to compress verbose build/test/lint shell output. Returns `None`
/// when output is small or folding does not save >=35%.
pub(crate) fn fold_shell_output(command: &str, raw_output: &str, success: bool) -> Option<String> {
    if raw_output.len() < MIN_FOLD_BYTES {
        return None;
    }
    let flavor = detect_flavor(command, raw_output);
    let (header, rest) = match raw_output.split_once('\n') {
        Some((first, rest))
            if first.starts_with("exit ") || first.starts_with("exit_code=") =>
        {
            (Some(first), rest)
        }
        _ => (None, raw_output),
    };
    let folded_body = match flavor {
        CommandFlavor::Cargo => fold_cargo(rest, success),
        CommandFlavor::Pytest => fold_pytest(rest, success),
        CommandFlavor::JsTest => fold_js_test(rest, success),
        CommandFlavor::GoTest => fold_go_test(rest, success),
        CommandFlavor::Tsc | CommandFlavor::Eslint => fold_lint_or_tsc(rest, success),
        CommandFlavor::SwiftXcode => fold_swift_or_xcode(rest, success),
        CommandFlavor::PythonLint => fold_lint_or_tsc(rest, success),
        CommandFlavor::JsBuild => fold_js_build(rest, success),
        CommandFlavor::Docker => fold_docker(rest, success),
        CommandFlavor::Git => fold_git(rest, success),
        CommandFlavor::Generic => super::distill::distill_generic_log(rest),
    }
    .or_else(|| super::distill::distill_generic_log(rest))?;
    let mut out = String::with_capacity(folded_body.len() + 128);
    if let Some(h) = header {
        out.push_str(h);
        out.push('\n');
    }
    out.push_str(folded_body.trim_end());
    let orig_len = raw_output.len();
    let approx_final = out.len() + 72;
    if approx_final > orig_len.saturating_mul(65) / 100 {
        return None;
    }
    let _ = write!(
        out,
        "\n[folded {} -> {} bytes; use artifact_read if full output was stored]",
        orig_len,
        out.len() + 64
    );
    Some(out)
}

fn is_cargo_noise(trimmed: &str) -> bool {
    trimmed.starts_with("Compiling ")
        || trimmed.starts_with("Checking ")
        || trimmed.starts_with("Downloading ")
        || trimmed.starts_with("Downloaded ")
        || trimmed.starts_with("Updating ")
        || trimmed.starts_with("Fresh ")
        || trimmed.starts_with("Locking ")
        || trimmed.starts_with("Adding ")
        || trimmed.starts_with("Packaging ")
        || trimmed.starts_with("Blocking waiting for file lock")
}

fn fold_cargo(body: &str, success: bool) -> Option<String> {
    let mut compiled = 0usize;
    let mut warnings = 0usize;
    let mut passed_tests = 0usize;
    let mut keep = Vec::<String>::new();
    let mut in_error_block = false;
    let mut error_lines = 0usize;

    for line in body.lines() {
        let trimmed = line.trim_start();
        if is_cargo_noise(trimmed) {
            compiled += 1;
            in_error_block = false;
            continue;
        }
        if trimmed.starts_with("warning:") || trimmed.starts_with("warning[") {
            warnings += 1;
            if !success && keep.len() < 40 {
                keep.push(line.to_owned());
            }
            in_error_block = false;
            continue;
        }
        if trimmed.starts_with("test ") && trimmed.ends_with(" ... ok") {
            passed_tests += 1;
            in_error_block = false;
            continue;
        }
        if trimmed.starts_with("error")
            || trimmed.starts_with("failures:")
            || trimmed.starts_with("panicked at")
            || trimmed.starts_with("thread '")
        {
            in_error_block = true;
            error_lines = 0;
            keep.push(line.to_owned());
            continue;
        }
        if in_error_block {
            if trimmed.is_empty() && error_lines > 8 {
                in_error_block = false;
            } else if error_lines < 40 {
                keep.push(line.to_owned());
                error_lines += 1;
                continue;
            }
        }
        if trimmed.starts_with("test result:")
            || trimmed.starts_with("Finished ")
            || trimmed.starts_with("Running ")
            || trimmed.starts_with("Doc-tests ")
            || (!success && trimmed.starts_with("---- ") && trimmed.ends_with(" ----"))
            || (!success && trimmed.ends_with(" ... FAILED"))
        {
            keep.push(line.to_owned());
        }
    }

    if compiled == 0 && passed_tests == 0 && warnings < 3 {
        return None;
    }

    let mut summary = String::new();
    if compiled > 0 || warnings > 0 || passed_tests > 0 {
        let _ = writeln!(
            summary,
            "[cargo summary: compiled/checked_crates={compiled}, passed_tests={passed_tests}, warnings={warnings}]"
        );
    }
    for line in keep {
        summary.push_str(&line);
        summary.push('\n');
    }
    Some(summary)
}

fn fold_pytest(body: &str, success: bool) -> Option<String> {
    let mut passed = 0usize;
    let mut keep = Vec::new();
    let mut capturing = false;
    let mut captured = 0usize;
    for line in body.lines() {
        let trimmed = line.trim();
        if trimmed.ends_with(" PASSED") || trimmed.ends_with(" [100%]") && success {
            passed += 1;
            continue;
        }
        if trimmed.starts_with("=== FAILURES ===")
            || trimmed.starts_with("=== ERRORS ===")
            || trimmed.starts_with("FAILED ")
            || trimmed.starts_with("E   ")
        {
            capturing = true;
            captured = 0;
            keep.push(line.to_owned());
            continue;
        }
        if trimmed.starts_with("=== short test summary") || trimmed.contains(" passed") && trimmed.starts_with("===") {
            capturing = false;
            keep.push(line.to_owned());
            continue;
        }
        if !success && capturing && captured < 40 {
            keep.push(line.to_owned());
            captured += 1;
        }
    }
    if keep.is_empty() && passed == 0 {
        return None;
    }
    let mut out = if passed > 0 {
        format!("[pytest: {passed} passed lines collapsed]\n")
    } else {
        String::new()
    };
    for line in keep {
        out.push_str(&line);
        out.push('\n');
    }
    Some(out)
}

fn fold_js_test(body: &str, success: bool) -> Option<String> {
    let mut pass_lines = 0usize;
    let mut keep = Vec::new();
    let mut in_fail = false;
    let mut fail_lines = 0usize;
    for line in body.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("PASS ") || trimmed.starts_with("✓ ") || trimmed.starts_with("✔ ") {
            pass_lines += 1;
            in_fail = false;
            continue;
        }
        if trimmed.starts_with("FAIL ") || trimmed.starts_with("✕ ") || trimmed.starts_with("FAILURES") {
            in_fail = true;
            fail_lines = 0;
            keep.push(line.to_owned());
            continue;
        }
        if trimmed.starts_with("Test Suites:")
            || trimmed.starts_with("Test Files")
            || trimmed.starts_with("Tests:")
            || trimmed.starts_with("Tests ")
            || trimmed.starts_with("Duration")
            || trimmed.starts_with("Snapshots:")
        {
            in_fail = false;
            keep.push(line.to_owned());
            continue;
        }
        if !success && in_fail && fail_lines < 40 {
            keep.push(line.to_owned());
            fail_lines += 1;
        }
    }
    if pass_lines < 3 && keep.is_empty() {
        return None;
    }
    let mut out = format!("[js-test: {pass_lines} passed entries collapsed]\n");
    for line in keep {
        out.push_str(&line);
        out.push('\n');
    }
    Some(out)
}

fn fold_go_test(body: &str, success: bool) -> Option<String> {
    let mut ok_pkgs = 0usize;
    let mut keep = Vec::new();
    for line in body.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("ok ") || trimmed.starts_with("--- PASS:") || trimmed.starts_with("=== RUN ") {
            ok_pkgs += 1;
            continue;
        }
        if trimmed.starts_with("FAIL")
            || trimmed.starts_with("--- FAIL:")
            || !success && (trimmed.contains("_test.go:") || trimmed.starts_with("panic:"))
        {
            if keep.len() < 80 {
                keep.push(line.to_owned());
            }
        }
    }
    if ok_pkgs < 3 && keep.is_empty() {
        return None;
    }
    let mut out = format!("[go test: {ok_pkgs} passing test/package lines collapsed]\n");
    for line in keep {
        out.push_str(&line);
        out.push('\n');
    }
    Some(out)
}

fn fold_lint_or_tsc(body: &str, _success: bool) -> Option<String> {
    let mut kept = Vec::new();
    let mut omitted = 0usize;
    for line in body.lines() {
        let trimmed = line.trim();
        if trimmed.contains("error TS")
            || trimmed.contains(" error ")
            || trimmed.contains(" problems (")
            || trimmed.starts_with("Found ")
        {
            if kept.len() < 60 {
                kept.push(line.to_owned());
            } else {
                omitted += 1;
            }
        } else if !trimmed.is_empty() {
            omitted += 1;
        }
    }
    if kept.is_empty() || omitted < 8 {
        return None;
    }
    let mut out = kept.join("\n");
    let _ = write!(out, "\n[{omitted} non-error/excess lint lines omitted]");
    Some(out)
}

fn fold_swift_or_xcode(body: &str, success: bool) -> Option<String> {
    let mut compiled = 0usize;
    let mut passed = 0usize;
    let mut kept = Vec::new();
    for line in body.lines() {
        let t = line.trim();
        if t.starts_with("Compiling ")
            || t.starts_with("CompileSwift")
            || t.starts_with("CompileC ")
            || t.starts_with("Ld ")
            || t.starts_with("Build complete!")
        {
            compiled += 1;
            continue;
        }
        if t.contains(" passed (") || t.contains("Test Case ") && t.contains(" passed") {
            passed += 1;
            continue;
        }
        if !success
            || t.contains("error:")
            || t.contains("FAILED")
            || t.contains("Executed ")
            || t.starts_with("warning:")
        {
            kept.push(line);
        }
    }
    if compiled + passed < 6 {
        return None;
    }
    let mut out = String::new();
    let _ = writeln!(
        out,
        "[swift/xcode: {compiled} build steps, {passed} passing test lines omitted]"
    );
    for line in kept.into_iter().take(120) {
        out.push_str(line);
        out.push('\n');
    }
    Some(out)
}

fn fold_js_build(body: &str, success: bool) -> Option<String> {
    let mut skipped = 0usize;
    let mut kept = Vec::new();
    for line in body.lines() {
        let t = line.trim();
        let is_noise = t.starts_with("added ")
            || t.starts_with("Progress:")
            || t.starts_with("Packages:")
            || t.starts_with("transforming (")
            || t.starts_with("rendering chunks")
            || t.starts_with("computing gzip size")
            || t.starts_with("✓ ")
            || t.starts_with("dist/");
        if is_noise && success {
            skipped += 1;
            continue;
        }
        if t.to_ascii_lowercase().contains("error")
            || t.to_ascii_lowercase().contains("warn")
            || t.contains("built in ")
            || t.contains("Compiled ")
            || !success
        {
            kept.push(line);
        } else {
            skipped += 1;
        }
    }
    if skipped < 12 {
        return None;
    }
    let mut out = String::new();
    let _ = writeln!(out, "[js-build: {skipped} progress/asset lines omitted]");
    for line in kept.into_iter().take(100) {
        out.push_str(line);
        out.push('\n');
    }
    Some(out)
}

fn fold_docker(body: &str, success: bool) -> Option<String> {
    let mut cached = 0usize;
    let mut steps = 0usize;
    let mut kept = Vec::new();
    for line in body.lines() {
        let t = line.trim();
        if t.contains("CACHED") && t.starts_with('#') {
            cached += 1;
            continue;
        }
        if t.starts_with('#') && (t.contains(" DONE ") || t.contains(" sha256:") || t.contains(" transferring ")) {
            steps += 1;
            continue;
        }
        if !success || t.contains("ERROR") || t.contains("error") || t.starts_with("naming to ") || t.starts_with("exporting ") {
            kept.push(line);
        } else {
            steps += 1;
        }
    }
    if cached + steps < 10 {
        return None;
    }
    let mut out = String::new();
    let _ = writeln!(out, "[docker: {steps} layer/transfer lines, {cached} cached steps omitted]");
    for line in kept.into_iter().take(120) {
        out.push_str(line);
        out.push('\n');
    }
    Some(out)
}

fn fold_git(body: &str, _success: bool) -> Option<String> {
    let lines: Vec<&str> = body.lines().collect();
    if lines.len() < 60 {
        return None;
    }
    let mut out = String::new();
    for line in lines.iter().take(30) {
        out.push_str(line);
        out.push('\n');
    }
    let omitted = lines.len().saturating_sub(50);
    let _ = writeln!(out, "[git: ... {omitted} middle lines omitted ...]");
    for line in lines.iter().skip(lines.len().saturating_sub(20)) {
        out.push_str(line);
        out.push('\n');
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cargo_test_success_folds_compiling_and_passing_lines_while_preserving_exit_header() {
        let mut raw = String::from("exit_code=0\nstdout:\nstderr:\n");
        for i in 0..40 {
            raw.push_str(&format!("   Compiling dep-{i} v0.1.0\n"));
        }
        for i in 0..35 {
            raw.push_str(&format!("test mod::case_{i} ... ok\n"));
        }
        raw.push_str("test result: ok. 35 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.42s\n");

        let folded = fold_shell_output("cargo test -p slim-core", &raw, true).expect("should fold");
        assert!(folded.starts_with("exit_code=0\n"));
        assert!(folded.contains("compiled/checked_crates=40, passed_tests=35"));
        assert!(folded.contains("test result: ok. 35 passed"));
        assert!(folded.len() < raw.len() / 2);
    }

    #[test]
    fn cargo_test_failure_keeps_error_details_and_collapses_noise() {
        let mut raw = String::from("exit_code=101\nstdout:\n");
        for i in 0..35 {
            raw.push_str(&format!("   Compiling crate-{i} v0.1.0\n"));
            raw.push_str(&format!("test suite::ok_{i} ... ok\n"));
        }
        raw.push_str("failures:\n---- suite::broken stdout ----\nthread 'suite::broken' panicked at src/lib.rs:42:\nassertion failed: 1 == 2\ntest result: FAILED. 35 passed; 1 failed\n");

        let folded = fold_shell_output("cargo test", &raw, false).expect("should fold");
        assert!(folded.starts_with("exit_code=101\n"));
        assert!(folded.contains("panicked at src/lib.rs:42"));
        assert!(folded.contains("assertion failed: 1 == 2"));
        assert!(folded.contains("test result: FAILED"));
    }

    #[test]
    fn small_output_is_returned_unfolded() {
        let small = "exit_code=0\nstdout:\ntest result: ok. 1 passed\n";
        assert!(fold_shell_output("cargo test", small, true).is_none());
    }
}
