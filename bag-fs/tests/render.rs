use bag_fs::{
    file::ArchiveEntry,
    render::{ArchiveRenderParams, archive_parent_path, render_archive},
};
use bag_lib::{
    path::Path,
    ui::{Component, GalleryImageType, InputType, Text, TextVariant},
};

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
    let metadata = vec![Component::Text(Text {
        content: "Gallery".to_owned(),
        variant: TextVariant::Title,
        action: None,
    })];
    let layout = render_archive(
        ArchiveRenderParams {
            path: Path::try_from("file/gallery.zip/%3A,pw=secret,limit=2,offset=0").unwrap(),
            default_page_size: 100,
            thumbnail: |path: Path<'_>| Some(format!("thumb/{path}")),
        },
        vec![
            ArchiveEntry {
                name: "inner.zip".to_owned(),
                is_dir: false,
            },
            ArchiveEntry {
                name: "photo one.jpg".to_owned(),
                is_dir: false,
            },
        ],
        metadata,
    );
    assert_eq!(layout.metadata.len(), 1);
    let Component::Text(rendered_metadata) = &layout.metadata[0] else {
        panic!("archive metadata was not preserved");
    };
    assert_eq!(rendered_metadata.content, "Gallery");
    assert_eq!(layout.left, None);
    assert_eq!(layout.right, None);
    let Component::Gallery(gallery) = &layout.main[0] else {
        panic!("archive root did not render as a gallery");
    };
    assert_eq!(gallery.images.len(), 2);
    assert!(matches!(&gallery.images[0].ty, GalleryImageType::Archive));
    assert_eq!(gallery.images[0].thumbnail, None);
    assert_eq!(gallery.images[0].name, "inner.zip");
    assert!(matches!(&gallery.images[1].ty, GalleryImageType::File));
    assert_eq!(
        gallery.images[1].thumbnail.as_deref(),
        Some("thumb/file/gallery.zip/%3A,pw=secret/photo%20one.jpg")
    );
}

#[test]
fn renders_a_password_input_for_the_failing_archive_marker() {
    let path = Path::try_from("file/outer.zip/%3A/inner.zip/%3A/photo.jpg").unwrap();
    let metadata = vec![Component::Text(Text {
        content: "photo.jpg".to_owned(),
        variant: TextVariant::Title,
        action: None,
    })];

    let layout = bag_fs::render::render_archive_password(&path, 4, metadata);

    assert_eq!(layout.metadata.len(), 1);
    assert!(layout.left.is_none());
    assert!(layout.right.is_none());
    let [Component::Input(input)] = layout.main.as_slice() else {
        panic!("password error did not render a single input");
    };
    assert!(!input.bidir);
    assert_eq!(input.segment, 4);
    assert_eq!(input.param, "pw");
    assert!(matches!(input.ty, InputType::Password));
    assert_eq!(input.placeholder.as_deref(), Some("Password"));
    assert_eq!(input.button.as_deref(), Some("Open"));
}
