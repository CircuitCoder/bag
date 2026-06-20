// Default fs handler

use axum::{body::Body, http::Uri, response::Response};
use std::path::PathBuf;
use tokio_util::io::ReaderStream;

use crate::Result;

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

        let mut cur = self.root.clone();
        let mut remaining = path;

        loop {
            // Try to check the type of cur
            let metadata = tokio::fs::metadata(&cur).await?;

            if metadata.is_dir() {
                if remaining.is_empty() {
                    return Err(crate::Error::NotFound);
                }

                let (first, second) = remaining.split_once('/').unwrap_or((remaining, ""));
                cur.push(first);
                remaining = second;

                continue;
            }

            // First, see if we've exausted the path
            if remaining.is_empty() {
                let mime = mime_guess::from_path(&cur).first_or_octet_stream();
                let file = tokio::fs::File::open(&cur).await?;
                let stream = ReaderStream::new(file);
                return Ok(Response::builder()
                    .header("Content-Type", mime.to_string())
                    .body(Body::from_stream(stream))
                    .unwrap());
                // TODO: handle range, last-modified
                // Etag is done in tower
            }

            // Guess the file type based on extension name
            let _ext = cur.extension();

            // TODO: if this is a known archive type, goto the archive handler
            // Right now we don't have any

            // Finally, returns not found
            return Err(crate::Error::NotFound);
        }
    }

    pub async fn handle(&self, uri: Uri) -> Response {
        // Ignores query
        let path = uri.path();

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
