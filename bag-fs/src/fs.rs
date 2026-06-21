// Default fs handler

use axum::{body::Body, response::Response};
use std::path::PathBuf;
use tokio_util::io::ReaderStream;

use crate::{Result, etag::Etag};

#[derive(Clone)]
pub struct FsHandler {
    root: PathBuf,
}

impl FsHandler {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    async fn load_fs(&self, path: &str) -> Result<Response> {
        // FIXME: path sanitization
        // FIXME: path canonicalization
        // FIXME: empty path segment

        // TODO: binary search
        let tgt = self.root.join(path);
        let metadata = tokio::fs::metadata(&tgt).await?;
        if metadata.is_dir() {
            return Err(crate::Error::NotFound);
        }

        let mtime = metadata.modified()?;
        let length = metadata.len();
        let etag = Etag { mtime, length }.hash_string();

        let mime = mime_guess::from_path(&tgt).first_or_octet_stream();
        let file = tokio::fs::File::open(&tgt).await?;
        let stream = ReaderStream::new(file);
        return Ok(Response::builder()
            .header("Content-Type", mime.to_string())
            .header("Etag", etag)
            .body(Body::from_stream(stream))
            .unwrap());
        // TODO: handle range
        // Etag is done in tower
    }

    pub async fn handle(&self, path: &str) -> Response {
        match self.load_fs(path).await {
            Err(crate::Error::NotFound) => {
                return Response::builder()
                    .status(404)
                    .body("Not Found".into())
                    .unwrap();
            }
            Err(crate::Error::IoError(e)) if e.kind() == std::io::ErrorKind::NotFound => {
                return Response::builder()
                    .status(404)
                    .body("Not Found".into())
                    .unwrap();
            }
            Err(crate::Error::IoError(e)) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                return Response::builder()
                    .status(403)
                    .body("Permission Denied".into())
                    .unwrap();
            }
            Err(e) => {
                return Response::builder()
                    .status(500)
                    .body(format!("Internal Server Error: {}", e).into())
                    .unwrap();
            }
            Ok(resp) => resp,
        }
    }
}
