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
        if matches!(
            arg.as_str(),
            "--provider"
                | "--model"
                | "--endpoint"
                | "--session"
                | "--resume"
                | "--recover"
                | "--image"
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
            "--plan" | "--read-only" | "--verbose" | "--jsonl" | "--headless" | "--tui"
            | "--fake" => {}
            "--prompt" | "--provider" | "--model" | "--endpoint" | "--session" | "--resume"
            | "--recover" | "--image" => {
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
