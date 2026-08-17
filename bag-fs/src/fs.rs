// Default fs handler

use axum::{body::Body, http::HeaderMap, response::Response};
use ouroboros::self_referencing;
use std::{
    fs::File,
    io::{self, Cursor, Read, Seek, SeekFrom},
    ops::Range,
    os::unix::fs::MetadataExt,
    path::PathBuf,
};
use zip::{HasZipMetadata, ZipArchive};

use crate::{
    Result,
    etag::{self, Etag},
    fs::FileOpenResult::Unchanged,
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

#[derive(Clone)]
pub struct FsHandler {
    root: PathBuf,
}

enum FileOpenResult {
    File {
        mime: mime_guess::mime::Mime,
        file: NestedFile,
        size: u64,
        etag: String,
    },
    InvalidArchive,
    NotAFile, // Not found or directory
    Unchanged(String),
}

#[self_referencing]
struct ZipFileDisk {
    archive: ZipArchive<File>,
    #[borrows(mut archive)]
    #[not_covariant]
    file: zip::read::ZipFile<'this, File>,
}

#[self_referencing]
struct ZipFileMem {
    archive: ZipArchive<Cursor<Vec<u8>>>,
    #[borrows(mut archive)]
    #[not_covariant]
    file: zip::read::ZipFile<'this, Cursor<Vec<u8>>>,
}

enum NestedOpen {
    File(NestedFile, u64),
    InvalidArchive,
    NotAFile,
}

enum NestedFile {
    File(File),
    ZipFileDisk(ZipFileDisk),
    ZipFileMem(ZipFileMem),
}

impl NestedFile {
    fn open_zip_entry(self, nest: &str) -> Result<NestedOpen> {
        match self {
            NestedFile::File(file) => open_disk_zip_entry(file, nest),
            NestedFile::ZipFileDisk(mut file) => {
                let mut buffer = Vec::new();
                file.with_file_mut(|file| file.read_to_end(&mut buffer))?;
                open_mem_zip_entry(buffer, nest)
            }
            NestedFile::ZipFileMem(mut file) => {
                let mut buffer = Vec::new();
                file.with_file_mut(|file| file.read_to_end(&mut buffer))?;
                open_mem_zip_entry(buffer, nest)
            }
        }
    }

    fn skip_to(&mut self, offset: u64) -> io::Result<()> {
        match self {
            NestedFile::File(file) => file.seek(SeekFrom::Start(offset)).map(|_| ()),
            NestedFile::ZipFileDisk(file) => file.with_file_mut(|file| discard_exact(file, offset)),
            NestedFile::ZipFileMem(file) => file.with_file_mut(|file| discard_exact(file, offset)),
        }
    }

    fn read_limited_to_end(&mut self, limit: u64, result: &mut Vec<u8>) -> io::Result<usize> {
        match self {
            NestedFile::File(file) => file.take(limit).read_to_end(result),
            NestedFile::ZipFileDisk(file) => {
                file.with_file_mut(|file| file.take(limit).read_to_end(result))
            }
            NestedFile::ZipFileMem(file) => {
                file.with_file_mut(|file| file.take(limit).read_to_end(result))
            }
        }
    }
}

fn discard_exact(reader: &mut impl Read, len: u64) -> io::Result<()> {
    let copied = io::copy(&mut reader.take(len), &mut io::sink())?;
    if copied == len {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "failed to skip requested number of bytes",
        ))
    }
}

fn find_zip_entry<R: Read + Seek>(archive: &ZipArchive<R>, path: &str) -> Option<usize> {
    let path = urlencoding::decode(path).ok()?;

    archive.index_for_path(path.as_ref()).or_else(|| {
        (0..archive.len()).find(|&index| archive.name_for_index(index) == Some(path.as_ref()))
    })
}

fn open_disk_zip_entry(file: File, nest: &str) -> Result<NestedOpen> {
    let archive = match ZipArchive::new(file) {
        Ok(archive) => archive,
        Err(_) => return Ok(NestedOpen::InvalidArchive),
    };
    let Some(index) = find_zip_entry(&archive, nest) else {
        return Ok(NestedOpen::NotAFile);
    };
    let file = match (ZipFileDiskTryBuilder {
        archive,
        file_builder: move |archive| archive.by_index(index),
    })
    .try_build()
    {
        Ok(file) => file,
        Err(_) => return Ok(NestedOpen::NotAFile),
    };
    let size = file.with_file(|file| file.get_metadata().uncompressed_size);
    Ok(NestedOpen::File(NestedFile::ZipFileDisk(file), size))
}

fn open_mem_zip_entry(buffer: Vec<u8>, nest: &str) -> Result<NestedOpen> {
    let archive = match ZipArchive::new(Cursor::new(buffer)) {
        Ok(archive) => archive,
        Err(_) => return Ok(NestedOpen::InvalidArchive),
    };
    let Some(index) = find_zip_entry(&archive, nest) else {
        return Ok(NestedOpen::NotAFile);
    };
    let file = match (ZipFileMemTryBuilder {
        archive,
        file_builder: move |archive| archive.by_index(index),
    })
    .try_build()
    {
        Ok(file) => file,
        Err(_) => return Ok(NestedOpen::NotAFile),
    };
    let size = file.with_file(|file| file.get_metadata().uncompressed_size);
    Ok(NestedOpen::File(NestedFile::ZipFileMem(file), size))
}

impl FsHandler {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    fn get_nested(
        tgt: &str,
        subpath: Option<&str>,
        expected_etag: Option<&str>,
    ) -> Result<FileOpenResult> {
        let file = std::fs::File::open(tgt)?;
        let metadata = file.metadata()?;

        let mtime = metadata.modified()?;
        let length = metadata.len();
        let etag = Etag {
            mtime,
            length,
            subpath,
        };
        let encoded_etag = etag.hash_string();
        if let Some(expected_etag) = expected_etag
            && etag::check_header(&encoded_etag, expected_etag)
        {
            return Ok(FileOpenResult::Unchanged(encoded_etag));
        }

        let nests = subpath.iter().flat_map(|s| s.split("/:/"));
        let mut current_mime = mime_guess::from_path(tgt).first_or_octet_stream();
        let mut current_file = NestedFile::File(file);
        let mut current_file_size = metadata.size();

        for nest in nests {
            if current_mime != "application/zip" {
                return Ok(FileOpenResult::InvalidArchive);
            }

            let (file, size) = match current_file.open_zip_entry(nest)? {
                NestedOpen::File(file, size) => (file, size),
                NestedOpen::InvalidArchive => return Ok(FileOpenResult::InvalidArchive),
                NestedOpen::NotAFile => return Ok(FileOpenResult::NotAFile),
            };
            current_file_size = size;
            current_mime = mime_guess::from_path(nest).first_or_octet_stream();
            current_file = file;
        }

        Ok(FileOpenResult::File {
            mime: current_mime,
            file: current_file,
            size: current_file_size,
            etag: encoded_etag,
        })
    }

    async fn load_fs(&self, path: &str, header: &HeaderMap) -> Result<Response> {
        // FIXME: path sanitization
        // FIXME: path canonicalization
        // FIXME: empty path segment

        // TODO: binary search

        // Part 1: get a Read + Seek handle to the (possibly nested) file.
        let segs = path.split_once("/:/");
        let path = segs.map(|(p, _)| p).unwrap_or(path);
        let subpath = segs.map(|(_, s)| s);

        let tgt = self.root.join(path);
        let result = tokio::task::spawn_blocking({
            let tgt = tgt.clone();
            let subpath = subpath.map(|s| s.to_string());
            let header_etag = header
                .get(axum::http::header::IF_NONE_MATCH)
                .and_then(|v| v.to_str().ok())
                .map(|e| e.to_string());
            move || {
                FsHandler::get_nested(
                    tgt.to_str().unwrap(),
                    subpath.as_deref(),
                    header_etag.as_deref(),
                )
            }
        })
        .await
        .unwrap();

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
            FileOpenResult::InvalidArchive => {
                return Ok(Response::builder()
                    .status(400)
                    .body("Invalid archive".into())
                    .unwrap());
            }
            FileOpenResult::NotAFile => return Err(crate::Error::NotFound),
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

        let mut resp = Response::builder()
            .status(code)
            .header("Content-Type", mime.to_string())
            .header("Etag", format!("\"{etag}\""))
            .header("Cache-Control", "max-age=60, stale-while-revalidate=86400")
            .header(
                axum::http::header::CONTENT_DISPOSITION,
                format!(
                    "inline; filename=\"{}\"",
                    tgt.file_name().unwrap().to_string_lossy()
                ),
            )
            .header(axum::http::header::CONTENT_LENGTH, read_len)
            .header(axum::http::header::ACCEPT_RANGES, "bytes");
        if let Some(Range { start, end }) = range {
            resp = resp.header(
                axum::http::header::CONTENT_RANGE,
                format!("bytes {}-{}/{}", start, end - 1, size),
            );
        }
        Ok(resp.body(Body::from(body)).unwrap())
    }

    pub async fn handle(&self, path: &str, headers: &HeaderMap) -> Response {
        match self.load_fs(path, headers).await {
            Err(crate::Error::NotFound) => {
                Response::builder()
                    .status(404)
                    .body("Not Found".into())
                    .unwrap()
            }
            Err(crate::Error::IoError(e)) if e.kind() == std::io::ErrorKind::NotFound => {
                Response::builder()
                    .status(404)
                    .body("Not Found".into())
                    .unwrap()
            }
            Err(crate::Error::IoError(e)) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                Response::builder()
                    .status(403)
                    .body("Permission Denied".into())
                    .unwrap()
            }
            Err(crate::Error::IoError(e)) if e.kind() == std::io::ErrorKind::IsADirectory => {
                Response::builder()
                    .status(400)
                    .body("Reading a directory".into())
                    .unwrap()
            }
            Err(crate::Error::RangeUnsatisfiable) => {
                Response::builder()
                    .status(416)
                    .body("Range Unsatisfiable".into())
                    .unwrap()
            }
            Err(e) => {
                Response::builder()
                    .status(500)
                    .body(format!("Internal Server Error: {}", e).into())
                    .unwrap()
            }
            Ok(resp) => resp,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{NestedFile, NestedOpen, open_mem_zip_entry};
    use std::io::{Cursor, Write};
    use zip::{ZipWriter, write::SimpleFileOptions};

    fn zip_with_file(name: &str, contents: &[u8]) -> Vec<u8> {
        let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
        writer
            .start_file(name, SimpleFileOptions::default())
            .unwrap();
        writer.write_all(contents).unwrap();
        writer.finish().unwrap().into_inner()
    }

    fn read_entry(archive: Vec<u8>, path: &str) -> Vec<u8> {
        let NestedOpen::File(mut file, size) = open_mem_zip_entry(archive, path).unwrap() else {
            panic!("ZIP entry was not opened");
        };
        assert!(matches!(file, NestedFile::ZipFileMem(_)));

        let mut contents = Vec::new();
        file.read_limited_to_end(size, &mut contents).unwrap();
        contents
    }

    #[test]
    fn reads_percent_encoded_utf8_entry_name() {
        let name = "目录/图.jpg";
        let archive = zip_with_file(name, b"utf8 contents");
        let encoded_name = urlencoding::encode(name);

        assert_eq!(read_entry(archive, &encoded_name), b"utf8 contents");
    }

    #[test]
    fn reads_cp437_entry_name_by_decoded_name() {
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

        let encoded_name = urlencoding::encode("café.txt");
        assert_eq!(read_entry(archive, &encoded_name), b"cp437 contents");
    }
}
