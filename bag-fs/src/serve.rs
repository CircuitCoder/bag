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

pub enum OpenFile<'a> {
    Fs(std::fs::File),
    Nested(crate::file::ArchiveOpen<'a>),
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
    pub file: OpenFile<'a>,
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
    F: (for<'a> FnOnce(RenderContext<'a>) -> Result<Response>) + Send + 'static,
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
        return tokio::task::spawn_blocking(move || {
            let ctx = RenderContext {
                file: OpenFile::Fs(file),
                etag: encoded_etag,
            };
            render(ctx)
        })
        .await?;
    };

    // Descend
    // TODO: move to blocking thread
    let ty = ArchiveType::from_path(&base).ok_or(crate::Error::NotArchive(subpath.len()))?;
    tokio::task::spawn_blocking({
        let subpath = subpath.to_static();
        move || {
            let wrapped = crate::file::File::Fs(file);
            wrapped.descend(ty, &subpath, |open| {
                let ctx = RenderContext {
                    file: OpenFile::Nested(open),
                    etag: encoded_etag,
                };
                render(ctx)
            })
        }
    })
    .await?
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
        .map(|v| etag::check_header(&etag, v))
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
    let mut body = Vec::with_capacity(read_len);
    body.resize(read_len, 0);
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
    // FIXME: filename escaping
    let code = if range.is_some() { 206 } else { 200 };
    let mut resp = Response::builder()
        .status(code)
        .header("Content-Type", mime.to_string())
        .header("Etag", format!("\"{etag}\""))
        .header("Cache-Control", "max-age=60, stale-while-revalidate=86400")
        .header(
            axum::http::header::CONTENT_DISPOSITION,
            format!("inline; filename=\"{filename}\""),
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

/*/
impl FsHandler {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    fn get_nested(
        root: &std::path::Path,
        parsed: ParsedFsPath,
        expected_etag: Option<&str>,
    ) -> Result<FileOpenResult> {
        let file = File::open(root.join(&parsed.outer))?;
        let metadata = file.metadata()?;

        let mtime = metadata.modified()?;
        let length = metadata.len();
        let subpath = (!parsed.steps.is_empty()).then(|| {
            parsed
                .steps
                .iter()
                .map(|step| step.target.as_str())
                .collect::<Vec<_>>()
                .join("/:/")
        });
        let etag = Etag {
            mtime,
            length,
            subpath: subpath.as_deref(),
        };
        let encoded_etag = etag.hash_string();
        let (current_mime, current_file, current_file_size) = if parsed.steps.is_empty() {
            (
                mime_guess::from_path(&parsed.outer).first_or_octet_stream(),
                BufferedFile::Disk(BufReader::new(file)),
                metadata.len(),
            )
        } else {
            drop(file);
            let (mut archive, step) = open_target_archive(root, &parsed)?;
            if step.target.is_empty() {
                return Err(crate::Error::NotFound);
            }
            let (contents, is_directory) = archive
                .read_entry(&step.target, step)?
                .ok_or(crate::Error::NotFound)?;
            if is_directory {
                return Err(crate::Error::NotFound);
            }
            let size = contents.len() as u64;
            (
                mime_guess::from_path(&step.target).first_or_octet_stream(),
                BufferedFile::Memory(Cursor::new(contents)),
                size,
            )
        };

        if let Some(expected_etag) = expected_etag
            && etag::check_header(&encoded_etag, expected_etag)
        {
            return Ok(FileOpenResult::Unchanged(encoded_etag));
        }

        Ok(FileOpenResult::File {
            mime: current_mime,
            file: current_file,
            size: current_file_size,
            etag: encoded_etag,
        })
    }

    async fn load_fs(&self, path: &BagPath<'_>, header: &HeaderMap) -> Result<Response> {
        // FIXME: path sanitization
        // FIXME: path canonicalization
        // FIXME: empty path segment

        // TODO: binary search

        let parsed = parse_fs_path(path)?;
        let filename = path
            .last()
            .map(|segment| segment.name().to_owned())
            .unwrap_or_default();
        let result = tokio::task::spawn_blocking({
            let root = self.root.clone();
            let header_etag = header
                .get(axum::http::header::IF_NONE_MATCH)
                .and_then(|v| v.to_str().ok())
                .map(|e| e.to_string());
            move || FsHandler::get_nested(&root, parsed, header_etag.as_deref())
        })
        .await?;

        let (mime, mut file, size, etag) = match result? {
            Unchanged(etag) => {
                return Ok(Response::builder()
                    .status(304)
                    .header("Etag", format!("\"{etag}\""))
                    .header("Cache-Control", "max-age=60, stale-while-revalidate=86400")
                    .body(Body::empty())
                    .unwrap());
            }
            FileOpenResult::File {
                mime,
                file,
                size,
                etag,
            } => (mime, file, size, etag),
        };

        // Honor `If-Range`: if present and it does not match the current
        // representation, the range is ignored and the full body is served.
        let if_range_ok = header
            .get(axum::http::header::IF_RANGE)
            .and_then(|v| v.to_str().ok())
            .map(|v| etag::check_header(&etag, v))
            .unwrap_or(true);

        // FIXME: don't parse the range based on actual file length.
        // Parse the length as is, seek, try to read as much as possible
        let range = if if_range_ok {
            parse_range(header, size)?
        } else {
            None
        };

        let mut read_len = size;
        // FIXME: use a state machine to impl Stream
        if let Some(Range { start, end }) = range {
            file.skip_to(start)?;
            read_len = end - start;
        }
        let body = tokio::task::spawn_blocking(move || {
            let mut file = file;
            let mut result = Vec::with_capacity(read_len as usize);
            file.read_limited_to_end(read_len, &mut result)
                .map(|_| result)
        })
        .await
        .unwrap()?;
        let code = if range.is_some() { 206 } else { 200 };

        Ok(resp.body(Body::from(body)).unwrap())
    }

    pub async fn handle(&self, path: &BagPath<'_>, headers: &HeaderMap) -> Result<Response> {
        self.load_fs(path, headers).await
    }
}

#[cfg(test)]
mod tests {
    use super::FsHandler;
    use bag_lib::path::Path;
    use std::io::{Cursor, Read, Write};
    use zip::{ZipWriter, write::SimpleFileOptions};

    fn zip_with_file(name: &str, contents: &[u8]) -> Vec<u8> {
        zip_with_files([(name, contents)])
    }

    fn zip_with_files<'a>(files: impl IntoIterator<Item = (&'a str, &'a [u8])>) -> Vec<u8> {
        let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
        for (name, contents) in files {
            writer
                .start_file(name, SimpleFileOptions::default())
                .unwrap();
            writer.write_all(contents).unwrap();
        }
        writer.finish().unwrap().into_inner()
    }

    fn encrypted_zip_with_file(name: &str, contents: &[u8], password: &str) -> Vec<u8> {
        let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
        writer
            .start_file(
                name,
                SimpleFileOptions::default().with_aes_encryption(zip::AesMode::Aes256, password),
            )
            .unwrap();
        writer.write_all(contents).unwrap();
        writer.finish().unwrap().into_inner()
    }

    #[tokio::test]
    async fn reads_percent_encoded_utf8_entry_name() {
        let name = "目录/图.jpg";
        let archive = zip_with_file(name, b"utf8 contents");
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("names.zip"), archive).unwrap();
        let fs = FsHandler::new(directory.path().to_path_buf());
        let path = format!("names.zip/%3A/{}", urlencoding::encode(name));
        let mut reader = fs
            .open_buffered(&Path::try_from(path.as_str()).unwrap())
            .await
            .unwrap();
        let mut contents = Vec::new();
        reader.read_to_end(&mut contents).unwrap();

        assert_eq!(contents, b"utf8 contents");
    }

    #[tokio::test]
    async fn reads_cp437_entry_name_by_decoded_name() {
        let mut archive = zip_with_file("cafX.txt", b"cp437 contents");
        let placeholder = b"cafX.txt";
        let mut replacements = 0;

        for offset in 0..=archive.len() - placeholder.len() {
            if archive[offset..].starts_with(placeholder) {
                archive[offset + 3] = 0x82;
                replacements += 1;
            }
        }
        assert_eq!(replacements, 2);

        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("names.zip"), archive).unwrap();
        let fs = FsHandler::new(directory.path().to_path_buf());
        let path = format!("names.zip/%3A/{}", urlencoding::encode("café.txt"));
        let mut reader = fs
            .open_buffered(&Path::try_from(path.as_str()).unwrap())
            .await
            .unwrap();
        let mut contents = Vec::new();
        reader.read_to_end(&mut contents).unwrap();

        assert_eq!(contents, b"cp437 contents");
    }

    #[tokio::test]
    async fn fetches_files_directories_and_marker_opened_nested_archives() {
        use serve::ArchiveProbe;
        let nested = zip_with_files([("inside.jpg", b"nested".as_slice())]);
        let outer = zip_with_files([
            ("root.jpg", b"root".as_slice()),
            ("folder/photo.jpg", b"photo".as_slice()),
            ("folder/deeper/item.png", b"item".as_slice()),
            ("inner.zip", nested.as_slice()),
        ]);
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("outer.zip"), outer).unwrap();
        let fs = FsHandler::new(directory.path().to_path_buf());

        let ArchiveFetch::Directory(root) = fs
            .archive_fetch(&Path::try_from("outer.zip/%3A").unwrap())
            .await
            .unwrap()
        else {
            panic!("archive root was not a directory");
        };
        assert_eq!(
            root.entries
                .iter()
                .map(|entry| (&*entry.name, &*entry.subpath, entry.is_directory))
                .collect::<Vec<_>>(),
            vec![
                ("folder", "folder", true),
                ("inner.zip", "inner.zip", false),
                ("root.jpg", "root.jpg", false),
            ]
        );

        let ArchiveFetch::Directory(folder) = fs
            .archive_fetch(&Path::try_from("outer.zip/%3A/folder").unwrap())
            .await
            .unwrap()
        else {
            panic!("archive folder was not a directory");
        };
        assert_eq!(
            folder
                .entries
                .iter()
                .map(|entry| (&*entry.name, &*entry.subpath, entry.is_directory))
                .collect::<Vec<_>>(),
            vec![
                ("deeper", "folder/deeper", true),
                ("photo.jpg", "folder/photo.jpg", false),
            ]
        );

        let ArchiveFetch::File { parent } = fs
            .archive_fetch(&Path::try_from("outer.zip/%3A/inner.zip").unwrap())
            .await
            .unwrap()
        else {
            panic!("nested archive without a marker was not a file");
        };
        assert_eq!(
            parent
                .entries
                .iter()
                .find(|entry| entry.name == "inner.zip")
                .map(|entry| entry.is_directory),
            Some(false)
        );

        let ArchiveFetch::Directory(nested) = fs
            .archive_fetch(&Path::try_from("outer.zip/%3A/inner.zip/%3A").unwrap())
            .await
            .unwrap()
        else {
            panic!("marked nested archive was not a directory");
        };
        assert_eq!(nested.entries[0].subpath, "inner.zip/:/inside.jpg");

        let ArchiveFetch::File { parent } = fs
            .archive_fetch(&Path::try_from("outer.zip/%3A/folder/photo.jpg").unwrap())
            .await
            .unwrap()
        else {
            panic!("ordinary archive member was not a file");
        };
        assert!(parent.entries.iter().any(|entry| entry.name == "photo.jpg"));

        assert!(matches!(
            fs.archive_fetch(&Path::try_from("outer.zip/%3A/missing.jpg").unwrap())
                .await,
            Err(crate::Error::NotFound)
        ));

        let mut reader = fs
            .open_buffered(&Path::try_from("outer.zip/%3A/inner.zip/%3A/inside.jpg").unwrap())
            .await
            .unwrap();
        let mut contents = Vec::new();
        reader.read_to_end(&mut contents).unwrap();
        assert_eq!(contents, b"nested");
    }

    #[tokio::test]
    async fn uses_the_password_on_each_archive_marker() {
        let nested = encrypted_zip_with_file("inside.jpg", b"nested", "inner password");
        let outer = encrypted_zip_with_file("inner.zip", &nested, "outer password");
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("outer.zip"), outer).unwrap();
        let fs = FsHandler::new(directory.path().to_path_buf());

        let path = Path::try_from(
            "outer.zip/%3A,pw=outer%20password/inner.zip/%3A,pw=inner%20password/inside.jpg",
        )
        .unwrap();
        let mut reader = fs.open_buffered(&path).await.unwrap();
        let mut contents = Vec::new();
        reader.read_to_end(&mut contents).unwrap();
        assert_eq!(contents, b"nested");

        let missing = Path::try_from("outer.zip/%3A/inner.zip").unwrap();
        assert!(matches!(
            fs.archive_fetch(&missing).await,
            Err(crate::Error::ArchivePassword(path))
                if path == Path::try_from("outer.zip").unwrap()
        ));

        let wrong_outer = Path::try_from("outer.zip/%3A,pw=wrong/inner.zip").unwrap();
        assert!(matches!(
            fs.archive_fetch(&wrong_outer).await,
            Err(crate::Error::ArchivePassword(path))
                if path == Path::try_from("outer.zip").unwrap()
        ));

        let wrong_inner =
            Path::try_from("outer.zip/%3A,pw=outer%20password/inner.zip/%3A,pw=wrong").unwrap();
        assert!(matches!(
            fs.archive_fetch(&wrong_inner).await,
            Err(crate::Error::ArchivePassword(path))
                if path == Path::try_from("outer.zip/%3A/inner.zip").unwrap()
        ));
    }

    #[tokio::test]
    async fn distinguishes_non_archives_from_invalid_zip_files() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("plain.txt"), b"plain").unwrap();
        std::fs::write(directory.path().join("broken.zip"), b"not a zip").unwrap();
        std::fs::create_dir(directory.path().join("folder")).unwrap();
        let fs = FsHandler::new(directory.path().to_path_buf());

        for path in ["plain.txt/%3A", "folder/%3A"] {
            assert!(matches!(
                fs.archive_fetch(&Path::try_from(path).unwrap()).await,
                Err(crate::Error::NotArchive(_))
            ));
        }
        assert!(matches!(
            fs.archive_fetch(&Path::try_from("broken.zip/%3A").unwrap())
                .await,
            Err(crate::Error::Zip(zip::result::ZipError::InvalidArchive(_)))
        ));
    }
}
*/
