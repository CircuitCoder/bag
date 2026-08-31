use std::{
    io::{Cursor, Read, Write},
    sync::{Arc, Mutex},
};

use axum::{
    body::{Body, to_bytes},
    http::{HeaderMap, StatusCode, header},
    response::Response,
};
use bag_lib::path::Path;
use zip::{ZipWriter, write::SimpleFileOptions};

use bag_fs::{
    Error, Result,
    file::{ArchiveEntry, ArchiveOpen, File},
    serve::{FileThunk, RenderContext, finalize_range, read_range, serve},
};

#[derive(Debug)]
enum Opened {
    Physical {
        contents: Vec<u8>,
        etag: String,
    },
    File {
        contents: Vec<u8>,
        siblings: Vec<ArchiveEntry>,
        size: u64,
        etag: String,
    },
    Directory {
        entries: Vec<ArchiveEntry>,
        etag: String,
    },
}

impl Opened {
    fn etag(&self) -> &str {
        match self {
            Self::Physical { etag, .. }
            | Self::File { etag, .. }
            | Self::Directory { etag, .. } => etag,
        }
    }
}

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

async fn open(root: &std::path::Path, path: &str) -> Result<Opened> {
    let opened = Arc::new(Mutex::new(None));
    let output = Arc::clone(&opened);
    let path = Path::try_from(path).unwrap();

    let response = serve(root, &path, &HeaderMap::new(), async move |context| {
        let opened = realize(context).await?;
        *output.lock().unwrap() = Some(opened);
        Ok(Response::new(Body::empty()))
    })
    .await?;
    drop(response);

    Ok(Arc::try_unwrap(opened)
        .unwrap()
        .into_inner()
        .unwrap()
        .unwrap())
}

async fn realize(context: RenderContext<'_>) -> Result<Opened> {
    let RenderContext { file, etag } = context;
    match file {
        FileThunk::Fs(mut file) => {
            tokio::task::spawn_blocking(move || {
                let mut contents = Vec::new();
                file.read_to_end(&mut contents)?;
                Ok(Opened::Physical { contents, etag })
            })
            .await?
        }
        FileThunk::Nested(file, ty, path) => {
            let path = path.to_static();
            tokio::task::spawn_blocking(move || {
                File::Fs(file).descend(ty, &path, move |opened| match opened {
                    ArchiveOpen::File {
                        siblings,
                        mut file,
                        size,
                    } => {
                        let mut contents = Vec::new();
                        file.read_to_end(&mut contents)?;
                        Ok(Opened::File {
                            contents,
                            siblings,
                            size,
                            etag,
                        })
                    }
                    ArchiveOpen::Directory(entries) => Ok(Opened::Directory { entries, etag }),
                })
            })
            .await?
        }
    }
}

fn entry_summary(mut entries: Vec<ArchiveEntry>) -> Vec<(String, bool)> {
    let mut entries = entries
        .drain(..)
        .map(|entry| (entry.name, entry.is_dir))
        .collect::<Vec<_>>();
    entries.sort();
    entries
}

#[tokio::test]
async fn serves_physical_files_and_short_circuits_matching_etags() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("plain.txt"), b"plain contents").unwrap();

    let opened = open(directory.path(), "plain.txt").await.unwrap();
    let Opened::Physical { contents, .. } = &opened else {
        panic!("physical file was not returned as a filesystem file");
    };
    assert_eq!(contents, b"plain contents");

    let mut headers = HeaderMap::new();
    headers.insert(
        header::IF_NONE_MATCH,
        format!("\"{}\"", opened.etag()).parse().unwrap(),
    );
    let path = Path::try_from("plain.txt").unwrap();
    let response = serve(
        directory.path(),
        &path,
        &headers,
        async |_| -> Result<Response> { panic!("matching ETag should bypass the renderer") },
    )
    .await
    .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
}

#[tokio::test]
async fn does_not_descend_when_the_renderer_ignores_the_thunk() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("broken.zip"), b"not a zip").unwrap();
    let path = Path::try_from("broken.zip/%3A").unwrap();

    let response = serve(
        directory.path(),
        &path,
        &HeaderMap::new(),
        async |context| {
            assert!(matches!(context.file, FileThunk::Nested(..)));
            Ok(Response::new(Body::empty()))
        },
    )
    .await
    .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn reads_percent_encoded_utf8_entry_name() {
    let name = "目录/图.jpg";
    let archive = zip_with_file(name, b"utf8 contents");
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("names.zip"), archive).unwrap();
    let path = format!("names.zip/%3A/{}", urlencoding::encode(name));

    let Opened::File { contents, size, .. } = open(directory.path(), &path).await.unwrap() else {
        panic!("archive member was not returned as a file");
    };
    assert_eq!(contents, b"utf8 contents");
    assert_eq!(size, contents.len() as u64);
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
    let path = format!("names.zip/%3A/{}", urlencoding::encode("café.txt"));

    let Opened::File { contents, .. } = open(directory.path(), &path).await.unwrap() else {
        panic!("archive member was not returned as a file");
    };
    assert_eq!(contents, b"cp437 contents");
}

#[tokio::test]
async fn opens_files_directories_and_marker_opened_nested_archives() {
    let nested = zip_with_file("inside.jpg", b"nested");
    let outer = zip_with_files([
        ("root.jpg", b"root".as_slice()),
        ("folder/photo.jpg", b"photo".as_slice()),
        ("folder/deeper/item.png", b"item".as_slice()),
        ("inner.zip", nested.as_slice()),
    ]);
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("outer.zip"), outer).unwrap();

    let Opened::Directory { entries, .. } = open(directory.path(), "outer.zip/%3A").await.unwrap()
    else {
        panic!("archive root was not a directory");
    };
    assert_eq!(
        entry_summary(entries),
        vec![
            ("folder".to_owned(), true),
            ("inner.zip".to_owned(), false),
            ("root.jpg".to_owned(), false),
        ]
    );

    let Opened::Directory { entries, .. } = open(directory.path(), "outer.zip/%3A/folder")
        .await
        .unwrap()
    else {
        panic!("implicit archive folder was not a directory");
    };
    assert_eq!(
        entry_summary(entries),
        vec![("deeper".to_owned(), true), ("photo.jpg".to_owned(), false)]
    );

    let Opened::File { siblings, .. } = open(directory.path(), "outer.zip/%3A/inner.zip")
        .await
        .unwrap()
    else {
        panic!("unmarked nested archive was not returned as a file");
    };
    assert!(
        siblings
            .iter()
            .any(|entry| entry.name == "inner.zip" && !entry.is_dir)
    );

    let Opened::Directory { entries, .. } = open(directory.path(), "outer.zip/%3A/inner.zip/%3A")
        .await
        .unwrap()
    else {
        panic!("marked nested archive was not opened as a directory");
    };
    assert_eq!(
        entry_summary(entries),
        vec![("inside.jpg".to_owned(), false)]
    );

    let Opened::File {
        contents, siblings, ..
    } = open(directory.path(), "outer.zip/%3A/folder/photo.jpg")
        .await
        .unwrap()
    else {
        panic!("ordinary archive member was not returned as a file");
    };
    assert_eq!(contents, b"photo");
    assert!(siblings.iter().any(|entry| entry.name == "photo.jpg"));

    assert!(matches!(
        open(directory.path(), "outer.zip/%3A/missing.jpg").await,
        Err(Error::NotFound)
    ));

    let Opened::File { contents, .. } =
        open(directory.path(), "outer.zip/%3A/inner.zip/%3A/inside.jpg")
            .await
            .unwrap()
    else {
        panic!("nested archive member was not returned as a file");
    };
    assert_eq!(contents, b"nested");
}

#[tokio::test]
async fn uses_the_password_on_each_archive_marker() {
    let nested = encrypted_zip_with_file("inside.jpg", b"nested", "inner password");
    let outer = encrypted_zip_with_file("inner.zip", &nested, "outer password");
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("outer.zip"), outer).unwrap();

    let correct = "outer.zip/%3A,pw=outer%20password/inner.zip/%3A,pw=inner%20password/inside.jpg";
    let Opened::File { contents, .. } = open(directory.path(), correct).await.unwrap() else {
        panic!("encrypted nested member was not returned as a file");
    };
    assert_eq!(contents, b"nested");

    assert!(matches!(
        open(directory.path(), "outer.zip/%3A/inner.zip").await,
        Err(Error::ArchivePassword(2))
    ));
    assert!(matches!(
        open(
            directory.path(),
            "outer.zip/%3A,pw=wrong/inner.zip/%3A/inside.jpg"
        )
        .await,
        Err(Error::ArchivePassword(4))
    ));
    assert!(matches!(
        open(
            directory.path(),
            "outer.zip/%3A,pw=outer%20password/inner.zip/%3A,pw=wrong/inside.jpg"
        )
        .await,
        Err(Error::ArchivePassword(2))
    ));

    assert!(matches!(
        open(
            directory.path(),
            "outer.zip/%3A,pw=outer%20password/inner.zip/%3A,pw=wrong"
        )
        .await,
        Ok(Opened::Directory { .. })
    ));
}

#[tokio::test]
async fn distinguishes_unsupported_archives_from_invalid_zip_files() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("plain.txt"), b"plain").unwrap();
    std::fs::write(directory.path().join("broken.zip"), b"not a zip").unwrap();
    std::fs::create_dir(directory.path().join("folder")).unwrap();

    for path in ["plain.txt/%3A", "folder/%3A"] {
        assert!(matches!(
            open(directory.path(), path).await,
            Err(Error::NotArchive(1))
        ));
    }
    assert!(matches!(
        open(directory.path(), "broken.zip/%3A").await,
        Err(Error::Zip(zip::result::ZipError::InvalidArchive(_)))
    ));
}

#[tokio::test]
async fn builds_partial_responses_with_safely_encoded_filenames() {
    let mut headers = HeaderMap::new();
    headers.insert(header::RANGE, "bytes=1-3".parse().unwrap());
    let (body, range) = read_range(Cursor::new(b"hello".to_vec()), &headers, "etag", 5).unwrap();
    assert_eq!(body, b"ell");

    let filename = "résumé \"draft\"\\\r\n.jpg";
    let response = finalize_range(
        body,
        range,
        mime_guess::mime::IMAGE_JPEG,
        filename,
        "etag",
        5,
    )
    .await
    .unwrap();

    assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(response.headers()[header::CONTENT_RANGE], "bytes 1-3/5");
    assert_eq!(response.headers()[header::CONTENT_LENGTH], "3");
    assert_eq!(
        response.headers()[header::CONTENT_DISPOSITION],
        "inline; filename=\"r_sum_ \\\"draft\\\"\\\\__.jpg\"; filename*=UTF-8''r%C3%A9sum%C3%A9%20%22draft%22%5C%0D%0A.jpg"
    );
    assert_eq!(
        to_bytes(response.into_body(), usize::MAX).await.unwrap(),
        b"ell".as_slice()
    );
}
