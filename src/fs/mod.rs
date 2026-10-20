mod cft;
mod handle;
mod drive;
mod pcache;
use std::io::const_error;

pub use cft::*;
pub use handle::*;
pub use drive::*;

#[derive(Debug)]
pub enum Error {
    /// Errors raised within the filesystem, such as invalid paths, missing files, etc.
    /// These are desired behavior in case the user provides invalid inputs.
    FsError(FsError),
    /// Errors caused by the filesystem's backing.
    /// These are unexpected and usually mean the filesystem malfunctioned.
    BackingError(std::io::Error),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FsError {
    InvalidDrive,
    IsADirectory,
    AlreadyExists,
    FileNotFound,
    DirectoryNotEmpty,
    FileInUse,
    /// Path is invalid: it contains invalid characters, empty components, or files treated as directories
    InvalidPath,
    /// Write attempted on a read-only file
    PermissionDenied,
    SeekBeforeStart,
    FileNotOpen,
}

impl From<FsError> for Error {
    fn from(e: FsError) -> Self {
        Error::FsError(e)
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::BackingError(e)
    }
}

impl FsError {
    pub fn into_io_error(self) -> std::io::Error {
        match self {
            FsError::InvalidDrive => const_error!(std::io::ErrorKind::InvalidInput, "invalid drive"),
            FsError::IsADirectory => const_error!(std::io::ErrorKind::InvalidFilename, "is a directory"),
            FsError::AlreadyExists => const_error!(std::io::ErrorKind::AlreadyExists, "already exists"),
            FsError::FileNotFound => const_error!(std::io::ErrorKind::NotFound, "file not found"),
            FsError::DirectoryNotEmpty => const_error!(std::io::ErrorKind::DirectoryNotEmpty, "directory not empty"),
            FsError::FileInUse => const_error!(std::io::ErrorKind::ResourceBusy, "file in use"),
            FsError::InvalidPath => const_error!(std::io::ErrorKind::InvalidFilename, "invalid path"),
            FsError::PermissionDenied => const_error!(std::io::ErrorKind::PermissionDenied, "permission denied"),
            FsError::SeekBeforeStart => const_error!(std::io::ErrorKind::InvalidInput, "seek before start"),
            FsError::FileNotOpen => const_error!(std::io::ErrorKind::InvalidInput, "file not open"),
        }
    }
}

impl Error {
    pub fn into_io_error(self) -> std::io::Error {
        match self {
            Error::BackingError(e) => e,
            Error::FsError(e) => e.into_io_error()
        }
    }
}
