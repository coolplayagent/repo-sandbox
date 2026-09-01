/// Stable process exit codes shared by every front end.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(i32)]
pub enum ExitCode {
    Success = 0,
    Configuration = 2,
    Environment = 3,
    BuildFailed = 10,
    TestFailed = 11,
}

impl ExitCode {
    pub const fn as_i32(self) -> i32 {
        self as i32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_codes_are_stable() {
        assert_eq!(ExitCode::Success.as_i32(), 0);
        assert_eq!(ExitCode::Configuration.as_i32(), 2);
        assert_eq!(ExitCode::Environment.as_i32(), 3);
        assert_eq!(ExitCode::BuildFailed.as_i32(), 10);
        assert_eq!(ExitCode::TestFailed.as_i32(), 11);
    }
}
