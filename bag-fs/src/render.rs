use std::{borrow::Cow, collections::HashMap};

use bag_lib::{
    action::Action,
    path::{Path, Segment},
    ui::{Component, Gallery, GalleryImage, GalleryImageType, Image, Layout},
};

use crate::{
    Error, Result,
    fs::{ArchiveListing, FsHandler},
};

pub struct ArchiveRenderParams<'a, Thumbnail> {
    /// The full render path. It is the source of archive traversal and resource paths.
    pub path: Path<'a>,
    /// The page size used when the last segment does not provide a `limit` argument.
    pub default_page_size: usize,
    /// Builds a thumbnail resource from a file entry's full, normalized render path.
    ///
    /// `limit` and `offset` are removed; all other segment arguments are preserved.
    pub thumbnail: Thumbnail,
}

fn path_without_pagination<'a>(path: &Path<'a>) -> Path<'a> {
    let mut segments = path.segments().to_vec();
    if let Some(last) = segments.last_mut() {
        last.1.remove("offset");
        last.1.remove("limit");
    }
    Path(Cow::Owned(segments))
}

pub fn archive_parent_path<'a>(path: &Path<'a>) -> Option<Path<'a>> {
    let mut parent_len = path.segments().len().checked_sub(1)?;
    let parent_last = parent_len.checked_sub(1)?;
    let segment = &path.segments()[parent_last];
    if segment.name() == ":" && segment.1.is_empty() {
        parent_len = parent_last;
    }
    (parent_len > 0).then(|| Path(Cow::Owned(path.segments()[..parent_len].to_vec())))
}

fn archive_child_path<'a>(path: &Path<'a>, path_is_archive: bool, name: &str) -> Path<'a> {
    let mut path = path_without_pagination(path);
    let segments = path.0.to_mut();
    if path_is_archive && !segments.last().is_some_and(|segment| segment.name() == ":") {
        segments.push(Segment(Cow::Borrowed(":"), HashMap::new()));
    }
    segments.push(Segment(Cow::Owned(name.to_owned()), HashMap::new()));
    path
}

fn archive_lookup_path(path: &Path<'_>) -> Result<String> {
    path.next()
        .map(|path| path.to_bare_string())
        .ok_or(Error::NotFound)
}

fn archive_directory_layout<Thumbnail>(
    path: &Path<'_>,
    listing: ArchiveListing,
    default_page_size: usize,
    thumbnail: &Thumbnail,
) -> Layout
where
    Thumbnail: Fn(Path<'_>) -> Option<String>,
{
    let limit = path
        .last()
        .and_then(|segment| segment.arg("limit"))
        .and_then(|limit| limit.parse::<usize>().ok())
        .unwrap_or(default_page_size);
    let offset = path
        .last()
        .and_then(|segment| segment.arg("offset"))
        .and_then(|offset| offset.parse::<usize>().ok())
        .unwrap_or(0);
    let is_start = offset == 0;
    let is_end = offset.saturating_add(limit) >= listing.entries.len();
    let images = listing
        .entries
        .into_iter()
        .skip(offset)
        .take(limit)
        .map(|entry| {
            let child = archive_child_path(path, listing.is_archive, &entry.name);
            let action_path = child.to_string();
            GalleryImage {
                ty: if entry.is_directory {
                    GalleryImageType::Directory
                } else {
                    GalleryImageType::File
                },
                thumbnail: if entry.is_directory {
                    None
                } else {
                    thumbnail(child)
                },
                action: Some(Action::Navigate { to: action_path }),
                name: entry.name,
            }
        })
        .collect();

    Layout {
        top: vec![],
        main: vec![Component::Gallery(Gallery { images })],
        metadata: vec![],
        left: (!is_start).then(|| {
            let previous = offset.saturating_sub(limit).to_string();
            path.with_arg("offset", Some(&previous)).to_string()
        }),
        right: (!is_end).then(|| {
            let next = offset.saturating_add(limit).to_string();
            path.with_arg("offset", Some(&next)).to_string()
        }),
    }
}

async fn archive_file_layout(fs: &FsHandler, path: &Path<'_>) -> Result<Layout> {
    let parent = archive_parent_path(path).ok_or(Error::NotFound)?;
    let parent_lookup = archive_lookup_path(&parent)?;
    let listing = fs.list_archive(&parent_lookup).await?;
    let current_name = path.last().ok_or(Error::NotFound)?.name();
    let files = listing
        .entries
        .iter()
        .filter(|entry| !entry.is_directory)
        .collect::<Vec<_>>();
    let current = files.iter().position(|entry| entry.name == current_name);
    let sibling_path =
        |name: &str| archive_child_path(&parent, listing.is_archive, name).to_string();

    Ok(Layout {
        top: vec![],
        main: vec![Component::Image(Image {
            resource: path_without_pagination(path).to_string(),
            mime: None,
        })],
        metadata: vec![],
        left: current
            .and_then(|index| index.checked_sub(1))
            .map(|index| sibling_path(&files[index].name)),
        right: current
            .and_then(|index| files.get(index + 1))
            .map(|entry| sibling_path(&entry.name)),
    })
}

impl FsHandler {
    /// Renders an archive, nested archive, archive directory, or archive file.
    pub async fn render_archive<Thumbnail>(
        &self,
        params: ArchiveRenderParams<'_, Thumbnail>,
    ) -> Result<Layout>
    where
        Thumbnail: Fn(Path<'_>) -> Option<String> + Send + Sync,
    {
        let lookup = archive_lookup_path(&params.path)?;
        match self.list_archive(&lookup).await {
            Ok(listing) => Ok(archive_directory_layout(
                &params.path,
                listing,
                params.default_page_size,
                &params.thumbnail,
            )),
            Err(Error::NotFound) if self.archive_file_exists(&lookup).await? => {
                archive_file_layout(self, &params.path).await
            }
            Err(error) => Err(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ArchiveRenderParams, archive_child_path, archive_parent_path, path_without_pagination,
    };
    use crate::fs::FsHandler;
    use bag_lib::path::Path;
    use bag_lib::ui::Component;
    use std::io::{Cursor, Write};
    use zip::{ZipWriter, write::SimpleFileOptions};

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

    #[test]
    fn preserves_archive_arguments_while_building_paths() {
        let path =
            Path::try_from("file/gallery.zip/%3A,password=secret,limit=20,offset=40").unwrap();
        let child = archive_child_path(&path, true, "photo one.png");

        assert_eq!(
            child
                .segments()
                .iter()
                .map(|segment| segment.name())
                .collect::<Vec<_>>(),
            vec!["file", "gallery.zip", ":", "photo one.png"]
        );
        assert_eq!(child.segments()[2].arg("password"), Some("secret"));
        assert_eq!(child.segments()[2].arg("limit"), None);
        assert_eq!(child.segments()[2].arg("offset"), None);

        let normalized = path_without_pagination(&path);
        assert_eq!(normalized.segments()[2].arg("password"), Some("secret"));
        assert_eq!(archive_parent_path(&child), Some(normalized));
    }

    #[test]
    fn removes_an_argument_free_archive_marker_from_parent_paths() {
        let path = Path::try_from("file/gallery.zip/%3A/photo.png").unwrap();
        assert_eq!(
            archive_parent_path(&path).unwrap().to_string(),
            "file/gallery.zip"
        );
    }

    #[tokio::test]
    async fn renders_archive_listings_and_files_from_structured_paths() {
        let directory = tempfile::tempdir().unwrap();
        let archive = zip_with_files([("a.jpg", b"a".as_slice()), ("b.jpg", b"b".as_slice())]);
        std::fs::write(directory.path().join("gallery.zip"), archive).unwrap();
        let fs = FsHandler::new(directory.path().to_path_buf());

        let listing = fs
            .render_archive(ArchiveRenderParams {
                path: Path::try_from("file/gallery.zip,limit=1").unwrap(),
                default_page_size: 100,
                thumbnail: |path: Path<'_>| Some(format!("thumb/{}", path.to_string())),
            })
            .await
            .unwrap();
        assert!(listing.metadata.is_empty());
        assert_eq!(listing.left, None);
        let right = Path::try_from(listing.right.as_deref().unwrap()).unwrap();
        assert_eq!(right.last().unwrap().name(), "gallery.zip");
        assert_eq!(right.last().unwrap().arg("limit"), Some("1"));
        assert_eq!(right.last().unwrap().arg("offset"), Some("1"));
        let Component::Gallery(gallery) = &listing.main[0] else {
            panic!("archive root did not render as a gallery");
        };
        assert_eq!(gallery.images.len(), 1);
        assert_eq!(gallery.images[0].name, "a.jpg");
        assert_eq!(
            gallery.images[0].thumbnail.as_deref(),
            Some("thumb/file/gallery.zip/%3A/a.jpg")
        );

        let file = fs
            .render_archive(ArchiveRenderParams {
                path: Path::try_from("file/gallery.zip/%3A/b.jpg").unwrap(),
                default_page_size: 100,
                thumbnail: |_: Path<'_>| None,
            })
            .await
            .unwrap();
        let Component::Image(image) = &file.main[0] else {
            panic!("archive member did not render as an image");
        };
        assert_eq!(image.resource, "file/gallery.zip/%3A/b.jpg");
        assert_eq!(file.left.as_deref(), Some("file/gallery.zip/%3A/a.jpg"));
        assert_eq!(file.right, None);
    }
}
