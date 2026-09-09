use std::{
    ffi::OsStr,
    io::{Read, Seek},
};

use bag_lib::path::Path as BagPath;
use zip::{ZipArchive, read::ZipFile, result::ZipError};

use crate::Result;

#[auto_enums::enum_derive(Read, Seek)]
pub enum File<'a> {
    Fs(std::fs::File),
    Mem(std::io::Cursor<Vec<u8>>),
    Buf(seekbuf::Seekbuf<&'a mut dyn Read>),
}

impl<'a> File<'a> {
    pub fn into_static(self) -> Result<File<'static>> {
        match self {
            File::Fs(f) => Ok(File::Fs(f)),
            File::Mem(c) => Ok(File::Mem(c)),
            File::Buf(mut z) => {
                z.seek(std::io::SeekFrom::Start(0))?;
                let mut buf = Vec::new();
                std::io::copy(&mut z, &mut buf)?;
                Ok(File::Mem(std::io::Cursor::new(buf)))
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchiveEntry {
    pub name: String,
    pub is_dir: bool,
}

pub enum ArchiveOpen<'a> {
    File {
        siblings: Vec<ArchiveEntry>,
        file: File<'a>,
        size: u64,
    },
    Directory(Vec<ArchiveEntry>),
}

impl<'a> ArchiveOpen<'a> {
    pub fn into_static(self) -> Result<ArchiveOpen<'static>> {
        match self {
            ArchiveOpen::File {
                siblings,
                file,
                size,
            } => Ok(ArchiveOpen::File {
                siblings,
                file: file.into_static()?,
                size,
            }),
            ArchiveOpen::Directory(listing) => Ok(ArchiveOpen::Directory(listing)),
        }
    }
}

trait ArchiveList {
    /**
     * Prefix is a subpath not started by '/'
     */
    fn archive_list<'s, 'a: 's>(
        &'a mut self,
        prefix: &'s str,
    ) -> impl Iterator<Item = Result<ArchiveEntry>> + 's;
}

fn decode_zip_name<'a>(file: &'a zip::read::ZipFile<'a, impl Read>) -> &'a str {
    std::str::from_utf8(file.name_raw()).unwrap_or_else(|_| file.name())
}

// TODO: why does this not using decode_zip_name
fn find_zip_entry_decoded<R: Read + Seek>(archive: &ZipArchive<R>, path: &str) -> Option<usize> {
    archive
        .index_for_path(path)
        .or_else(|| (0..archive.len()).find(|&index| archive.name_for_index(index) == Some(path)))
}

impl<R: Read + Seek> ArchiveList for ZipArchive<R> {
    fn archive_list<'s, 'a: 's>(
        &'a mut self,
        prefix: &'s str,
    ) -> impl Iterator<Item = Result<ArchiveEntry>> + 's {
        // TODO: file and directory can have colliding name
        // Figure out how to handle that

        // Register all generated direct child
        let mut subdirs = std::collections::HashSet::new();

        (0..self.len()).filter_map(move |idx| -> Option<Result<ArchiveEntry>> {
            macro_rules! unwrap {
                ($expr:expr) => {
                    match $expr {
                        Ok(value) => value,
                        Err(error) => return Some(Err(error.into())),
                    }
                };
            }
            let ent = unwrap!(self.by_index_raw(idx));
            let name = decode_zip_name(&ent);
            if !name.starts_with(prefix) {
                return None;
            }

            let remainder = &name[prefix.len()..];
            // Check: reject foo on foobar
            if !prefix.is_empty() && !remainder.starts_with('/') {
                return None;
            }

            let remainder = if prefix.is_empty() {
                remainder
            } else {
                &remainder[1..]
            };

            let name;
            let is_dir;

            // See if the remaining path has a '/' in it.
            if remainder.is_empty() {
                // This is the explicit entry for the directory itself, skip it
                return None;
            } else if let Some(next_sep) = remainder.find('/') {
                // Is multi-level, must necessarily be a directory
                name = &remainder[..next_sep]; // next_sep starts from 1.
                is_dir = true;
            } else {
                // Is a file inside this directory
                name = remainder;
                is_dir = ent.is_dir(); // This should always be false
            };

            if is_dir {
                let new = subdirs.insert(name.to_owned());
                if new {
                    Some(Ok(ArchiveEntry {
                        name: name.to_owned(),
                        is_dir: true,
                    }))
                } else {
                    None
                }
            } else {
                // This is a file, immediately emit
                Some(Ok(ArchiveEntry {
                    name: name.to_owned(),
                    is_dir: false,
                }))
            }
        })
    }
}

pub enum ArchiveType {
    Zip,
}

pub trait EndsWithExt {
    fn ends_with_ext(&self, ext: &str) -> bool;
}

impl EndsWithExt for str {
    fn ends_with_ext(&self, ext: &str) -> bool {
        self.ends_with(ext)
    }
}

impl EndsWithExt for OsStr {
    fn ends_with_ext(&self, ext: &str) -> bool {
        // TODO: just compare
        self.to_str().map_or(false, |s| s.ends_with(ext))
    }
}

impl ArchiveType {
    pub fn from_path<S: EndsWithExt + ?Sized>(p: &S) -> Option<Self> {
        if p.ends_with_ext(".zip") {
            // TODO: cases
            Some(Self::Zip)
        } else {
            None
        }
    }

    pub fn from_read<R: Read + Seek>(reader: &mut R) -> std::io::Result<Option<Self>> {
        let cur = reader.stream_position()?;
        // Read at most 8192 bytes
        let mut buf = [0u8; 8192];
        let n = reader.take(8192).read(&mut buf)?;
        let inferred = infer::get(&buf[..n]);
        reader.seek(std::io::SeekFrom::Start(cur))?;
        Ok(inferred.and_then(|i| match i.mime_type() {
            "application/zip" => Some(Self::Zip),
            _ => None,
        }))
    }

    pub fn from_path_and_read<S: EndsWithExt + ?Sized, R: Read + Seek>(
        path: &S,
        reader: &mut R,
    ) -> std::io::Result<Option<Self>> {
        if let Some(ty) = Self::from_path(path) {
            Ok(Some(ty))
        } else {
            Self::from_read(reader)
        }
    }
}

// TODO: move into the archive trait
fn open_subpath<'child, 'parent: 'child>(
    arc: &'child mut ZipArchive<File<'parent>>,
    idx: usize,
    pw: Option<&str>,
    error_suffix: usize,
) -> Result<ZipFile<'child, File<'parent>>> {
    macro_rules! unwrap_pw {
        ($expr:expr) => {
            match $expr {
                Ok(value) => value,
                Err(zip::result::ZipError::UnsupportedArchive(ZipError::PASSWORD_REQUIRED))
                | Err(zip::result::ZipError::InvalidPassword) => {
                    return Err(crate::Error::ArchivePassword(error_suffix));
                }
                Err(error) => return Err(error.into()),
            }
        };
    }
    let file = if let Some(pw) = pw {
        unwrap_pw!(arc.by_index_decrypt(idx, pw.as_bytes()))
    } else {
        unwrap_pw!(arc.by_index(idx))
    };
    Ok(file)
}

impl<'a> File<'a> {
    /**
     * Descend into an archive
     *
     * Always open the file, but the consumer can choose to not read it
     */
    pub fn descend<F, T>(self, ty: ArchiveType, path: &BagPath<'_>, handler: F) -> Result<T>
    where
        F: for<'r> FnOnce(ArchiveOpen<'r>) -> Result<T>,
    {
        self.descend_impl(ty, path, 0, handler)
    }

    fn descend_impl<F, T>(
        self,
        ty: ArchiveType,
        total: &BagPath<'_>,
        offset: usize,
        handler: F,
    ) -> Result<T>
    where
        F: for<'r> FnOnce(ArchiveOpen<'r>) -> Result<T>,
    {
        // Assert that the start of the remaining path is a marker segment
        let remaining = total.suffix(offset).ok_or(crate::Error::InvalidPath)?;
        if remaining.is_empty() || remaining.segments()[0].name() != ":" {
            return Err(crate::Error::InvalidPath);
        }

        let pw = remaining.as_ref()[0].arg("pw");

        // Get the leading segment and the remaining subpath
        let remaining = remaining.next().unwrap_or(BagPath::empty());
        let (leading, subpath) = remaining.split_subpath(":");
        let mut leading_bare_with_slash = leading.to_bare_string();
        leading_bare_with_slash.push('/');
        let leading_bare = &leading_bare_with_slash[..leading_bare_with_slash.len() - 1];

        // Try to open the archive and look inside
        // TODO: refactor: split out a archive trait
        // Right now we only support zip
        assert!(matches!(ty, ArchiveType::Zip));

        enum Lookup {
            ExplicitDir, // Don't care about dir index
            NoEntry,
            File(usize),
        }

        // Needs to descend, move self into arena
        let mut arc = ZipArchive::new(self)?;
        let lookup = if let Some(idx) = find_zip_entry_decoded(&arc, leading_bare) {
            // There is an entry here. Since we do not include a slash at the end, it must be a file
            Lookup::File(idx)
        } else if find_zip_entry_decoded(&arc, &leading_bare_with_slash).is_some() {
            Lookup::ExplicitDir
        } else if leading_bare.is_empty() {
            // Always treat the root as an explicit directory
            Lookup::ExplicitDir
        } else {
            // Not found, may be an omitted directory
            // Check later during listing
            Lookup::NoEntry
        };

        // First, handle the case if there is no more descend
        if subpath.is_none() {
            match lookup {
                Lookup::ExplicitDir | Lookup::NoEntry => {
                    let listing = arc.archive_list(leading_bare).collect::<Result<Vec<_>>>()?;
                    if listing.is_empty() && matches!(lookup, Lookup::NoEntry) {
                        return Err(crate::Error::NotFound);
                    }
                    return handler(ArchiveOpen::Directory(listing));
                }
                Lookup::File(f) => {
                    let parent_path = if let Some((parent, _)) = leading_bare.rsplit_once('/') {
                        parent
                    } else {
                        ""
                    };
                    let siblings = arc.archive_list(parent_path).collect::<Result<Vec<_>>>()?;
                    let mut file = open_subpath(&mut arc, f, pw, total.len() - offset)?;
                    let size = file.size();
                    return handler(ArchiveOpen::File {
                        siblings,
                        file: File::Buf(seekbuf::Seekbuf::new(&mut file)),
                        size,
                    });
                }
            }
        };

        // Secondly, there is a subpath, we need to open the archive
        let leading_offset = offset + 1 + leading.len();
        let Lookup::File(f) = lookup else {
            return Err(crate::Error::NotArchive(total.len() - leading_offset));
        };
        let mut file = open_subpath(&mut arc, f, pw, total.len() - offset)?;
        let mut subarc = File::Buf(seekbuf::Seekbuf::new(&mut file));
        let ty = ArchiveType::from_path_and_read(leading_bare, &mut subarc)?
            .ok_or(crate::Error::NotArchive(total.len() - leading_offset))?;
        subarc.descend_impl(ty, total, leading_offset, handler)
    }
}
