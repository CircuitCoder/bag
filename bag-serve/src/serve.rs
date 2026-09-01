use std::{
    io::{BufReader, Cursor, Read, Seek},
    path::PathBuf,
    sync::Arc,
};

use axum::{
    Json, Router,
    body::Body,
    extract::State,
    http::{HeaderMap, Response, Uri},
    response::IntoResponse,
};
use bag_fs::{
    file::{ArchiveEntry, ArchiveOpen, File},
    render::{
        ArchiveRenderParams, archive_parent_path, render_archive, render_archive_file,
        render_archive_password,
    },
    serve::{FileThunk, RenderContext, finalize_range, read_range},
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
    base: PathBuf,
    thumbnail_queue: ThumbnailQueue,
}

#[derive(Clone)]
struct ThumbnailQueue {
    semaphore: Option<Arc<tokio::sync::Semaphore>>,
}

impl ThumbnailQueue {
    fn new(concurrency: usize) -> Self {
        Self {
            semaphore: (concurrency != 0)
                .then(|| Arc::new(tokio::sync::Semaphore::new(concurrency))),
        }
    }

    async fn acquire(&self) -> Option<tokio::sync::OwnedSemaphorePermit> {
        match &self.semaphore {
            Some(semaphore) => Some(
                semaphore
                    .clone()
                    .acquire_owned()
                    .await
                    .expect("thumbnail queue semaphore is never closed"),
            ),
            None => None,
        }
    }
}

const DEFAULT_PAGE_SIZE: usize = 100;

trait ReadSeek: Read + Seek {}

impl<T: Read + Seek> ReadSeek for T {}

struct ReadableFile {
    file: Box<dyn ReadSeek + Send>,
    size: u64,
}

enum ArchiveView {
    File(Vec<ArchiveEntry>),
    Directory(Vec<ArchiveEntry>),
}

async fn open_readable(thunk: FileThunk<'_>) -> bag_fs::Result<ReadableFile> {
    match thunk {
        FileThunk::Fs(file) => {
            let size = file.metadata()?.len();
            Ok(ReadableFile {
                file: Box::new(file),
                size,
            })
        }
        FileThunk::Nested(file, ty, path) => {
            let path = path.to_static();
            tokio::task::spawn_blocking(move || {
                File::Fs(file).descend(ty, &path, |opened| match opened {
                    ArchiveOpen::File { mut file, size, .. } => {
                        let mut contents = Vec::new();
                        file.read_to_end(&mut contents)?;
                        Ok(ReadableFile {
                            file: Box::new(Cursor::new(contents)),
                            size,
                        })
                    }
                    ArchiveOpen::Directory(_) => Err(bag_fs::Error::NotFound),
                })
            })
            .await?
        }
    }
}

async fn open_archive_view(thunk: FileThunk<'_>) -> bag_fs::Result<ArchiveView> {
    let FileThunk::Nested(file, ty, path) = thunk else {
        return Err(bag_fs::Error::InvalidPath);
    };
    let path = path.to_static();
    tokio::task::spawn_blocking(move || {
        File::Fs(file).descend(ty, &path, |opened| {
            Ok(match opened {
                ArchiveOpen::File { siblings, .. } => ArchiveView::File(siblings),
                ArchiveOpen::Directory(entries) => ArchiveView::Directory(entries),
            })
        })
    })
    .await?
}

fn is_archive_path(path: &str) -> bool {
    mime_guess::from_path(path).first_or_octet_stream() == "application/zip"
}

fn archive_redirect(path: &Path<'_>) -> LayoutOrAction {
    LayoutOrAction::Action(Action::Redirect {
        to: path.append(":".try_into().unwrap()).to_string(),
    })
}

fn render_response(result: LayoutOrAction) -> Response<Body> {
    Json(result).into_response()
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

pub async fn render_file(
    db: &Database,
    base: &std::path::Path,
    path: Path<'_>,
) -> anyhow::Result<Option<Response<Body>>> {
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
        return Ok(Some(render_response(archive_redirect(&path))));
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
        let path = path.to_static();
        let response = bag_fs::serve::serve(base, &fs_path, &HeaderMap::new(), async move |ctx| {
            let result = match open_archive_view(ctx.file).await {
                Ok(ArchiveView::Directory(listing)) => LayoutOrAction::Layout(render_archive(
                    ArchiveRenderParams {
                        path: path.borrow(),
                        default_page_size: DEFAULT_PAGE_SIZE,
                        thumbnail: thumbnail_resource,
                    },
                    listing,
                    metadata,
                )),
                Ok(ArchiveView::File(siblings)) => {
                    if path
                        .last()
                        .is_some_and(|segment| is_archive_path(segment.name()))
                    {
                        archive_redirect(&path)
                    } else {
                        LayoutOrAction::Layout(render_archive_file(&path, siblings, metadata))
                    }
                }
                Err(bag_fs::Error::ArchivePassword(suffix_len)) => {
                    let offset = path
                        .len()
                        .checked_sub(suffix_len)
                        .ok_or(bag_fs::Error::InvalidPath)?;
                    if path
                        .segments()
                        .get(offset)
                        .is_none_or(|segment| segment.name() != ":")
                    {
                        return Err(bag_fs::Error::InvalidPath);
                    }
                    LayoutOrAction::Layout(render_archive_password(&path, offset, metadata))
                }
                Err(error) => return Err(error),
            };
            Ok(render_response(result))
        })
        .await?;
        return Ok(Some(response));
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
                let ty = if row.is_directory {
                    GalleryImageType::Directory
                } else if is_archive_path(&row.path) {
                    GalleryImageType::Archive
                } else {
                    GalleryImageType::File
                };
                GalleryImage {
                    thumbnail: if matches!(&ty, GalleryImageType::File) {
                        Some(format!("thumbnail/{}", encoded_relative_path(&row.path)))
                    } else {
                        None
                    },
                    ty,
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

    Ok(Some(render_response(LayoutOrAction::Layout(result))))
}

struct ThumbnailLookup {
    outer: String,
    subpath: Option<String>,
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

    let subpath = marker.map(|marker| {
        segments[marker + 1..]
            .iter()
            .map(|segment| segment.name())
            .collect::<Vec<_>>()
            .join("/")
    });
    if subpath.as_deref().is_some_and(str::is_empty) {
        return None;
    }
    let media_path = segments.last()?.name().to_owned();

    Some(ThumbnailLookup {
        outer,
        subpath,
        media_path,
    })
}

async fn cached_thumbnail(
    db: &Database,
    file_id: i64,
    subpath: Option<&str>,
) -> Result<Option<(Vec<u8>, String)>, sqlx::Error> {
    Ok(sqlx::query!(
        "SELECT thumbnail, mime FROM thumbnails WHERE file_id = ? AND subpath IS ?",
        file_id,
        subpath,
    )
    .fetch_optional(db.as_ref())
    .await?
    .map(|thumbnail| (thumbnail.thumbnail, thumbnail.mime)))
}

async fn thumbnail_response(state: &AppState, path: &str, headers: &HeaderMap) -> Response<Body> {
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

    let file = match sqlx::query!(
        "SELECT id AS \"id!\", path, is_directory FROM files WHERE path = ?",
        lookup.outer,
    )
    .fetch_optional(state.db.as_ref())
    .await
    {
        Ok(Some(file)) => file,
        Ok(None) => {
            return (axum::http::StatusCode::NOT_FOUND, "Not Found").into_response();
        }
        Err(error) => {
            tracing::error!(
                "Failed to look up thumbnail source {}: {error}",
                lookup.outer
            );
            return (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                "Internal Server Error",
            )
                .into_response();
        }
    };
    if (file.is_directory && lookup.subpath.is_none())
        || parsed.last().is_some_and(|segment| segment.name() == ":")
        || is_archive_path(&lookup.media_path)
    {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            "Directories and archives do not have thumbnails",
        )
            .into_response();
    }

    let request_headers = headers.clone();
    let db = state.db.clone();
    let file_path = file.path;
    let file_id = file.id;
    let media_path = lookup.media_path.clone();
    let subpath = lookup.subpath.clone();
    let response = bag_fs::serve::serve(
        &state.base,
        &parsed,
        headers,
        async move |RenderContext { file, etag }| {
            // Cache hits intentionally leave the source thunk unopened.
            let cached = match cached_thumbnail(&db, file_id, subpath.as_deref()).await {
                Ok(cached) => cached,
                Err(error) => {
                    tracing::error!(
                        "Failed to read cached thumbnail for {} ({}): {}",
                        file_path,
                        subpath.as_deref().unwrap_or("<whole file>"),
                        error
                    );
                    return Ok((
                        axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                        "Internal Server Error",
                    )
                        .into_response());
                }
            };

            let (thumbnail, mime) = if let Some(cached) = cached {
                cached
            } else {
                let source = open_readable(file).await?;
                let source = BufReader::new(source.file);
                let is_video = mime_guess::from_path(&media_path)
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
                            file_path,
                            subpath.as_deref().unwrap_or("<whole file>"),
                            error
                        );
                        return Ok((
                            axum::http::StatusCode::NOT_FOUND,
                            "Failed to extract thumbnail",
                        )
                            .into_response());
                    }
                };

                if let Err(error) = sqlx::query!(
                    r#"
                    INSERT INTO thumbnails (file_id, subpath, thumbnail, mime)
                    VALUES (?, ?, ?, 'image/webp')
                    ON CONFLICT DO UPDATE SET
                        thumbnail = excluded.thumbnail,
                        mime = excluded.mime
                    "#,
                    file_id,
                    subpath,
                    thumbnail,
                )
                .execute(db.as_ref())
                .await
                {
                    tracing::error!(
                        "Failed to update thumbnail for {} ({}): {}",
                        file_id,
                        subpath.as_deref().unwrap_or("<whole file>"),
                        error
                    );
                }

                (thumbnail, "image/webp".to_owned())
            };

            let size = thumbnail.len() as u64;
            let mime = mime
                .parse::<mime_guess::Mime>()
                .unwrap_or_else(|_| "image/webp".parse().expect("WebP MIME type is valid"));
            let (body, range) = read_range(Cursor::new(thumbnail), &request_headers, &etag, size)?;
            finalize_range(body, range, mime, &media_path, &etag, size).await
        },
    )
    .await;

    match response {
        Ok(response) => response,
        Err(error) => {
            tracing::error!(
                "Failed to handle thumbnail request for {} ({}): {}",
                lookup.outer,
                lookup.subpath.as_deref().unwrap_or("<whole file>"),
                error
            );
            bag_fs::serve::error_to_resp(error)
        }
    }
}

async fn raw_file_response(
    base: &std::path::Path,
    path: &str,
    headers: &HeaderMap,
) -> Response<Body> {
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
    let filename = parsed
        .last()
        .map(|segment| segment.name().to_owned())
        .unwrap_or_default();
    let mime = mime_guess::from_path(&filename).first_or_octet_stream();
    let request_headers = headers.clone();

    match bag_fs::serve::serve(
        base,
        &parsed,
        headers,
        async move |RenderContext { file, etag }| {
            let source = open_readable(file).await?;
            let size = source.size;
            let range_etag = etag.clone();
            let (body, range) = tokio::task::spawn_blocking(move || {
                read_range(source.file, &request_headers, &range_etag, size)
            })
            .await??;
            finalize_range(body, range, mime, &filename, &etag, size).await
        },
    )
    .await
    {
        Ok(response) => response,
        Err(error) => bag_fs::serve::error_to_resp(error),
    }
}

async fn raw_response(
    State(state): State<AppState>,
    uri: Uri,
    headers: HeaderMap,
) -> Response<Body> {
    let path = uri.path().trim_start_matches('/');
    let Some((namespace, path)) = path.split_once('/') else {
        return (axum::http::StatusCode::NOT_FOUND, "Not found").into_response();
    };
    let namespace = match Segment::try_from(namespace) {
        Ok(namespace) => namespace,
        Err(SegmentParseError::InvalidArgument(arg)) => {
            return (
                axum::http::StatusCode::BAD_REQUEST,
                format!("Invalid argument: {arg}"),
            )
                .into_response();
        }
        Err(SegmentParseError::DecodeError(value)) => {
            return (
                axum::http::StatusCode::BAD_REQUEST,
                format!("Decode error: {value}"),
            )
                .into_response();
        }
    };

    match namespace.name() {
        "file" => raw_file_response(&state.base, path, &headers).await,
        "thumbnail" => {
            let _permit = state.thumbnail_queue.acquire().await;
            thumbnail_response(&state, path, &headers).await
        }
        _ => (axum::http::StatusCode::NOT_FOUND, "Not found").into_response(),
    }
}

pub fn build(db: Database, root: PathBuf, thumbnail_concurrency: usize) -> Router {
    let raw_handler = Router::new().fallback(raw_response);
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
                match render_file(&state.db, &state.base, parsed).await {
                    Err(error) => match error.downcast::<bag_fs::Error>() {
                        Ok(error) => bag_fs::serve::error_to_resp(error),
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
                    Ok(Some(response)) => response,
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

    let state = AppState {
        db,
        base: root,
        thumbnail_queue: ThumbnailQueue::new(thumbnail_concurrency),
    };

    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    Router::new()
        .nest("/v1/raw/", raw_handler)
        .nest("/v1/render/", render_handler)
        .with_state(state)
        .layer(cors)
}

#[cfg(test)]
mod tests {
    use super::{
        AppState, ThumbnailQueue, archive_redirect, build, is_archive_path, raw_file_response,
        render_file, thumbnail_lookup, thumbnail_resource, thumbnail_response,
    };
    use crate::db::Database;
    use axum::{
        body::{Body, to_bytes},
        http::{HeaderMap, Request, header},
    };
    use bag_lib::{action::Action, path::Path, ui::LayoutOrAction};
    use image::{DynamicImage, ImageFormat};
    use std::io::{Cursor, Write};
    use tower::ServiceExt;
    use zip::{ZipWriter, write::SimpleFileOptions};

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
        assert_eq!(lookup.subpath.as_deref(), Some("inner.zip/:/photo.jpg"));
        assert_eq!(lookup.media_path, "photo.jpg");

        let physical = thumbnail_lookup(&Path::try_from("albums/photo.jpg").unwrap()).unwrap();
        assert_eq!(physical.subpath, None);
        assert!(thumbnail_lookup(&Path::try_from("albums/archive.zip/%3A").unwrap()).is_none());
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
        let response = bag_fs::serve::error_to_resp(bag_fs::Error::ArchivePassword(1));

        assert_eq!(response.status(), axum::http::StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn raw_files_support_ranges_and_conditional_requests() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("hello.txt"), b"hello").unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(header::RANGE, "bytes=1-3".parse().unwrap());

        let response = raw_file_response(directory.path(), "hello.txt", &headers).await;
        assert_eq!(response.status(), axum::http::StatusCode::PARTIAL_CONTENT);
        assert_eq!(response.headers()[header::CONTENT_RANGE], "bytes 1-3/5");
        let etag = response.headers()[header::ETAG].clone();
        assert_eq!(
            to_bytes(response.into_body(), usize::MAX).await.unwrap(),
            b"ell".as_slice()
        );

        let mut headers = HeaderMap::new();
        headers.insert(header::IF_NONE_MATCH, etag);
        let response = raw_file_response(directory.path(), "hello.txt", &headers).await;
        assert_eq!(response.status(), axum::http::StatusCode::NOT_MODIFIED);
    }

    #[tokio::test]
    async fn raw_router_dispatches_namespaces_with_arguments() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("hello.txt"), b"hello").unwrap();
        let archive = encrypted_zip_with_file("photo.jpg", b"image contents", "secret");
        std::fs::write(directory.path().join("outer.zip"), archive).unwrap();
        let database_path = directory.path().join("database.sqlite");
        let database = Database::setup(&format!("sqlite:{}", database_path.display()))
            .await
            .unwrap();
        let app = build(database, directory.path().to_path_buf(), 1);

        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/v1/raw/file,offset=300/hello.txt")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), axum::http::StatusCode::OK);
        assert_eq!(
            to_bytes(response.into_body(), usize::MAX).await.unwrap(),
            b"hello".as_slice()
        );

        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/v1/raw/file,offset=300/outer.zip/%3A,pw=secret/photo.jpg")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), axum::http::StatusCode::OK);
        assert_eq!(
            to_bytes(response.into_body(), usize::MAX).await.unwrap(),
            b"image contents".as_slice()
        );

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/v1/raw/unknown,offset=300/hello.txt")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), axum::http::StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn renders_archive_directories_and_files_through_the_thunk() {
        let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
        writer
            .start_file("photo.jpg", SimpleFileOptions::default())
            .unwrap();
        writer.write_all(b"image contents").unwrap();
        let archive = writer.finish().unwrap().into_inner();
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("outer.zip"), &archive).unwrap();
        let database_path = directory.path().join("database.sqlite");
        let database = Database::setup(&format!("sqlite:{}", database_path.display()))
            .await
            .unwrap();
        sqlx::query(
            r#"
            INSERT INTO files (path, mtime, length, scan_id, is_directory)
            VALUES ('outer.zip', datetime('now'), ?, 1, FALSE)
            "#,
        )
        .bind(archive.len() as i64)
        .execute(database.as_ref())
        .await
        .unwrap();

        let path = Path::try_from("file/outer.zip/%3A").unwrap();
        let response = render_file(&database, directory.path(), path)
            .await
            .unwrap()
            .unwrap();
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body = std::str::from_utf8(&body).unwrap();
        assert!(body.contains("Gallery"));
        assert!(body.contains("photo.jpg"));

        let path = Path::try_from("file/outer.zip/%3A/photo.jpg").unwrap();
        let response = render_file(&database, directory.path(), path)
            .await
            .unwrap()
            .unwrap();
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert!(std::str::from_utf8(&body).unwrap().contains("Image"));
    }

    #[tokio::test]
    async fn renders_password_inputs_for_the_failing_archive_marker() {
        let nested = encrypted_zip_with_file("photo.jpg", b"image contents", "inner");
        let outer = encrypted_zip_with_file("inner.zip", &nested, "outer");
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("outer.zip"), &outer).unwrap();
        let database_path = directory.path().join("database.sqlite");
        let database = Database::setup(&format!("sqlite:{}", database_path.display()))
            .await
            .unwrap();
        sqlx::query(
            r#"
            INSERT INTO files (path, mtime, length, scan_id, is_directory)
            VALUES ('outer.zip', datetime('now'), ?, 1, FALSE)
            "#,
        )
        .bind(outer.len() as i64)
        .execute(database.as_ref())
        .await
        .unwrap();

        for (path, expected_segment) in [
            ("file/outer.zip/%3A/inner.zip/%3A/photo.jpg", 2),
            ("file/outer.zip/%3A,pw=outer/inner.zip/%3A/photo.jpg", 4),
        ] {
            let response = render_file(&database, directory.path(), Path::try_from(path).unwrap())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(response.status(), axum::http::StatusCode::OK);
            let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
            let rendered: LayoutOrAction = serde_json::from_slice(&body).unwrap();
            let LayoutOrAction::Layout(layout) = rendered else {
                panic!("password error rendered a navigation action");
            };
            assert!(!layout.metadata.is_empty());
            let [bag_lib::ui::Component::Input(input)] = layout.main.as_slice() else {
                panic!("password error did not render a single input");
            };
            assert_eq!(input.segment, expected_segment);
            assert_eq!(input.param, "pw");
            assert!(matches!(input.ty, bag_lib::ui::InputType::Password));
        }
    }

    #[tokio::test]
    async fn generates_and_caches_physical_thumbnails() {
        let mut source = Cursor::new(Vec::new());
        DynamicImage::new_rgb8(4, 2)
            .write_to(&mut source, ImageFormat::Png)
            .unwrap();
        let source = source.into_inner();
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("photo.png"), &source).unwrap();
        let database_path = directory.path().join("database.sqlite");
        let database = Database::setup(&format!("sqlite:{}", database_path.display()))
            .await
            .unwrap();
        let file_id = sqlx::query(
            r#"
            INSERT INTO files (path, mtime, length, scan_id, is_directory)
            VALUES ('photo.png', datetime('now'), ?, 1, FALSE)
            "#,
        )
        .bind(source.len() as i64)
        .execute(database.as_ref())
        .await
        .unwrap()
        .last_insert_rowid();
        let state = AppState {
            db: database,
            base: directory.path().to_path_buf(),
            thumbnail_queue: ThumbnailQueue::new(1),
        };

        let response = thumbnail_response(&state, "photo.png", &HeaderMap::new()).await;
        assert_eq!(response.status(), axum::http::StatusCode::OK);
        assert_eq!(response.headers()[header::CONTENT_TYPE], "image/webp");
        let thumbnail = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert_eq!(image::guess_format(&thumbnail).unwrap(), ImageFormat::WebP);
        let cached = sqlx::query_scalar::<_, Vec<u8>>(
            "SELECT thumbnail FROM thumbnails WHERE file_id = ? AND subpath IS NULL",
        )
        .bind(file_id)
        .fetch_one(state.db.as_ref())
        .await
        .unwrap();
        assert_eq!(cached, thumbnail);

        let mut headers = HeaderMap::new();
        headers.insert(header::RANGE, "bytes=0-2".parse().unwrap());
        let response = thumbnail_response(&state, "photo.png", &headers).await;
        assert_eq!(response.status(), axum::http::StatusCode::PARTIAL_CONTENT);
        assert_eq!(
            to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap()
                .len(),
            3
        );
    }

    #[tokio::test]
    async fn cached_archive_thumbnails_do_not_open_the_archive() {
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
            base: directory.path().to_path_buf(),
            thumbnail_queue: ThumbnailQueue::new(1),
        };
        let headers = HeaderMap::new();

        for path in [
            "outer.zip/%3A/photo.jpg",
            "outer.zip/%3A,pw=wrong/photo.jpg",
            "outer.zip/%3A,pw=secret/photo.jpg",
        ] {
            let cached = thumbnail_response(&state, path, &headers).await;
            assert_eq!(cached.status(), axum::http::StatusCode::OK);
        }

        let mut range_headers = HeaderMap::new();
        range_headers.insert(header::RANGE, "bytes=1-2".parse().unwrap());
        let ranged =
            thumbnail_response(&state, "outer.zip/%3A,pw=wrong/photo.jpg", &range_headers).await;
        assert_eq!(ranged.status(), axum::http::StatusCode::PARTIAL_CONTENT);
        assert_eq!(
            to_bytes(ranged.into_body(), usize::MAX).await.unwrap(),
            [2, 3].as_slice()
        );

        sqlx::query("DELETE FROM thumbnails WHERE file_id = ?")
            .bind(file_id)
            .execute(state.db.as_ref())
            .await
            .unwrap();
        let uncached = thumbnail_response(&state, "outer.zip/%3A/photo.jpg", &headers).await;
        assert_eq!(uncached.status(), axum::http::StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn thumbnail_queue_limits_concurrent_work() {
        let queue = ThumbnailQueue::new(1);
        let first = queue.acquire().await;
        let waiting_queue = queue.clone();
        let second = tokio::spawn(async move { waiting_queue.acquire().await });

        tokio::task::yield_now().await;
        assert!(!second.is_finished());

        drop(first);
        assert!(second.await.unwrap().is_some());
    }

    #[tokio::test]
    async fn zero_thumbnail_concurrency_is_unlimited() {
        let queue = ThumbnailQueue::new(0);

        assert!(queue.acquire().await.is_none());
        assert!(queue.acquire().await.is_none());
    }
}
