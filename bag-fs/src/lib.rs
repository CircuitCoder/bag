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
}

pub type Result<T> = std::result::Result<T, Error>;
