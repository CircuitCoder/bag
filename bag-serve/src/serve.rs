use std::{borrow::Cow, path::PathBuf};

use axum::{
    Json, Router,
    body::Body,
    extract::State,
    http::{HeaderMap, Response, Uri},
    response::IntoResponse,
};
use bag_fs::{
    etag::Etag,
    fs::{ArchiveFetch, ArchiveListing, FsHandler},
    render::{ArchiveRenderParams, archive_parent_path, render_archive},
    thumb::{extract_thumbnail_img, extract_thumbnail_video},
};
use bag_lib::{
    action::Action,
    path::{Path, Segment, SegmentParseError},
    ui::{
        Button, Component, Gallery, GalleryImage, GalleryImageType, Image, Layout, LayoutOrAction,
        Text,
    },
};
use chrono::{DateTime, Utc};
use tower_http::cors::{Any, CorsLayer};

use crate::db::Database;

#[derive(Clone)]
struct AppState {
    db: Database,
    fs: FsHandler,
}

const DEFAULT_PAGE_SIZE: usize = 100;

fn is_archive_path(path: &str) -> bool {
    mime_guess::from_path(path).first_or_octet_stream() == "application/zip"
}

fn append_path_segment<'a>(path: &Path<'a>, name: &str) -> Path<'a> {
    let mut segments = path.segments().to_vec();
    segments.push(Segment(Cow::Owned(name.to_owned()), Default::default()));
    Path(Cow::Owned(segments))
}

fn archive_redirect(path: &Path<'_>) -> LayoutOrAction {
    LayoutOrAction::Action(Action::Navigate {
        to: append_path_segment(path, ":").to_string(),
    })
}

fn encoded_relative_path(path: &str) -> String {
    path.split('/')
        .map(|segment| urlencoding::encode(segment).into_owned())
        .collect::<Vec<_>>()
        .join("/")
}

fn thumbnail_resource(path: Path<'_>) -> Option<String> {
    let path = path.next()?;
    let serialized = path
        .segments()
        .iter()
        .map(|segment| {
            if segment.name() == ":" {
                segment.to_string()
            } else {
                urlencoding::encode(segment.name()).into_owned()
            }
        })
        .collect::<Vec<_>>()
        .join("/");
    Some(format!("thumbnail/{serialized}"))
}

fn fs_error_response(error: bag_fs::Error) -> Response<Body> {
    let (status, message) = match &error {
        bag_fs::Error::ArchivePassword(_) => (
            axum::http::StatusCode::UNAUTHORIZED,
            "Archive password is missing or incorrect",
        ),
        bag_fs::Error::NotArchive(_) => (
            axum::http::StatusCode::BAD_REQUEST,
            "Path does not reference a supported archive",
        ),
        bag_fs::Error::NotFound => (axum::http::StatusCode::NOT_FOUND, "Not Found"),
        bag_fs::Error::IoError(error) if error.kind() == std::io::ErrorKind::NotFound => {
            (axum::http::StatusCode::NOT_FOUND, "Not Found")
        }
        bag_fs::Error::IoError(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
            (axum::http::StatusCode::FORBIDDEN, "Permission Denied")
        }
        bag_fs::Error::IoError(error) if error.kind() == std::io::ErrorKind::IsADirectory => {
            (axum::http::StatusCode::BAD_REQUEST, "Reading a directory")
        }
        bag_fs::Error::RangeUnsatisfiable => (
            axum::http::StatusCode::RANGE_NOT_SATISFIABLE,
            "Range Unsatisfiable",
        ),
        bag_fs::Error::Zip(_) => (axum::http::StatusCode::BAD_REQUEST, "Invalid archive"),
        _ => {
            tracing::error!("Failed to read file: {error}");
            (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                "Internal Server Error",
            )
        }
    };
    (status, message).into_response()
}

fn archive_file_layout(
    path: &Path<'_>,
    parent: ArchiveListing,
    metadata: Vec<Component>,
) -> Layout {
    let current_name = path.last().map(|segment| segment.name());
    let files = parent
        .entries
        .iter()
        .filter(|entry| !entry.is_directory && !is_archive_path(&entry.name))
        .collect::<Vec<_>>();
    let current = current_name.and_then(|name| files.iter().position(|entry| entry.name == name));
    let parent_path = path.parent();
    let sibling_path = |name: &str| {
        parent_path
            .as_ref()
            .map(|parent| append_path_segment(parent, name).to_string())
    };

    Layout {
        top: vec![],
        main: vec![Component::Image(Image {
            resource: path.to_string(),
            mime: None,
        })],
        metadata,
        left: current
            .and_then(|index| index.checked_sub(1))
            .and_then(|index| sibling_path(&files[index].name)),
        right: current
            .and_then(|index| files.get(index + 1))
            .and_then(|entry| sibling_path(&entry.name)),
    }
}

pub async fn render_file(
    db: &Database,
    fs: &FsHandler,
    path: Path<'_>,
) -> anyhow::Result<Option<LayoutOrAction>> {
    let serialized = path.to_string();
    let fs_path = path.next();
    let bare = fs_path
        .as_ref()
        .map(Path::to_bare_string)
        .unwrap_or_default();
    let archive_marker = fs_path.as_ref().and_then(|path| {
        path.segments()
            .iter()
            .position(|segment| segment.name() == ":")
    });
    let outer = fs_path.as_ref().map_or_else(String::new, |path| {
        path.segments()[..archive_marker.unwrap_or(path.segments().len())]
            .iter()
            .map(|segment| segment.name())
            .collect::<Vec<_>>()
            .join("/")
    });
    let file = sqlx::query!(
        r#"
        SELECT
          id AS "id!",
          mtime as "mtime: DateTime<Utc>",
          is_directory,
          parent
        FROM files WHERE path = ?
    "#,
        outer
    )
    .fetch_optional(db.as_ref())
    .await?;

    let Some(file) = file else { return Ok(None) };
    if archive_marker.is_none() && !file.is_directory && is_archive_path(&outer) {
        return Ok(Some(archive_redirect(&path)));
    }

    let name = match path.last() {
        None => "Root",
        Some(_) if path.segments().len() == 1 => "Root",
        Some(segment) if segment.name() == ":" => path
            .segments()
            .iter()
            .rev()
            .nth(1)
            .map_or(":", Segment::name),
        Some(segment) => segment.name(),
    };

    let mut metadata = vec![Component::Text(Text {
        content: name.to_owned(),
        variant: bag_lib::ui::TextVariant::Title,
        action: None,
    })];
    let parent = if archive_marker.is_some() {
        archive_parent_path(&path).map(|parent| parent.to_string())
    } else {
        path.parent().map(|parent| parent.to_string())
    };
    if let Some(parent) = parent {
        metadata.push(Component::Text(Text {
            content: "Path".to_owned(),
            variant: bag_lib::ui::TextVariant::Hint,
            action: None,
        }));
        metadata.push(Component::Text(Text {
            content: bare.clone(),
            variant: bag_lib::ui::TextVariant::Body,
            action: None,
        }));
        metadata.push(Component::Button(Button {
            text: "Go up".to_owned(),
            icon: Some("arrow_back".to_owned()),
            action: Action::Navigate { to: parent },
        }));
    }

    if archive_marker.is_some() {
        let Some(fs_path) = fs_path else {
            return Ok(None);
        };
        let thumbnail = thumbnail_resource;
        match fs.archive_fetch(&fs_path).await {
            Ok(ArchiveFetch::Directory(listing)) => {
                let mut layout = render_archive(
                    ArchiveRenderParams {
                        path,
                        default_page_size: DEFAULT_PAGE_SIZE,
                        thumbnail,
                    },
                    listing,
                );
                layout.metadata = metadata;
                return Ok(Some(LayoutOrAction::Layout(layout)));
            }
            Ok(ArchiveFetch::File { parent }) => {
                if path
                    .last()
                    .is_some_and(|segment| is_archive_path(segment.name()))
                {
                    return Ok(Some(archive_redirect(&path)));
                }
                return Ok(Some(LayoutOrAction::Layout(archive_file_layout(
                    &path, parent, metadata,
                ))));
            }
            Err(bag_fs::Error::NotFound) => return Ok(None),
            Err(error) => return Err(error.into()),
        }
    }

    // Fetch self
    let result = if file.is_directory {
        // TODO: filter
        // TODO: sort

        let limit = path
            .last()
            .unwrap()
            .arg("limit")
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(DEFAULT_PAGE_SIZE) as i64;
        let offset = path
            .last()
            .unwrap()
            .arg("offset")
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(0) as i64;

        let children = sqlx::query!(
            r#"
            SELECT path, is_directory, id FROM files WHERE parent = ?
            ORDER BY
                is_directory DESC,
                CASE WHEN substr(lower(path), -4) = '.zip' THEN 1 ELSE 0 END DESC,
                mtime DESC,
                path DESC
            LIMIT ? + 1 OFFSET ?
        "#,
            file.id,
            limit,
            offset
        )
        .fetch_all(db.as_ref())
        .await?;

        let is_start = offset == 0;
        let is_end = children.len() <= limit as usize;
        let children = &children[..(limit as usize).min(children.len())];

        let images = children
            .iter()
            .map(|row| {
                let renders_as_directory = row.is_directory || is_archive_path(&row.path);
                GalleryImage {
                    ty: if renders_as_directory {
                        GalleryImageType::Directory
                    } else {
                        GalleryImageType::File
                    },
                    thumbnail: if renders_as_directory {
                        None
                    } else {
                        Some(format!("thumbnail/{}", encoded_relative_path(&row.path)))
                    },
                    name: row
                        .path
                        .rsplit_once("/")
                        .map(|e| e.1.to_owned())
                        .unwrap_or(row.path.clone()),
                    action: Some(Action::Navigate {
                        to: format!(
                            "{}/{}",
                            serialized,
                            urlencoding::encode(row.path.rsplit('/').next().unwrap())
                        ),
                    }),
                }
            })
            .collect();

        Layout {
            top: vec![],
            main: vec![Component::Gallery(Gallery { images })],
            metadata,
            left: (!is_start).then(|| {
                let offset_str;
                let offset = if offset > limit {
                    offset_str = (offset - limit).to_string();
                    Some(offset_str.as_str())
                } else {
                    None
                };
                path.with_arg("offset", offset).to_string()
            }),
            right: (!is_end).then(|| {
                let offset_str = (offset + limit).to_string();
                path.with_arg("offset", Some(&offset_str)).to_string()
            }),
        }
    } else {
        // Siblings
        let parent_id = file.parent;
        let parent_path = path
            .parent()
            .map(|e| e.to_string() + "/")
            .unwrap_or_else(|| "".to_owned());
        let prev = sqlx::query!(
            r#"
                SELECT path FROM files
                WHERE parent = ?
                    AND id != ?
                    AND is_directory = FALSE
                    AND substr(lower(path), -4) != '.zip'
                    AND mtime >= ?
                ORDER BY mtime ASC, path ASC
                LIMIT 1
            "#,
            parent_id,
            file.id,
            file.mtime,
        )
        .fetch_optional(db.as_ref())
        .await?;
        let next = sqlx::query!(
            r#"
                SELECT path FROM files
                WHERE parent = ?
                    AND id != ?
                    AND is_directory = FALSE
                    AND substr(lower(path), -4) != '.zip'
                    AND mtime <= ?
                ORDER BY mtime DESC, path DESC
                LIMIT 1
            "#,
            parent_id,
            file.id,
            file.mtime,
        )
        .fetch_optional(db.as_ref())
        .await?;
        Layout {
            top: vec![],
            main: vec![Component::Image(Image {
                resource: format!("file/{}", bare),
                mime: None,
            })],
            metadata,
            left: prev.map(|p| parent_path.clone() + p.path.rsplit("/").next().unwrap()),
            right: next.map(|n| parent_path + n.path.rsplit("/").next().unwrap()),
        }
    };

    Ok(Some(LayoutOrAction::Layout(result)))
}

struct ThumbnailLookup {
    outer: String,
    subpath: String,
    media_path: String,
}

fn thumbnail_lookup(path: &Path<'_>) -> Option<ThumbnailLookup> {
    let segments = path.segments();
    let marker = segments.iter().position(|segment| segment.name() == ":");
    let outer_end = marker.unwrap_or(segments.len());
    let outer = segments[..outer_end]
        .iter()
        .map(|segment| segment.name())
        .collect::<Vec<_>>()
        .join("/");
    if outer.is_empty() {
        return None;
    }

    let subpath = marker.map_or_else(String::new, |marker| {
        segments[marker + 1..]
            .iter()
            .map(|segment| segment.name())
            .collect::<Vec<_>>()
            .join("/")
    });
    let media_path = segments.last()?.name().to_owned();

    Some(ThumbnailLookup {
        outer,
        subpath,
        media_path,
    })
}

async fn thumbnail_response(state: &AppState, path: &str) -> Response<Body> {
    let parsed = match Path::try_from(path) {
        Ok(path) => path,
        Err(error) => {
            return (
                axum::http::StatusCode::BAD_REQUEST,
                format!("Invalid thumbnail path: {error:?}"),
            )
                .into_response();
        }
    };
    let Some(lookup) = thumbnail_lookup(&parsed) else {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            "Invalid thumbnail path".to_owned(),
        )
            .into_response();
    };

    let response: anyhow::Result<Response<Body>> = async {
        let file = sqlx::query!(
            "SELECT id AS \"id!\", path, is_directory FROM files WHERE path = ?",
            lookup.outer,
        )
        .fetch_optional(state.db.as_ref())
        .await?;
        let Some(file) = file else {
            return Ok((axum::http::StatusCode::NOT_FOUND, "Not Found".to_owned()).into_response());
        };
        if (file.is_directory && lookup.subpath.is_empty())
            || parsed.last().is_some_and(|segment| segment.name() == ":")
            || is_archive_path(&lookup.media_path)
        {
            return Ok((
                axum::http::StatusCode::BAD_REQUEST,
                "Directories and archives do not have thumbnails".to_owned(),
            )
                .into_response());
        }

        if parsed
            .segments()
            .iter()
            .any(|segment| segment.name() == ":")
        {
            match state.fs.archive_fetch(&parsed).await {
                Ok(ArchiveFetch::File { .. }) => {}
                Ok(ArchiveFetch::Directory(_)) => {
                    return Ok((
                        axum::http::StatusCode::BAD_REQUEST,
                        "Directories and archives do not have thumbnails".to_owned(),
                    )
                        .into_response());
                }
                Err(error) => return Ok(fs_error_response(error)),
            }
        }

        let existing = sqlx::query!(
            "SELECT thumbnail, mime FROM thumbnails WHERE file_id = ? AND subpath = ?",
            file.id,
            lookup.subpath,
        )
        .fetch_optional(state.db.as_ref())
        .await?;
        let (thumbnail, mime) = if let Some(existing) = existing {
            (existing.thumbnail, existing.mime)
        } else {
            let source = match state.fs.open_buffered(&parsed).await {
                Ok(source) => source,
                Err(error) => {
                    tracing::error!(
                        "Failed to open thumbnail source {} ({}): {}",
                        file.path,
                        lookup.subpath,
                        error
                    );
                    return Ok(fs_error_response(error));
                }
            };
            let is_video = mime_guess::from_path(&lookup.media_path)
                .first_or_octet_stream()
                .type_()
                .as_str()
                == "video";
            let generated = if is_video {
                extract_thumbnail_video(source, 150).await
            } else {
                extract_thumbnail_img(source, 150).await
            };
            let thumbnail = match generated {
                Ok(thumbnail) => thumbnail,
                Err(error) => {
                    tracing::error!(
                        "Failed to extract thumbnail for {} ({}): {}",
                        file.path,
                        lookup.subpath,
                        error
                    );
                    return Ok((
                        axum::http::StatusCode::NOT_FOUND,
                        "Failed to extract thumbnail".to_owned(),
                    )
                        .into_response());
                }
            };

            if let Err(error) = sqlx::query!(
                r#"
                INSERT INTO thumbnails (file_id, subpath, thumbnail, mime)
                VALUES (?, ?, ?, 'image/webp')
                ON CONFLICT(file_id, subpath) DO UPDATE SET
                    thumbnail = excluded.thumbnail,
                    mime = excluded.mime
                "#,
                file.id,
                lookup.subpath,
                thumbnail,
            )
            .execute(state.db.as_ref())
            .await
            {
                tracing::error!(
                    "Failed to update thumbnail for {} ({}): {}",
                    file.id,
                    lookup.subpath,
                    error
                );
            }

            (thumbnail, "image/webp".to_owned())
        };

        Ok(Response::builder()
            .header("Content-Type", mime)
            .header(
                "Cache-Control",
                "max-age=3600, stale-while-revalidate=86400",
            )
            .body(thumbnail.into())
            .unwrap())
    }
    .await;

    match response {
        Ok(response) => response,
        Err(error) => {
            tracing::error!(
                "Failed to handle thumbnail request for {} ({}): {}",
                lookup.outer,
                lookup.subpath,
                error
            );
            (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                "Internal Server Error".to_owned(),
            )
                .into_response()
        }
    }
}

pub fn build(db: Database, root: PathBuf) -> Router {
    let fs = FsHandler::new(root.clone());
    let raw_fs = fs.clone();
    let file_handler = Router::new().fallback(
        async move |state: State<AppState>, uri: Uri, headers: HeaderMap| {
            let path = uri.path().trim_start_matches('/');
            let parsed = match Path::try_from(path) {
                Ok(path) => path,
                Err(error) => {
                    return (
                        axum::http::StatusCode::BAD_REQUEST,
                        format!("Invalid file path: {error:?}"),
                    )
                        .into_response();
                }
            };
            let marker = parsed
                .segments()
                .iter()
                .position(|segment| segment.name() == ":");
            let outer = parsed.segments()[..marker.unwrap_or(parsed.segments().len())]
                .iter()
                .map(Segment::name)
                .collect::<Vec<_>>()
                .join("/");
            // Query fs for the file mtime
            let metadata = sqlx::query!(
                r#"SELECT mtime AS "mtime: DateTime<Utc>", length FROM files WHERE path = ?"#,
                outer
            )
            .fetch_optional(state.db.as_ref())
            .await;

            if marker.is_none()
                && let Some(etag) = headers
                    .get(axum::http::header::IF_NONE_MATCH)
                    .and_then(|e| e.to_str().ok())
                && let Ok(Some(metadata)) = metadata
            {
                let mtime: std::time::SystemTime = metadata.mtime.into();
                let ref_etag = Etag {
                    mtime,
                    length: metadata.length as u64,
                    subpath: None,
                };
                if bag_fs::etag::check_header(&ref_etag.hash_string(), etag) {
                    return Response::builder()
                        .status(axum::http::StatusCode::NOT_MODIFIED)
                        .header("ETag", ref_etag.hash_string())
                        .header("Cache-Control", "max-age=60, stale-while-revalidate=86400")
                        .body(Body::empty())
                        .unwrap();
                }
            }
            // FIXME: get length

            match raw_fs.handle(&parsed, &headers).await {
                Ok(response) => response,
                Err(error) => fs_error_response(error),
            }
        },
    );
    let thumbnail_handler =
        Router::new().fallback(async move |State(state): State<AppState>, uri: Uri| {
            let path = uri.path().trim_start_matches('/');
            thumbnail_response(&state, path).await
        });
    let render_handler = Router::new()
        .fallback(async move |state: State<AppState>, uri: Uri| {
            // Trim prefixing & suffixing "/"
            let path = uri.path().trim_start_matches('/').trim_end_matches('/');
            let parsed = match Path::try_from(path) {
                Ok(p) => p,
                Err(SegmentParseError::InvalidArgument(arg)) => {
                    return (
                        axum::http::StatusCode::BAD_REQUEST,
                        format!("Invalid argument: {}", arg),
                    )
                        .into_response();
                }
                Err(SegmentParseError::DecodeError(value)) => {
                    return (
                        axum::http::StatusCode::BAD_REQUEST,
                        format!("Decode error: {}", value),
                    )
                        .into_response();
                }
            };
            tracing::info!("Render request: {}", parsed.to_bare_string());

            if parsed.first().map(|e| e.name()) == Some("file") {
                match render_file(&state.db, &state.fs, parsed).await {
                    Err(error) => match error.downcast::<bag_fs::Error>() {
                        Ok(error) => fs_error_response(error),
                        Err(error) => {
                            tracing::error!("Failed to render file: {error}");
                            (
                                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                                "Internal Server Error",
                            )
                                .into_response()
                        }
                    },
                    Ok(None) => {
                        (axum::http::StatusCode::NOT_FOUND, "Not found".to_string()).into_response()
                    }
                    Ok(Some(result)) => Json(result).into_response(),
                }
            } else if parsed.first().is_none() {
                Json(Action::Navigate {
                    to: "file".to_owned(),
                })
                .into_response()
            } else {
                (axum::http::StatusCode::NOT_FOUND, "Not found".to_string()).into_response()
            }
        })
        .layer(tower_http::compression::CompressionLayer::new());

    let state = AppState { db, fs };

    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    Router::new()
        .nest("/v1/raw/thumbnail/", thumbnail_handler)
        .nest("/v1/raw/file/", file_handler)
        .nest("/v1/render/", render_handler)
        .with_state(state)
        .layer(cors)
}

#[cfg(test)]
mod tests {
    use super::{
        AppState, archive_redirect, fs_error_response, is_archive_path, thumbnail_lookup,
        thumbnail_resource, thumbnail_response,
    };
    use crate::db::Database;
    use bag_fs::fs::FsHandler;
    use bag_lib::{action::Action, path::Path, ui::LayoutOrAction};
    use std::io::{Cursor, Write};
    use zip::{ZipWriter, write::SimpleFileOptions};

    #[test]
    fn detects_archive_paths() {
        assert!(is_archive_path("gallery.ZIP"));
        assert!(!is_archive_path("gallery.png"));
    }

    #[test]
    fn builds_canonical_archive_redirects() {
        let path = Path::try_from("file/outer.zip/%3A/inner.zip").unwrap();
        let LayoutOrAction::Action(Action::Navigate { to }) = archive_redirect(&path) else {
            panic!("archive redirect was not a navigation action");
        };
        assert_eq!(to, "file/outer.zip/%3A/inner.zip/%3A");
    }

    #[test]
    fn normalizes_full_thumbnail_paths_for_database_lookup() {
        let path = Path::try_from(
            "albums/outer.zip/%3A,pw=outer/inner.zip/%3A,pw=inner/photo.jpg,limit=20",
        )
        .unwrap();
        let lookup = thumbnail_lookup(&path).unwrap();

        assert_eq!(lookup.outer, "albums/outer.zip");
        assert_eq!(lookup.subpath, "inner.zip/:/photo.jpg");
        assert_eq!(lookup.media_path, "photo.jpg");
    }

    #[test]
    fn thumbnail_urls_keep_only_archive_marker_arguments() {
        let path = Path::try_from(
            "file/albums,view=grid/outer.zip/%3A,pw=outer/inner.zip/%3A,pw=inner/photo.jpg,quality=high",
        )
        .unwrap();

        assert_eq!(
            thumbnail_resource(path),
            Some(
                "thumbnail/albums/outer.zip/%3A,pw=outer/inner.zip/%3A,pw=inner/photo.jpg"
                    .to_owned()
            )
        );
    }

    #[test]
    fn maps_archive_password_errors_without_exposing_the_path() {
        let response = fs_error_response(bag_fs::Error::ArchivePassword(
            Path::try_from("secret.zip").unwrap(),
        ));

        assert_eq!(response.status(), axum::http::StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn cached_archive_thumbnails_still_require_the_password() {
        let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
        writer
            .start_file(
                "photo.jpg",
                SimpleFileOptions::default().with_aes_encryption(zip::AesMode::Aes256, "secret"),
            )
            .unwrap();
        writer.write_all(b"encrypted source").unwrap();
        let archive = writer.finish().unwrap().into_inner();

        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("outer.zip"), &archive).unwrap();
        let database_path = directory.path().join("database.sqlite");
        let database = Database::setup(&format!("sqlite:{}", database_path.display()))
            .await
            .unwrap();
        let file_id = sqlx::query(
            r#"
            INSERT INTO files (path, mtime, length, scan_id, is_directory)
            VALUES ('outer.zip', datetime('now'), ?, 1, FALSE)
            "#,
        )
        .bind(archive.len() as i64)
        .execute(database.as_ref())
        .await
        .unwrap()
        .last_insert_rowid();
        sqlx::query(
            r#"
            INSERT INTO thumbnails (file_id, subpath, thumbnail, mime)
            VALUES (?, 'photo.jpg', X'010203', 'image/webp')
            "#,
        )
        .bind(file_id)
        .execute(database.as_ref())
        .await
        .unwrap();
        let state = AppState {
            db: database,
            fs: FsHandler::new(directory.path().to_path_buf()),
        };

        let missing = thumbnail_response(&state, "outer.zip/%3A/photo.jpg").await;
        assert_eq!(missing.status(), axum::http::StatusCode::UNAUTHORIZED);
        let wrong = thumbnail_response(&state, "outer.zip/%3A,pw=wrong/photo.jpg").await;
        assert_eq!(wrong.status(), axum::http::StatusCode::UNAUTHORIZED);
        let correct = thumbnail_response(&state, "outer.zip/%3A,pw=secret/photo.jpg").await;
        assert_eq!(correct.status(), axum::http::StatusCode::OK);
    }
}
