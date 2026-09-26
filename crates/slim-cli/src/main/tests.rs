use super::*;

fn args(list: &[&str]) -> Vec<String> {
    list.iter().map(|arg| (*arg).to_owned()).collect()
}

#[test]
fn effort_value_is_not_a_positional_prompt() {
    let args = args(&[
        "--headless",
        "--provider",
        "openai-codex",
        "--effort",
        "high",
    ]);
    assert!(!has_positional_prompt(&args));
    assert!(known_options_without_prompt(&args));
}

#[test]
fn effort_without_value_disables_stdin() {
    let args = args(&["--headless", "--effort"]);
    assert!(!has_positional_prompt(&args));
    assert!(!known_options_without_prompt(&args));
}

#[test]
fn codex_speed_flags_keep_stdin_prompt_mode() {
    for flag in ["--fast", "--normal"] {
        let args = args(&["--headless", "--provider", "openai-codex", flag]);
        assert!(!has_positional_prompt(&args), "{flag}");
        assert!(known_options_without_prompt(&args), "{flag}");
    }
}

#[test]
fn recover_with_abandon_pending_does_not_read_stdin() {
    let args = args(&["--headless", "--recover", "run.slim", "--abandon-pending"]);
    assert!(!has_positional_prompt(&args));
    assert!(!known_options_without_prompt(&args));
}

#[test]
fn unknown_option_disables_stdin() {
    let args = args(&["--headless", "--bogus"]);
    assert!(!known_options_without_prompt(&args));
}

#[test]
fn benchmark_label_values_are_not_positional_prompts() {
    let args = args(&[
        "--headless",
        "--experiment-id",
        "exp-arm-b",
        "--task-id",
        "repo-17",
    ]);
    assert!(!has_positional_prompt(&args));
    assert!(known_options_without_prompt(&args));
}
