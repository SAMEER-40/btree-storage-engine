use std::fmt;

#[derive(Debug)]
pub enum Error {
    Io(std::io::Error),
    PageNotFound(u32),
    PageFull(u32),
    KeyNotFound,
    DuplicateKey,
    WalCorrupted(String),
    BufferPoolFull,
    TransactionAborted(u64),
    DeadlockDetected(u64),
    LockConflict(u64),
    InvalidData(String),
    ChecksumMismatch { expected: u32, actual: u32 },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "I/O error: {}", e),
            Error::PageNotFound(id) => write!(f, "Page {} not found", id),
            Error::PageFull(id) => write!(f, "Page {} is full", id),
            Error::KeyNotFound => write!(f, "Key not found"),
            Error::DuplicateKey => write!(f, "Duplicate key"),
            Error::WalCorrupted(msg) => write!(f, "WAL corrupted: {}", msg),
            Error::BufferPoolFull => write!(f, "Buffer pool full, no evictable pages"),
            Error::TransactionAborted(id) => write!(f, "Transaction {} aborted", id),
            Error::DeadlockDetected(id) => write!(f, "Deadlock detected for transaction {}", id),
            Error::LockConflict(id) => write!(f, "Lock conflict for transaction {}", id),
            Error::InvalidData(msg) => write!(f, "Invalid data: {}", msg),
            Error::ChecksumMismatch { expected, actual } => {
                write!(
                    f,
                    "Checksum mismatch: expected {:#010x}, got {:#010x}",
                    expected, actual
                )
            }
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

pub type Result<T> = std::result::Result<T, Error>;
