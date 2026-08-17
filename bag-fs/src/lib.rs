use thiserror::Error;

pub mod etag;
pub mod fs;
pub mod thumb;

#[derive(Error, Debug)]
pub enum Error {
    #[error("IO error: {0}")]
    IoError(#[from] std::io::Error),

    #[error("Not found due to path segment mismatch")]
    NotFound,

    #[error("Range unsatisfiable")]
    RangeUnsatisfiable,

    #[error("ZIP error: {0}")]
    Zip(#[from] zip::result::ZipError),

    #[error("Blocking task failed: {0}")]
    Join(#[from] tokio::task::JoinError),
}

pub type Result<T> = std::result::Result<T, Error>;
