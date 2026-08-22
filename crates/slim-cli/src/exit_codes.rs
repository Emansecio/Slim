#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(i32)]
pub enum ExitCode {
    Success = 0,
    ApprovalRequired = 10,
    InputRequired = 11,
    Blocked = 12,
    Cancelled = 13,
    Auth = 20,
    Provider = 21,
    Tool = 22,
    Internal = 30,
}

impl ExitCode {
    pub const fn as_i32(self) -> i32 {
        self as i32
    }
}
