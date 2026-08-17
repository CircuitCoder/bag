// Default fs handler

use axum::{body::Body, http::HeaderMap, response::Response};
use ouroboros::self_referencing;
use std::{
    collections::BTreeMap,
    fs::File,
    io::{self, BufRead, BufReader, Cursor, Read, Seek, SeekFrom},
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

pub enum BufferedFile {
    Disk(BufReader<File>),
    Memory(Cursor<Vec<u8>>),
}

impl Read for BufferedFile {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Disk(reader) => reader.read(buf),
            Self::Memory(reader) => reader.read(buf),
        }
    }
}

impl BufRead for BufferedFile {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        match self {
            Self::Disk(reader) => reader.fill_buf(),
            Self::Memory(reader) => reader.fill_buf(),
        }
    }

    fn consume(&mut self, amount: usize) {
        match self {
            Self::Disk(reader) => reader.consume(amount),
            Self::Memory(reader) => reader.consume(amount),
        }
    }
}

impl Seek for BufferedFile {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        match self {
            Self::Disk(reader) => reader.seek(position),
            Self::Memory(reader) => reader.seek(position),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchiveEntry {
    pub name: String,
    pub subpath: String,
    pub is_directory: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchiveListing {
    pub entries: Vec<ArchiveEntry>,
    pub is_archive: bool,
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

    find_zip_entry_decoded(archive, &path)
}

fn find_zip_entry_decoded<R: Read + Seek>(archive: &ZipArchive<R>, path: &str) -> Option<usize> {
    archive
        .index_for_path(path)
        .or_else(|| (0..archive.len()).find(|&index| archive.name_for_index(index) == Some(path)))
}

fn decoded_zip_name(file: &zip::read::ZipFile<'_, impl Read>) -> String {
    std::str::from_utf8(file.name_raw())
        .unwrap_or_else(|_| file.name())
        .to_owned()
}

#[derive(Debug)]
struct ZipEntryInfo {
    name: String,
    is_directory: bool,
}

fn archive_entry_info<R: Read + Seek>(
    archive: &mut ZipArchive<R>,
    path: &str,
) -> zip::result::ZipResult<Option<ZipEntryInfo>> {
    let Some(index) = find_zip_entry_decoded(archive, path) else {
        return Ok(None);
    };
    let file = archive.by_index_raw(index)?;
    Ok(Some(ZipEntryInfo {
        name: decoded_zip_name(&file),
        is_directory: file.is_dir(),
    }))
}

fn archive_entries<R: Read + Seek>(
    archive: &mut ZipArchive<R>,
) -> zip::result::ZipResult<Vec<ZipEntryInfo>> {
    (0..archive.len())
        .map(|index| {
            let file = archive.by_index_raw(index)?;
            Ok(ZipEntryInfo {
                name: decoded_zip_name(&file),
                is_directory: file.is_dir(),
            })
        })
        .collect()
}

fn read_archive_entry<R: Read + Seek>(
    archive: &mut ZipArchive<R>,
    path: &str,
) -> zip::result::ZipResult<Option<Vec<u8>>> {
    let Some(index) = find_zip_entry_decoded(archive, path) else {
        return Ok(None);
    };
    let mut file = archive.by_index(index)?;
    let mut contents = Vec::with_capacity(file.size() as usize);
    file.read_to_end(&mut contents)?;
    Ok(Some(contents))
}

enum OpenArchive {
    Disk(ZipArchive<File>),
    Memory(ZipArchive<Cursor<Vec<u8>>>),
}

impl OpenArchive {
    fn entry_info(&mut self, path: &str) -> zip::result::ZipResult<Option<ZipEntryInfo>> {
        match self {
            Self::Disk(archive) => archive_entry_info(archive, path),
            Self::Memory(archive) => archive_entry_info(archive, path),
        }
    }

    fn entries(&mut self) -> zip::result::ZipResult<Vec<ZipEntryInfo>> {
        match self {
            Self::Disk(archive) => archive_entries(archive),
            Self::Memory(archive) => archive_entries(archive),
        }
    }

    fn read_entry(&mut self, path: &str) -> zip::result::ZipResult<Option<Vec<u8>>> {
        match self {
            Self::Disk(archive) => read_archive_entry(archive, path),
            Self::Memory(archive) => read_archive_entry(archive, path),
        }
    }

    fn open_nested(&mut self, path: &str) -> Result<OpenArchive> {
        let contents = self.read_entry(path)?.ok_or(crate::Error::NotFound)?;
        Ok(Self::Memory(ZipArchive::new(Cursor::new(contents))?))
    }
}

fn is_zip_path(path: &str) -> bool {
    mime_guess::from_path(path).first_or_octet_stream() == "application/zip"
}

fn join_archive_subpath(archive: &str, entry: &str) -> String {
    if archive.is_empty() {
        entry.to_owned()
    } else {
        format!("{archive}/:/{entry}")
    }
}

fn split_archive_path(path: &str) -> (&str, Option<&str>) {
    path.split_once("/:/")
        .map_or((path, None), |(outer, subpath)| (outer, Some(subpath)))
}

fn list_archive(root: &std::path::Path, path: &str) -> Result<ArchiveListing> {
    let (outer, subpath) = split_archive_path(path);
    let mut archive = OpenArchive::Disk(ZipArchive::new(File::open(root.join(outer))?)?);
    let mut archive_subpath = String::new();
    let mut prefix = "";
    let mut is_archive = true;

    if let Some(subpath) = subpath {
        let parts = subpath.split("/:/").collect::<Vec<_>>();
        for part in &parts[..parts.len().saturating_sub(1)] {
            archive = archive.open_nested(part)?;
            archive_subpath = join_archive_subpath(&archive_subpath, part);
        }

        let target = parts.last().copied().ok_or(crate::Error::NotFound)?;
        let target_info = archive.entry_info(target)?;
        if target_info
            .as_ref()
            .is_some_and(|entry| !entry.is_directory && is_zip_path(&entry.name))
        {
            archive = archive.open_nested(target)?;
            archive_subpath = join_archive_subpath(&archive_subpath, target);
        } else {
            let normalized = target.trim_end_matches('/');
            let child_prefix = format!("{normalized}/");
            let has_children = archive
                .entries()?
                .iter()
                .any(|entry| entry.name.starts_with(&child_prefix));
            if !target_info.is_some_and(|entry| entry.is_directory) && !has_children {
                return Err(crate::Error::NotFound);
            }
            prefix = normalized;
            is_archive = false;
        }
    }

    let child_prefix = (!prefix.is_empty()).then(|| format!("{prefix}/"));
    let mut children = BTreeMap::<String, bool>::new();
    for entry in archive.entries()? {
        let name = entry.name.trim_end_matches('/');
        let remainder = match &child_prefix {
            Some(child_prefix) => match name.strip_prefix(child_prefix) {
                Some(remainder) => remainder,
                None => continue,
            },
            None => name,
        };
        if remainder.is_empty() {
            continue;
        }

        let (name, is_directory) = match remainder.split_once('/') {
            Some((name, _)) => (name, true),
            None => (remainder, entry.is_directory || is_zip_path(remainder)),
        };
        if name.is_empty() {
            continue;
        }
        children
            .entry(name.to_owned())
            .and_modify(|existing| *existing |= is_directory)
            .or_insert(is_directory);
    }

    let mut entries = children
        .into_iter()
        .map(|(name, is_directory)| {
            let entry_path = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix}/{name}")
            };
            ArchiveEntry {
                name,
                subpath: join_archive_subpath(&archive_subpath, &entry_path),
                is_directory,
            }
        })
        .collect::<Vec<_>>();
    entries.sort_by(|left, right| {
        right
            .is_directory
            .cmp(&left.is_directory)
            .then_with(|| left.name.cmp(&right.name))
    });

    Ok(ArchiveListing {
        entries,
        is_archive,
    })
}

fn archive_file_exists(root: &std::path::Path, path: &str) -> Result<bool> {
    let (outer, subpath) = split_archive_path(path);
    let Some(subpath) = subpath else {
        return Ok(root.join(outer).is_file());
    };
    let parts = subpath.split("/:/").collect::<Vec<_>>();
    let (entry, archive_parts) = parts.split_last().ok_or(crate::Error::NotFound)?;
    let mut archive = OpenArchive::Disk(ZipArchive::new(File::open(root.join(outer))?)?);
    for archive_entry in archive_parts {
        archive = archive.open_nested(archive_entry)?;
    }
    Ok(archive
        .entry_info(entry)?
        .is_some_and(|entry| !entry.is_directory))
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

    pub async fn open_buffered(&self, path: &str) -> Result<BufferedFile> {
        let root = self.root.clone();
        let path = path.to_owned();
        tokio::task::spawn_blocking(move || {
            let (outer, subpath) = split_archive_path(&path);
            let file = File::open(root.join(outer))?;
            let Some(subpath) = subpath else {
                return Ok(BufferedFile::Disk(BufReader::new(file)));
            };

            let parts = subpath.split("/:/").collect::<Vec<_>>();
            let (entry, archive_parts) = parts.split_last().ok_or(crate::Error::NotFound)?;
            let mut archive = OpenArchive::Disk(ZipArchive::new(file)?);
            for archive_entry in archive_parts {
                archive = archive.open_nested(archive_entry)?;
            }
            let contents = archive.read_entry(entry)?.ok_or(crate::Error::NotFound)?;
            Ok(BufferedFile::Memory(Cursor::new(contents)))
        })
        .await?
    }

    pub async fn list_archive(&self, path: &str) -> Result<ArchiveListing> {
        let root = self.root.clone();
        let path = path.to_owned();
        tokio::task::spawn_blocking(move || list_archive(&root, &path)).await?
    }

    pub async fn archive_file_exists(&self, path: &str) -> Result<bool> {
        let root = self.root.clone();
        let path = path.to_owned();
        tokio::task::spawn_blocking(move || archive_file_exists(&root, &path)).await?
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
    use super::{FsHandler, NestedFile, NestedOpen, open_mem_zip_entry};
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

    #[tokio::test]
    async fn lists_implicit_directories_and_nested_archives() {
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

        let root = fs.list_archive("outer.zip").await.unwrap();
        assert!(root.is_archive);
        assert_eq!(
            root.entries
                .iter()
                .map(|entry| (&*entry.name, &*entry.subpath, entry.is_directory))
                .collect::<Vec<_>>(),
            vec![
                ("folder", "folder", true),
                ("inner.zip", "inner.zip", true),
                ("root.jpg", "root.jpg", false),
            ]
        );

        let folder = fs.list_archive("outer.zip/:/folder").await.unwrap();
        assert!(!folder.is_archive);
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

        let nested = fs.list_archive("outer.zip/:/inner.zip").await.unwrap();
        assert!(nested.is_archive);
        assert_eq!(nested.entries[0].subpath, "inner.zip/:/inside.jpg");

        let mut reader = fs
            .open_buffered("outer.zip/:/inner.zip/:/inside.jpg")
            .await
            .unwrap();
        let mut contents = Vec::new();
        reader.read_to_end(&mut contents).unwrap();
        assert_eq!(contents, b"nested");
    }
}
