use crate::device::IoError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FsError {
    NotFound,
    Exists,
    NotDir,
    IsDir,
    NotEmpty,
    NoSpace,
    NameTooLong,
    InvalidName,
    FileTooBig,
    InvalidArgument,
    /// The underlying device failed.
    Io,
    /// The on-disk structures are inconsistent. Never a panic: a damaged or
    /// hostile disk must produce this error, not a crash.
    Corrupt(&'static str),
}

impl From<IoError> for FsError {
    fn from(_: IoError) -> Self {
        FsError::Io
    }
}
