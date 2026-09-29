// Session storage/reducer contracts share one executable; each file stays a module.

#[cfg(windows)]
#[path = "../../../../tests/support/windows_symlink.rs"]
mod windows_symlink;

mod adv_session_jsonl;
mod prompt_queue_snapshot_compat;
mod rt_session_lock;
mod session_attempts;
mod session_branch;
mod session_branch_v2;
mod session_capabilities;
mod session_durable_run;
mod session_jsonl_repo;
mod session_queue;
mod session_recovery;
mod session_reducer;
mod session_repo_conformance;
mod session_resume;
mod session_schema_v2;
mod session_tool_phases;
