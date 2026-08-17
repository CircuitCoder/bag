use std::borrow::Cow;

use bag_lib::{
    action::Action,
    path::{Path, Segment},
    ui::{Component, Gallery, GalleryImage, GalleryImageType, Layout},
};

use crate::fs::ArchiveListing;

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
    if path.last().is_some_and(|segment| segment.name() == ":") {
        parent_len = parent_len.checked_sub(1)?;
    }
    (parent_len > 0).then(|| Path(Cow::Owned(path.segments()[..parent_len].to_vec())))
}

fn archive_child_path<'a>(path: &Path<'a>, name: &str) -> Path<'a> {
    let mut path = path_without_pagination(path);
    path.0
        .to_mut()
        .push(Segment(Cow::Owned(name.to_owned()), Default::default()));
    path
}

fn is_archive_path(path: &str) -> bool {
    mime_guess::from_path(path).first_or_octet_stream() == "application/zip"
}

pub fn render_archive<Thumbnail>(
    params: ArchiveRenderParams<'_, Thumbnail>,
    listing: ArchiveListing,
) -> Layout
where
    Thumbnail: Fn(Path<'_>) -> Option<String> + Send + Sync,
{
    let path = &params.path;
    let limit = path
        .last()
        .and_then(|segment| segment.arg("limit"))
        .and_then(|limit| limit.parse::<usize>().ok())
        .unwrap_or(params.default_page_size);
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
            let renders_as_directory = entry.is_directory || is_archive_path(&entry.name);
            let child = archive_child_path(path, &entry.name);
            let action_path = child.to_string();
            GalleryImage {
                ty: if renders_as_directory {
                    GalleryImageType::Directory
                } else {
                    GalleryImageType::File
                },
                thumbnail: if renders_as_directory {
                    None
                } else {
                    (params.thumbnail)(child)
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

#[cfg(test)]
mod tests {
    use super::{
        ArchiveRenderParams, archive_child_path, archive_parent_path, path_without_pagination,
        render_archive,
    };
    use crate::fs::{ArchiveEntry, ArchiveListing};
    use bag_lib::path::Path;
    use bag_lib::ui::{Component, GalleryImageType};

    #[test]
    fn preserves_archive_arguments_while_building_paths() {
        let path = Path::try_from("file/gallery.zip/%3A,pw=secret,limit=20,offset=40").unwrap();
        let child = archive_child_path(&path, "photo one.png");

        assert_eq!(
            child
                .segments()
                .iter()
                .map(|segment| segment.name())
                .collect::<Vec<_>>(),
            vec!["file", "gallery.zip", ":", "photo one.png"]
        );
        assert_eq!(child.segments()[2].arg("pw"), Some("secret"));
        assert_eq!(child.segments()[2].arg("limit"), None);
        assert_eq!(child.segments()[2].arg("offset"), None);

        let normalized = path_without_pagination(&path);
        assert_eq!(normalized.segments()[2].arg("pw"), Some("secret"));
        assert_eq!(archive_parent_path(&child), Some(normalized));
    }

    #[test]
    fn keeps_archive_markers_in_file_parent_paths() {
        let path = Path::try_from("file/gallery.zip/%3A/photo.png").unwrap();
        assert_eq!(
            archive_parent_path(&path).unwrap().to_string(),
            "file/gallery.zip/%3A"
        );

        let root = Path::try_from("file/gallery.zip/%3A,pw=secret").unwrap();
        assert_eq!(archive_parent_path(&root).unwrap().to_string(), "file");
    }

    #[test]
    fn renders_archive_listing_and_classifies_nested_archives_by_mime() {
        let layout = render_archive(
            ArchiveRenderParams {
                path: Path::try_from("file/gallery.zip/%3A,pw=secret,limit=2,offset=0").unwrap(),
                default_page_size: 100,
                thumbnail: |path: Path<'_>| Some(format!("thumb/{path}")),
            },
            ArchiveListing {
                entries: vec![
                    ArchiveEntry {
                        name: "inner.zip".to_owned(),
                        subpath: "inner.zip".to_owned(),
                        is_directory: false,
                    },
                    ArchiveEntry {
                        name: "photo one.jpg".to_owned(),
                        subpath: "photo one.jpg".to_owned(),
                        is_directory: false,
                    },
                ],
            },
        );
        assert!(layout.metadata.is_empty());
        assert_eq!(layout.left, None);
        assert_eq!(layout.right, None);
        let Component::Gallery(gallery) = &layout.main[0] else {
            panic!("archive root did not render as a gallery");
        };
        assert_eq!(gallery.images.len(), 2);
        assert!(matches!(&gallery.images[0].ty, GalleryImageType::Directory));
        assert_eq!(gallery.images[0].thumbnail, None);
        assert_eq!(gallery.images[0].name, "inner.zip");
        assert!(matches!(&gallery.images[1].ty, GalleryImageType::File));
        assert_eq!(
            gallery.images[1].thumbnail.as_deref(),
            Some("thumb/file/gallery.zip/%3A,pw=secret/photo%20one.jpg")
        );
    }
}
