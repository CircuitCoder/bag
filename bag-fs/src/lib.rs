use thiserror::Error;

pub mod etag;
pub mod file;
pub mod render;
pub mod serve;
pub mod thumb;

#[derive(Error, Debug)]
pub enum Error {
    #[error("IO error: {0}")]
    IoError(#[from] std::io::Error),

    #[error("Not found due to path segment mismatch")]
    NotFound,

    /// The index is the length of the _SUFFIX_. This is to make it directly propagatable
    #[error("Archive password is missing or incorrect during file read operation")]
    ArchivePassword(usize),

    /// The index is the length of the _SUFFIX_. This is to make it directly propagatable
    #[error("Path does not reference a supported archive")]
    NotArchive(usize),

    #[error("Invalid path given for some API")]
    InvalidPath,

    #[error("Range unsatisfiable")]
    RangeUnsatisfiable,

    #[error("ZIP error: {0}")]
    Zip(#[from] zip::result::ZipError),

    #[error("Blocking task failed: {0}")]
    Join(#[from] tokio::task::JoinError),
}

pub type Result<T> = std::result::Result<T, Error>;
