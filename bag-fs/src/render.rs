use std::borrow::Cow;

use bag_lib::{
    action::Action,
    path::{Path, Segment},
    ui::{Component, Gallery, GalleryImage, GalleryImageType, Input, InputType, Layout},
};

use crate::file::ArchiveEntry;

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
    listing: Vec<ArchiveEntry>,
    metadata: Vec<Component>,
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
    let is_end = offset.saturating_add(limit) >= listing.len();
    let images = listing
        .into_iter()
        .skip(offset)
        .take(limit)
        .map(|entry| {
            let ty = if entry.is_dir {
                GalleryImageType::Directory
            } else if is_archive_path(&entry.name) {
                GalleryImageType::Archive
            } else {
                GalleryImageType::File
            };
            let child = archive_child_path(path, &entry.name);
            let action_path = child.to_string();
            GalleryImage {
                thumbnail: if matches!(&ty, GalleryImageType::File) {
                    (params.thumbnail)(child)
                } else {
                    None
                },
                ty,
                action: Some(Action::Navigate { to: action_path }),
                name: entry.name,
            }
        })
        .collect();

    Layout {
        top: vec![],
        main: vec![Component::Gallery(Gallery { images })],
        metadata,
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

pub fn render_archive_file(
    path: &Path<'_>,
    siblings: Vec<ArchiveEntry>,
    metadata: Vec<Component>,
) -> Layout {
    let current_name = path.last().map(|segment| segment.name());
    let current =
        current_name.and_then(|name| siblings.iter().position(|entry| entry.name == name));
    let parent_path = path.parent();
    let sibling_path = |name: &str| {
        parent_path.as_ref().map(|parent| {
            parent
                .append(Segment::new(name.into(), Default::default()))
                .to_string()
        })
    };

    Layout {
        top: vec![],
        main: vec![Component::Image(bag_lib::ui::Image {
            resource: path.to_string(),
            mime: None,
        })],
        metadata,
        left: current
            .and_then(|index| index.checked_sub(1))
            .and_then(|index| sibling_path(&siblings[index].name)),
        right: current
            .and_then(|index| siblings.get(index + 1))
            .and_then(|entry| sibling_path(&entry.name)),
    }
}

pub fn render_archive_password(
    _path: &Path<'_>,
    offset: usize,
    metadata: Vec<Component>,
) -> Layout {
    Layout {
        top: vec![],
        main: vec![Component::Input(Input {
            bidir: false,
            segment: offset,
            param: "pw".to_owned(),
            ty: InputType::Password,
            placeholder: Some("Password".to_owned()),
            button: Some("Open".to_owned()),
        })],
        metadata,
        left: None,
        right: None,
    }
}

#[cfg(test)]
mod tests {
    use super::{archive_child_path, archive_parent_path, path_without_pagination};
    use bag_lib::path::Path;

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
}
