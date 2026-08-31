// Default fs handler

use axum::{
    body::Body,
    http::HeaderMap,
    response::{IntoResponse, Response},
};
use bag_lib::path::Path as BagPath;
use std::{
    io::{Read, Seek, SeekFrom},
    ops::Range,
    path::Path,
};

use crate::{
    Result,
    etag::{self, Etag},
    file::ArchiveType,
};

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

    let parse = |s: &str| {
        s.parse::<u64>()
            .map_err(|_| crate::Error::RangeUnsatisfiable)
    };

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

pub enum FileThunk<'p> {
    Fs(std::fs::File),
    Nested(std::fs::File, ArchiveType, BagPath<'p>),
}

pub fn error_to_resp(error: crate::Error) -> Response {
    let (status, message) = match &error {
        crate::Error::ArchivePassword(_) => (
            axum::http::StatusCode::UNAUTHORIZED,
            "Archive password is missing or incorrect",
        ),
        crate::Error::NotArchive(_) => (
            axum::http::StatusCode::BAD_REQUEST,
            "Path does not reference a supported archive",
        ),
        crate::Error::NotFound => (axum::http::StatusCode::NOT_FOUND, "Not Found"),
        crate::Error::IoError(error) if error.kind() == std::io::ErrorKind::NotFound => {
            (axum::http::StatusCode::NOT_FOUND, "Not Found")
        }
        crate::Error::IoError(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
            (axum::http::StatusCode::FORBIDDEN, "Permission Denied")
        }
        crate::Error::IoError(error) if error.kind() == std::io::ErrorKind::IsADirectory => {
            (axum::http::StatusCode::BAD_REQUEST, "Reading a directory")
        }
        crate::Error::RangeUnsatisfiable => (
            axum::http::StatusCode::RANGE_NOT_SATISFIABLE,
            "Range Unsatisfiable",
        ),
        crate::Error::Zip(_) => (axum::http::StatusCode::BAD_REQUEST, "Invalid archive"),
        crate::Error::InvalidPath => (axum::http::StatusCode::BAD_REQUEST, "Invalid path"),
        _ => {
            // tracing::error!("Failed to read file: {error}");
            (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                "Internal Server Error",
            )
        }
    };
    (status, message).into_response()
}

pub struct RenderContext<'a> {
    pub file: FileThunk<'a>,
    pub etag: String,
}

/**
 * Handle a request based on a filesystem. The path is interpreted from the base directory,
 * the first segment before : is the outer FS file, and the rest is the path inside an archive.
 */
pub async fn serve<F>(
    base: &Path,
    path: &BagPath<'_>,
    headers: &HeaderMap,
    render: F,
) -> Result<Response>
where
    F: for<'a> AsyncFnOnce(RenderContext<'a>) -> Result<Response>,
{
    let mut base = base.to_path_buf();
    let (outer, subpath) = path.split_subpath(":");
    for segment in outer.segments().iter() {
        // Reject all path segments going up
        if segment.name() == ".." {
            return Err(crate::Error::InvalidPath);
        }
        base.push(segment.name());
    }
    // TODO: validate that the base is still under the root directory after joining

    // Get fs file
    let file = tokio::fs::File::open(&base).await?;
    let metadata = file.metadata().await?;
    let etag = Etag {
        mtime: metadata.modified()?,
        length: metadata.len(),
        subpath: subpath.as_ref().map(BagPath::borrow),
    };
    let encoded_etag = etag.hash_string();
    let header_etag = headers
        .get(axum::http::header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .map(|e| e.to_string());
    if let Some(expected_etag) = header_etag
        && etag::check_header(&encoded_etag, &expected_etag)
    {
        return Ok(Response::builder()
            .status(304)
            .header("Etag", format!("\"{encoded_etag}\""))
            .header("Cache-Control", "max-age=60, stale-while-revalidate=86400")
            .body(Body::empty())
            .unwrap());
    }

    // Read file out
    let file = file.into_std().await;
    let Some(subpath) = subpath else {
        let ctx = RenderContext {
            file: FileThunk::Fs(file),
            etag: encoded_etag,
        };
        return render(ctx).await;
    };

    // Descend
    let ty = ArchiveType::from_path(&base).ok_or(crate::Error::NotArchive(subpath.len()))?;
    let ctx = RenderContext {
        file: FileThunk::Nested(file, ty, subpath),
        etag: encoded_etag,
    };
    render(ctx).await
}

pub fn read_range(
    file: impl Read + Seek + Send + 'static,
    headers: &HeaderMap,
    etag: &str,
    size: u64,
) -> Result<(Vec<u8>, Option<Range<u64>>)> {
    // Honor `If-Range`: if present and it does not match the current
    // representation, the range is ignored and the full body is served.
    let if_range_ok = headers
        .get(axum::http::header::IF_RANGE)
        .and_then(|v| v.to_str().ok())
        .map(|v| etag::check_header(etag, v))
        .unwrap_or(true);

    // FIXME: don't parse the range based on actual file length.
    // Parse the length as is, seek, try to read as much as possible
    let range = if if_range_ok {
        parse_range(headers, size)?
    } else {
        None
    };

    let mut read_len = size;
    // FIXME: use a state machine to impl Stream
    let sent_range = range.clone();
    let mut file = file;
    if let Some(Range { start, end }) = sent_range {
        file.seek(SeekFrom::Start(start))?;
        read_len = end - start;
    }
    let read_len = read_len as usize;
    let mut body = vec![0; read_len];
    // TODO: use read_buf_exact once stabilized.
    file.read_exact(&mut body)?; // Should not meet premature EoF because we truncated the range
    Ok((body, range))
}

pub async fn finalize_range(
    body: Vec<u8>,
    range: Option<Range<u64>>,
    mime: mime_guess::Mime,
    filename: &str,
    etag: &str,
    size: u64,
) -> Result<Response> {
    let code = if range.is_some() { 206 } else { 200 };
    let mut resp = Response::builder()
        .status(code)
        .header("Content-Type", mime.to_string())
        .header("Etag", format!("\"{etag}\""))
        .header("Cache-Control", "max-age=60, stale-while-revalidate=86400")
        .header(
            axum::http::header::CONTENT_DISPOSITION,
            content_disposition(filename),
        )
        .header(axum::http::header::CONTENT_LENGTH, body.len())
        .header(axum::http::header::ACCEPT_RANGES, "bytes");
    if let Some(Range { start, end }) = range {
        resp = resp.header(
            axum::http::header::CONTENT_RANGE,
            format!("bytes {}-{}/{}", start, end - 1, size),
        );
    }

    Ok(resp.body(Body::from(body)).unwrap())
}

fn content_disposition(filename: &str) -> String {
    let mut fallback = String::with_capacity(filename.len());
    for character in filename.chars() {
        match character {
            '"' | '\\' => {
                fallback.push('\\');
                fallback.push(character);
            }
            ' '..='~' => fallback.push(character),
            _ => fallback.push('_'),
        }
    }

    format!(
        "inline; filename=\"{fallback}\"; filename*=UTF-8''{}",
        urlencoding::encode(filename)
    )
}
