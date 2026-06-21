// Default fs handler

use axum::{body::Body, http::HeaderMap, response::Response};
use std::{path::PathBuf, range::Range};
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio_util::io::ReaderStream;

use crate::{Result, etag::Etag};

fn parse_range(headers: &HeaderMap, length: u64) -> Result<Option<Range<u64>>> {
    let raw = match headers
        .get(axum::http::header::RANGE)
        .and_then(|v| v.to_str().ok())
    {
        Some(v) => v,
        None => return Ok(None),
    };

    // Unknown range unit: ignore the header and serve the full body.
    let spec = match raw.trim().strip_prefix("bytes=") {
        Some(s) => s.trim(),
        None => return Ok(None),
    };

    // Multi-range (comma separated) is not supported; serve the full body.
    if spec.contains(',') {
        return Ok(None);
    }

    let (start_str, end_str) = spec
        .split_once('-')
        .ok_or(crate::Error::RangeUnsatisfiable)?;
    let start_str = start_str.trim();
    let end_str = end_str.trim();

    let parse = |s: &str| s.parse::<u64>().map_err(|_| crate::Error::RangeUnsatisfiable);

    let (start, end) = if start_str.is_empty() {
        // Suffix range `bytes=-N`: the last N bytes.
        let suffix = parse(end_str)?;
        if suffix == 0 {
            return Err(crate::Error::RangeUnsatisfiable);
        }
        (length.saturating_sub(suffix), length)
    } else {
        let start = parse(start_str)?;
        let end = if end_str.is_empty() {
            // Half-open `bytes=N-`: from N to the end.
            length
        } else {
            // Fully specified `bytes=N-M`: inclusive end -> exclusive, clamped.
            parse(end_str)?
                .checked_add(1)
                .ok_or(crate::Error::RangeUnsatisfiable)?
                .min(length)
        };
        (start, end)
    };

    if start >= end || start >= length {
        return Err(crate::Error::RangeUnsatisfiable);
    }

    Ok(Some(Range { start, end }))
}

#[derive(Clone)]
pub struct FsHandler {
    root: PathBuf,
}

impl FsHandler {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    async fn load_fs(&self, path: &str, header: &HeaderMap) -> Result<Response> {
        // FIXME: path sanitization
        // FIXME: path canonicalization
        // FIXME: empty path segment

        // TODO: binary search
        let tgt = self.root.join(path);
        let mut file = tokio::fs::File::open(&tgt).await?;
        let metadata = file.metadata().await?;

        let mtime = metadata.modified()?;
        let length = metadata.len();
        let etag = Etag { mtime, length };
        let encoded_etag = etag.hash_string();
        if let Some(in_etag) = header.get(axum::http::header::IF_NONE_MATCH).and_then(|e| e.to_str().ok()) {
            if etag.check_header(in_etag) {
                return Ok(Response::builder()
                    .status(304)
                    .header("Etag", format!("\"{encoded_etag}\""))
                    .header("Cache-Control", "max-age=60, stale-while-revalidate=86400")
                    .body(Body::empty())
                    .unwrap());
            }
        }

        let mime = mime_guess::from_path(&tgt).first_or_octet_stream();

        // Honor `If-Range`: if present and it does not match the current
        // representation, the range is ignored and the full body is served.
        let if_range_ok = header
            .get(axum::http::header::IF_RANGE)
            .and_then(|v| v.to_str().ok())
            .map(|v| etag.check_header(v))
            .unwrap_or(true);

        let range = if if_range_ok {
            parse_range(header, length)?
        } else {
            None
        };

        let mut read_len = length;
        if let Some(Range { start, end }) = range {
            file.seek(std::io::SeekFrom::Start(start)).await?;
            read_len = end - start;
        }
        let stream = ReaderStream::new(file.take(read_len));
        let code = if range.is_some() { 206 } else { 200 };

        let mut resp = Response::builder()
            .status(code)
            .header("Content-Type", mime.to_string())
            .header("Etag", format!("\"{encoded_etag}\""))
            .header("Cache-Control", "max-age=60, stale-while-revalidate=86400")
            .header(axum::http::header::CONTENT_LENGTH, read_len)
            .header(axum::http::header::ACCEPT_RANGES, "bytes");
        if let Some(Range { start, end }) = range {
            resp = resp.header(
                axum::http::header::CONTENT_RANGE,
                format!("bytes {}-{}/{}", start, end-1, length),
            );
        }
        return Ok(resp.body(Body::from_stream(stream)).unwrap());
    }

    pub async fn handle(&self, path: &str, headers: &HeaderMap) -> Response {
        match self.load_fs(path, headers).await {
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
            Err(crate::Error::IoError(e)) if e.kind() == std::io::ErrorKind::IsADirectory => {
                return Response::builder()
                    .status(400)
                    .body("Reading a directory".into())
                    .unwrap();
            }
            Err(crate::Error::RangeUnsatisfiable) => {
                return Response::builder()
                    .status(416)
                    .body("Range Unsatisfiable".into())
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
