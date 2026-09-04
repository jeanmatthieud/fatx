use std::io;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum Error {
    #[error("io error")]
    Io(#[from] io::Error),
    #[error("the filesystem did not have the expected signature")]
    InvalidFilesystemSignature,
    #[error("the partition offset is invalid")]
    InvalidPartitionOffset,
    #[error("the partition size is invalid")]
    InvalidPartitionSize,
    #[error("the number of sectors per cluster is invalid")]
    InvalidSectorsPerCluster,
    #[error("the root cluster is invalid")]
    InvalidRootCluster,
    #[error("the cluster number is invalid")]
    InvalidClusterNumber,
    #[error("the cluster chain is corrupt")]
    InvalidClusterChain,
    #[error("the desired item could not be found")]
    NotFound,
    #[error("one path component is not a directory")]
    NotADirectory,
    #[error("the path unxpectedly identifies a directory")]
    IsADirectory,
    #[error("the filesystem was not opened for writing")]
    ReadOnlyFilesystem,
    #[error("an item with that name already exists")]
    AlreadyExists,
    #[error("the file name is empty or longer than the filesystem allows")]
    InvalidFileName,
    #[error("the directory is not empty")]
    DirectoryNotEmpty,
    #[error("no free cluster is left on the filesystem")]
    NoSpaceLeft,
    #[error("the file is larger than the filesystem can describe")]
    FileTooLarge,
    #[error("a directory cannot be moved inside itself")]
    InvalidRename,
    #[error("the operation is not allowed on the root directory")]
    IsRootDirectory,
}

impl From<Error> for io::Error {
    fn from(err: Error) -> Self {
        match err {
            Error::Io(err) => err,
            Error::NotFound => io::Error::new(io::ErrorKind::NotFound, err),
            Error::NotADirectory => io::Error::new(io::ErrorKind::NotADirectory, err),
            Error::IsADirectory => io::Error::new(io::ErrorKind::IsADirectory, err),
            Error::ReadOnlyFilesystem => io::Error::new(io::ErrorKind::ReadOnlyFilesystem, err),
            Error::AlreadyExists => io::Error::new(io::ErrorKind::AlreadyExists, err),
            Error::InvalidFileName => io::Error::new(io::ErrorKind::InvalidInput, err),
            Error::DirectoryNotEmpty => io::Error::new(io::ErrorKind::DirectoryNotEmpty, err),
            Error::NoSpaceLeft => io::Error::new(io::ErrorKind::StorageFull, err),
            Error::FileTooLarge => io::Error::new(io::ErrorKind::FileTooLarge, err),
            Error::InvalidRename => io::Error::new(io::ErrorKind::InvalidInput, err),
            Error::IsRootDirectory => io::Error::new(io::ErrorKind::PermissionDenied, err),
            _ => io::Error::other(err),
        }
    }
}
