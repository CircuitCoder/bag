use thiserror::Error;

pub mod fs;

#[derive(Error, Debug)]
pub enum Error {
    #[error("IO error: {0}")]
    IoError(#[from] std::io::Error),

    #[error("Not found due to path segment mismatch")]
    NotFound,
}

pub type Result<T> = std::result::Result<T, Error>;
