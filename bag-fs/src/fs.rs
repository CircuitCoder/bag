// Default fs handler

use axum::{body::Body, http::HeaderMap, response::Response};
use bag_lib::path::{Path as BagPath, Segment};
use std::{
    borrow::Cow,
    collections::BTreeMap,
    fs::File,
    io::{self, BufRead, BufReader, Cursor, Read, Seek, SeekFrom},
    ops::Range,
    path::PathBuf,
};
use zip::ZipArchive;

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

impl BufferedFile {
    fn skip_to(&mut self, offset: u64) -> io::Result<()> {
        self.seek(SeekFrom::Start(offset)).map(|_| ())
    }

    fn read_limited_to_end(&mut self, limit: u64, result: &mut Vec<u8>) -> io::Result<usize> {
        self.take(limit).read_to_end(result)
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
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArchiveFetch {
    File { parent: ArchiveListing },
    Directory(ArchiveListing),
}

enum FileOpenResult {
    File {
        mime: mime_guess::mime::Mime,
        file: BufferedFile,
        size: u64,
        etag: String,
    },
    Unchanged(String),
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

struct ArchiveStep {
    reference: BagPath<'static>,
    password: Option<Vec<u8>>,
    target: String,
}

struct ParsedFsPath {
    outer: String,
    steps: Vec<ArchiveStep>,
}

fn sanitized_path_prefix(path: &BagPath<'_>, end: usize) -> BagPath<'static> {
    BagPath(Cow::Owned(
        path.segments()[..end]
            .iter()
            .map(|segment| Segment(Cow::Owned(segment.name().to_owned()), BTreeMap::new()))
            .collect(),
    ))
}

fn parse_fs_path(path: &BagPath<'_>) -> Result<ParsedFsPath> {
    let segments = path.segments();
    let markers = segments
        .iter()
        .enumerate()
        .filter_map(|(index, segment)| (segment.name() == ":").then_some(index))
        .collect::<Vec<_>>();
    let outer_end = markers.first().copied().unwrap_or(segments.len());
    let outer = segments[..outer_end]
        .iter()
        .map(Segment::name)
        .collect::<Vec<_>>()
        .join("/");
    if outer.is_empty() {
        return Err(crate::Error::NotFound);
    }

    let mut steps = Vec::with_capacity(markers.len());
    for (position, marker) in markers.iter().copied().enumerate() {
        let target_end = markers.get(position + 1).copied().unwrap_or(segments.len());
        let target = segments[marker + 1..target_end]
            .iter()
            .map(Segment::name)
            .collect::<Vec<_>>()
            .join("/");
        if target.is_empty() && position + 1 < markers.len() {
            return Err(crate::Error::NotFound);
        }
        steps.push(ArchiveStep {
            reference: sanitized_path_prefix(path, marker),
            password: segments[marker]
                .arg("pw")
                .map(|value| value.as_bytes().to_vec()),
            target,
        });
    }

    Ok(ParsedFsPath { outer, steps })
}

fn is_zip_path(path: &str) -> bool {
    std::path::Path::new(path)
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("zip"))
}

fn ensure_zip(reference: &BagPath<'static>, is_directory: bool) -> Result<()> {
    if is_directory
        || !reference
            .last()
            .is_some_and(|segment| is_zip_path(segment.name()))
    {
        return Err(crate::Error::NotArchive(reference.clone()));
    }
    Ok(())
}

fn password_error(error: &zip::result::ZipError) -> bool {
    matches!(
        error,
        zip::result::ZipError::InvalidPassword
            | zip::result::ZipError::UnsupportedArchive(zip::result::ZipError::PASSWORD_REQUIRED)
    )
}

fn map_zip_error<T>(result: zip::result::ZipResult<T>, reference: &BagPath<'static>) -> Result<T> {
    result.map_err(|error| {
        if password_error(&error) {
            crate::Error::ArchivePassword(reference.clone())
        } else {
            crate::Error::Zip(error)
        }
    })
}

fn read_zip_file(
    mut file: zip::read::ZipFile<'_, impl Read>,
    encrypted: bool,
    reference: &BagPath<'static>,
) -> Result<Vec<u8>> {
    let mut contents = Vec::with_capacity(file.size() as usize);
    file.read_to_end(&mut contents).map_err(|error| {
        if encrypted
            && matches!(
                error.kind(),
                io::ErrorKind::InvalidData | io::ErrorKind::InvalidInput
            )
        {
            crate::Error::ArchivePassword(reference.clone())
        } else {
            crate::Error::IoError(error)
        }
    })?;
    Ok(contents)
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

fn validate_archive_password<R: Read + Seek>(
    archive: &mut ZipArchive<R>,
    password: Option<&[u8]>,
    reference: &BagPath<'static>,
) -> Result<()> {
    let mut encrypted_index = None;
    for index in 0..archive.len() {
        let file = map_zip_error(archive.by_index_raw(index), reference)?;
        if file.encrypted() {
            encrypted_index = Some(index);
            break;
        }
    }
    let Some(index) = encrypted_index else {
        return Ok(());
    };
    let Some(password) = password else {
        return Err(crate::Error::ArchivePassword(reference.clone()));
    };
    let file = map_zip_error(archive.by_index_decrypt(index, password), reference)?;
    read_zip_file(file, true, reference).map(|_| ())
}

fn read_archive_entry<R: Read + Seek>(
    archive: &mut ZipArchive<R>,
    path: &str,
    password: Option<&[u8]>,
    reference: &BagPath<'static>,
) -> Result<Option<(Vec<u8>, bool)>> {
    let Some(index) = find_zip_entry_decoded(archive, path) else {
        return Ok(None);
    };
    let raw = map_zip_error(archive.by_index_raw(index), reference)?;
    let encrypted = raw.encrypted();
    let is_directory = raw.is_dir();
    drop(raw);
    if encrypted && password.is_none() {
        return Err(crate::Error::ArchivePassword(reference.clone()));
    }
    let file = if let Some(password) = password {
        map_zip_error(archive.by_index_decrypt(index, password), reference)?
    } else {
        map_zip_error(archive.by_index(index), reference)?
    };
    Ok(Some((
        read_zip_file(file, encrypted, reference)?,
        is_directory,
    )))
}

enum OpenArchive {
    Disk(ZipArchive<File>),
    Memory(ZipArchive<Cursor<Vec<u8>>>),
}

impl OpenArchive {
    fn entries(&mut self) -> Result<Vec<ZipEntryInfo>> {
        match self {
            Self::Disk(archive) => Ok(archive_entries(archive)?),
            Self::Memory(archive) => Ok(archive_entries(archive)?),
        }
    }

    fn validate_password(&mut self, step: &ArchiveStep) -> Result<()> {
        match self {
            Self::Disk(archive) => {
                validate_archive_password(archive, step.password.as_deref(), &step.reference)
            }
            Self::Memory(archive) => {
                validate_archive_password(archive, step.password.as_deref(), &step.reference)
            }
        }
    }

    fn read_entry(&mut self, path: &str, step: &ArchiveStep) -> Result<Option<(Vec<u8>, bool)>> {
        match self {
            Self::Disk(archive) => {
                read_archive_entry(archive, path, step.password.as_deref(), &step.reference)
            }
            Self::Memory(archive) => {
                read_archive_entry(archive, path, step.password.as_deref(), &step.reference)
            }
        }
    }
}

fn join_archive_subpath(archive: &str, entry: &str) -> String {
    if archive.is_empty() {
        entry.to_owned()
    } else {
        format!("{archive}/:/{entry}")
    }
}

fn archive_listing(
    entries: &[ZipEntryInfo],
    archive_subpath: &str,
    prefix: &str,
) -> ArchiveListing {
    let child_prefix = (!prefix.is_empty()).then(|| format!("{prefix}/"));
    let mut children = BTreeMap::<String, bool>::new();
    for entry in entries {
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
            None => (remainder, entry.is_directory),
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
                subpath: join_archive_subpath(archive_subpath, &entry_path),
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

    ArchiveListing { entries }
}

fn open_target_archive<'a>(
    root: &std::path::Path,
    parsed: &'a ParsedFsPath,
) -> Result<(OpenArchive, &'a ArchiveStep)> {
    let first = parsed.steps.first().ok_or(crate::Error::NotFound)?;
    let file = File::open(root.join(&parsed.outer))?;
    let metadata = file.metadata()?;
    ensure_zip(&first.reference, metadata.is_dir())?;
    let mut archive = OpenArchive::Disk(ZipArchive::new(file)?);

    for (index, step) in parsed.steps.iter().enumerate() {
        archive.validate_password(step)?;
        if index + 1 == parsed.steps.len() {
            return Ok((archive, step));
        }
        let (contents, is_directory) = archive
            .read_entry(&step.target, step)?
            .ok_or(crate::Error::NotFound)?;
        let next = &parsed.steps[index + 1];
        ensure_zip(&next.reference, is_directory)?;
        archive = OpenArchive::Memory(ZipArchive::new(Cursor::new(contents))?);
    }

    unreachable!("an archive path always has at least one step")
}

fn archive_fetch(root: &std::path::Path, parsed: ParsedFsPath) -> Result<ArchiveFetch> {
    let (mut archive, step) = open_target_archive(root, &parsed)?;
    let mut archive_subpath = String::new();
    for nested_archive in parsed.steps[..parsed.steps.len() - 1]
        .iter()
        .map(|step| &step.target)
    {
        archive_subpath = join_archive_subpath(&archive_subpath, nested_archive);
    }

    let target = &step.target;
    let entries = archive.entries()?;
    if target.is_empty() {
        return Ok(ArchiveFetch::Directory(archive_listing(
            &entries,
            &archive_subpath,
            "",
        )));
    }

    let target = target.trim_end_matches('/');
    let child_prefix = format!("{target}/");
    let target_info = entries
        .iter()
        .find(|entry| entry.name.trim_end_matches('/') == target);
    let has_children = entries
        .iter()
        .any(|entry| entry.name.starts_with(&child_prefix));
    if target_info.is_some_and(|entry| entry.is_directory) || has_children {
        return Ok(ArchiveFetch::Directory(archive_listing(
            &entries,
            &archive_subpath,
            target,
        )));
    }
    if target_info.is_none() {
        return Err(crate::Error::NotFound);
    }

    let parent = target.rsplit_once('/').map_or("", |(parent, _)| parent);
    Ok(ArchiveFetch::File {
        parent: archive_listing(&entries, &archive_subpath, parent),
    })
}

impl FsHandler {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    pub async fn open_buffered(&self, path: &BagPath<'_>) -> Result<BufferedFile> {
        let root = self.root.clone();
        let parsed = parse_fs_path(path)?;
        tokio::task::spawn_blocking(move || {
            if parsed.steps.is_empty() {
                let file = File::open(root.join(&parsed.outer))?;
                return Ok(BufferedFile::Disk(BufReader::new(file)));
            }
            let (mut archive, step) = open_target_archive(&root, &parsed)?;
            if step.target.is_empty() {
                return Err(crate::Error::NotFound);
            }
            let (contents, is_directory) = archive
                .read_entry(&step.target, step)?
                .ok_or(crate::Error::NotFound)?;
            if is_directory {
                return Err(crate::Error::NotFound);
            }
            Ok(BufferedFile::Memory(Cursor::new(contents)))
        })
        .await?
    }

    pub async fn archive_fetch(&self, path: &BagPath<'_>) -> Result<ArchiveFetch> {
        let root = self.root.clone();
        let parsed = parse_fs_path(path)?;
        tokio::task::spawn_blocking(move || archive_fetch(&root, parsed)).await?
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

        let mut resp = Response::builder()
            .status(code)
            .header("Content-Type", mime.to_string())
            .header("Etag", format!("\"{etag}\""))
            .header("Cache-Control", "max-age=60, stale-while-revalidate=86400")
            .header(
                axum::http::header::CONTENT_DISPOSITION,
                format!("inline; filename=\"{filename}\""),
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
        use super::ArchiveFetch;
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
