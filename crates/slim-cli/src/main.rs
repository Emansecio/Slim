use std::io::Read;

const MAX_STDIN_BYTES: usize = 8 * 1024 * 1024;

fn main() {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    let special = args
        .iter()
        .any(|arg| matches!(arg.as_str(), "--help" | "-h" | "--version" | "-V"));
    if !special && !args.iter().any(|arg| arg == "--headless") {
        if let Err(error) = slim_cli::run_tui(args) {
            eprintln!("tui error: {error}");
            std::process::exit(error.code().as_i32());
        }
        return;
    }
    let has_prompt = has_positional_prompt(&args);
    let known_stdin_mode = known_options_without_prompt(&args);
    let needs_stdin = !special && !has_prompt && known_stdin_mode;
    let mut stdin = String::new();
    if needs_stdin {
        let mut input = std::io::stdin().take((MAX_STDIN_BYTES + 1) as u64);
        if let Err(error) = input.read_to_string(&mut stdin) {
            eprintln!("stdin could not be read: {error}");
            std::process::exit(slim_cli::ExitCode::InputRequired.as_i32());
        }
        if stdin.len() > MAX_STDIN_BYTES {
            eprintln!("stdin exceeds the {MAX_STDIN_BYTES}-byte safety limit");
            std::process::exit(slim_cli::ExitCode::InputRequired.as_i32());
        }
    }
    let output = slim_cli::run_cli(args, &stdin);
    print!("{}", output.stdout);
    eprint!("{}", output.stderr);
    if output.code != slim_cli::ExitCode::Success {
        std::process::exit(output.code.as_i32());
    }
}

fn has_positional_prompt(args: &[String]) -> bool {
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if arg == "--prompt" {
            return args.get(index + 1).is_some();
        }
        // Value-taking options must mirror parse_cli_args so the option's
        // value is not mistaken for a positional prompt.
        if matches!(
            arg.as_str(),
            "--provider"
                | "--model"
                | "--endpoint"
                | "--session"
                | "--resume"
                | "--recover"
                | "--image"
                | "--effort"
                | "--experiment-id"
                | "--task-id"
        ) {
            index += 2;
            continue;
        }
        if !arg.starts_with('-') {
            return true;
        }
        index += 1;
    }
    false
}

fn known_options_without_prompt(args: &[String]) -> bool {
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--plan" | "--read-only" | "--jev" | "--verbose" | "--jsonl" | "--headless"
            | "--tui" | "--fake" | "--abandon-pending" | "--fast" | "--normal" => {}
            "--prompt" | "--provider" | "--model" | "--endpoint" | "--session" | "--resume"
            | "--recover" | "--image" | "--effort" | "--experiment-id" | "--task-id" => {
                if args.get(index + 1).is_none() {
                    return false;
                }
                index += 1;
            }
            _ => return false,
        }
        index += 1;
    }
    true
}

#[cfg(test)]
mod tests {
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
    fn abandon_pending_flag_is_recognized() {
        let args = args(&["--headless", "--recover", "run.slim", "--abandon-pending"]);
        assert!(!has_positional_prompt(&args));
        assert!(known_options_without_prompt(&args));
    }

    #[test]
    fn unknown_option_disables_stdin() {
        let args = args(&["--headless", "--bogus"]);
        assert!(!known_options_without_prompt(&args));
    }

    #[test]
    fn jev_flag_keeps_stdin_prompt_mode() {
        let args = args(&[
            "--headless",
            "--jev",
            "--provider",
            "anthropic",
            "--model",
            "claude-sonnet-5",
        ]);
        assert!(!has_positional_prompt(&args));
        assert!(known_options_without_prompt(&args));
    }

    #[test]
    fn benchmark_label_values_are_not_positional_prompts() {
        let args = args(&[
            "--headless",
            "--experiment-id",
            "jev-arm-b",
            "--task-id",
            "repo-17",
        ]);
        assert!(!has_positional_prompt(&args));
        assert!(known_options_without_prompt(&args));
    }
}
