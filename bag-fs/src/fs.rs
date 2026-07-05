// Default fs handler

use axum::{body::Body, http::HeaderMap, response::Response};
use std::{
    cell::UnsafeCell,
    io::{Read, Seek},
    marker::PhantomData,
    os::unix::fs::MetadataExt,
    path::PathBuf,
    ptr::NonNull,
    range::Range,
};
use typed_arena::Arena;
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

trait FileOpen: Read + Seek + Send {}
impl<T: Read + Seek + Send> FileOpen for T {}

enum ArenaSlot<'a> {
    ZipArchive(ZipArchive<&'a mut Box<dyn FileOpen + 'a>>),
    File(Box<dyn FileOpen + 'a>),
}

trait ArenaSlotVariant<'a> {
    fn into_arena(self, arena: &'a Arena<ArenaSlot<'a>>) -> &'a mut Self;
}

macro_rules! impl_slot {
    (type $t:ty, $v:ident) => {
        impl<'a> ArenaSlotVariant<'a> for $t {
            fn into_arena(self, arena: &'a Arena<ArenaSlot<'a>>) -> &'a mut Self {
                let slot = ArenaSlot::$v(self);
                let slot_ref = arena.alloc(slot);
                match slot_ref {
                    ArenaSlot::$v(inner) => inner,
                    _ => unreachable!(),
                }
            }
        }
    };
}

impl_slot!(type ZipArchive<&mut Box<dyn FileOpen + 'a>>, ZipArchive);
impl_slot!(type Box<dyn FileOpen + 'a>, File);

struct NestedFile {
    _arena: Box<Arena<ArenaSlot<'static>>>,
    file: NonNull<Box<dyn FileOpen + 'static>>,
    _not_sync: PhantomData<UnsafeCell<()>>,
}

// `NestedFile` has exclusive access to the final reader and keeps the backing
// arena alive. It is not `Sync`, so the reader cannot be accessed concurrently.
unsafe impl Send for NestedFile {}

impl NestedFile {
    fn new(arena: Box<Arena<ArenaSlot<'static>>>, file: &'static mut Box<dyn FileOpen + 'static>) -> Self {
        Self {
            _arena: arena,
            file: NonNull::from(file),
            _not_sync: PhantomData,
        }
    }

    fn get_file(&mut self) -> &mut dyn FileOpen {
        // The pointer targets a file allocated in `_arena`; the Arc keeps the
        // arena alive for at least as long as this handle.
        unsafe { self.file.as_mut().as_mut() }
    }
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

        // Secondly, recursively open the nested file
        let arena = Box::new(Arena::new());
        // The returned `NestedFile` owns an Arc to this arena, so arena-backed
        // readers remain valid after crossing the `spawn_blocking` boundary.
        let arena_ref: &'static Arena<ArenaSlot<'static>> = unsafe { &*(&*arena as *const _) };
        let nests = subpath.iter().flat_map(|s| s.split("/:/"));
        let mut current_mime = mime_guess::from_path(tgt).first_or_octet_stream();
        let file: Box<dyn FileOpen + 'static> = Box::new(file);
        let mut current_file = file.into_arena(arena_ref);
        let mut current_file_size = metadata.size();

        for nest in nests {
            if current_mime != "application/zip" {
                return Ok(FileOpenResult::InvalidArchive);
            }

            let Ok(archive) = zip::ZipArchive::new(current_file) else {
                return Ok(FileOpenResult::InvalidArchive);
            };
            let archive = archive.into_arena(arena_ref);
            let Some(index) = archive.index_for_path(nest) else {
                return Ok(FileOpenResult::NotAFile);
            };
            let Ok(file) = archive.by_index_seek(index) else {
                return Ok(FileOpenResult::NotAFile);
            };
            current_file_size = file.get_metadata().uncompressed_size;
            let file: Box<dyn FileOpen + 'static> = Box::new(file);
            current_mime = mime_guess::from_path(nest).first_or_octet_stream();
            current_file = file.into_arena(arena_ref);
        }

        Ok(FileOpenResult::File {
            mime: current_mime,
            file: NestedFile::new(arena, current_file),
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
                    subpath.as_ref().map(String::as_str),
                    header_etag.as_ref().map(String::as_str),
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
            file.get_file().seek(std::io::SeekFrom::Start(start))?;
            read_len = end - start;
        }
        let body = tokio::task::spawn_blocking(move || {
            let mut file = file;
            let mut result = Vec::with_capacity(read_len as usize);
            file.get_file()
                .take(read_len)
                .read_to_end(&mut result)
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
        return Ok(resp.body(Body::from(body)).unwrap());
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
