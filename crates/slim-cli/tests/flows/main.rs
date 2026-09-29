// In-process CLI/TUI flows over loopback fixtures share one executable. Tests that
// mutate the environment, spawn processes or drive a PTY keep their own.
mod adv_cli_args;
mod ask_question_tui;
mod headless_resume;
mod opencode_go_headless;
mod sec_secret_flow;
mod tui_runtime;
