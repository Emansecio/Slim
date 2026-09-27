//! File symlink setup for Windows integration tests.
//!
//! Creating a symlink requires Developer Mode or `SeCreateSymbolicLinkPrivilege`.
//! Tests that only need the link as setup report an explicit skip when the
//! current session lacks it; with the privilege they run unchanged.

use std::io::{self, Write};
use std::path::Path;

/// `ERROR_PRIVILEGE_NOT_HELD`; std maps it to no specific `io::ErrorKind`.
const ERROR_PRIVILEGE_NOT_HELD: i32 = 1314;

/// Creates the file symlink `link -> target`. Returns `false` after reporting
/// a skip for `test` when the environment cannot create symlinks; any other
/// failure panics because it is not an environment limitation.
pub fn symlink_file_or_skip(target: &Path, link: &Path, test: &str) -> bool {
    match std::os::windows::fs::symlink_file(target, link) {
        Ok(()) => true,
        Err(error) if lacks_symlink_privilege(&error) => {
            // Write to the raw handle: libtest captures `eprintln!` for passing
            // tests, which would hide the skip from a normal run.
            let _ = writeln!(
                io::stderr().lock(),
                "SKIPPED {test}: criar symlink requer Developer Mode ou \
                 SeCreateSymbolicLinkPrivilege ({error})"
            );
            false
        }
        Err(error) => panic!("symlink setup failed for {test}: {error}"),
    }
}

fn lacks_symlink_privilege(error: &io::Error) -> bool {
    error.raw_os_error() == Some(ERROR_PRIVILEGE_NOT_HELD)
        || error.kind() == io::ErrorKind::PermissionDenied
}
