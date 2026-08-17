use std::path::PathBuf;

use axum::{
    Json, Router,
    body::Body,
    extract::State,
    http::{HeaderMap, Response, Uri},
    response::IntoResponse,
    routing::get,
};
use bag_fs::{
    etag::Etag,
    fs::FsHandler,
    render::{ArchiveRenderParams, archive_parent_path},
    thumb::{extract_thumbnail_img, extract_thumbnail_video},
};
use bag_lib::{
    action::Action,
    path::{Path, SegmentParseError},
    ui::{Button, Component, Gallery, GalleryImage, GalleryImageType, Image, Layout, Text},
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

fn split_archive_path(path: &str) -> (&str, Option<&str>) {
    path.split_once("/:/")
        .map_or((path, None), |(outer, subpath)| (outer, Some(subpath)))
}

pub async fn render_file(
    db: &Database,
    fs: &FsHandler,
    path: Path<'_>,
) -> anyhow::Result<Option<Layout>> {
    let serialized = path.to_string();
    let bare = path
        .next()
        .map(|e| e.to_bare_string())
        .unwrap_or("".to_owned());
    let (outer, archive_subpath) = split_archive_path(&bare);
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
    let name = if path.segments().len() == 1 {
        "Root"
    } else {
        path.last().unwrap().name()
    };

    let mut metadata = vec![Component::Text(Text {
        content: name.to_owned(),
        variant: bag_lib::ui::TextVariant::Title,
        action: None,
    })];
    let parent = if archive_subpath.is_some() {
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

    if archive_subpath.is_some() || (!file.is_directory && is_archive_path(outer)) {
        let thumbnail = |path: Path<'_>| {
            let bare = path.next()?.to_bare_string();
            let (_, subpath) = split_archive_path(&bare);
            Some(format!(
                "thumbnail/{}/{}",
                file.id,
                urlencoding::encode(subpath?)
            ))
        };
        match fs
            .render_archive(ArchiveRenderParams {
                path,
                default_page_size: DEFAULT_PAGE_SIZE,
                thumbnail,
            })
            .await
        {
            Ok(mut layout) => {
                layout.metadata = metadata;
                return Ok(Some(layout));
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
                        Some(format!("thumbnail/{}", row.id))
                    },
                    name: row
                        .path
                        .rsplit_once("/")
                        .map(|e| e.1.to_owned())
                        .unwrap_or(row.path.clone()),
                    action: Some(Action::Navigate {
                        to: format!("{}/{}", serialized, row.path.rsplit("/").next().unwrap()),
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

    Ok(Some(result))
}

async fn thumbnail_response(state: &AppState, id: i64, subpath: String) -> Response<Body> {
    let response: anyhow::Result<Response<Body>> = async {
        let existing = sqlx::query!(
            "SELECT thumbnail, mime FROM thumbnails WHERE file_id = ? AND subpath = ?",
            id,
            subpath,
        )
        .fetch_optional(state.db.as_ref())
        .await?;
        let (thumbnail, mime) = if let Some(existing) = existing {
            (existing.thumbnail, existing.mime)
        } else {
            let file = sqlx::query!("SELECT path, is_directory FROM files WHERE id = ?", id)
                .fetch_optional(state.db.as_ref())
                .await?;
            let Some(file) = file else {
                return Ok(
                    (axum::http::StatusCode::NOT_FOUND, "Not Found".to_owned()).into_response()
                );
            };
            if (file.is_directory || is_archive_path(&file.path)) && subpath.is_empty() {
                return Ok((
                    axum::http::StatusCode::BAD_REQUEST,
                    "Directories and archives do not have thumbnails".to_owned(),
                )
                    .into_response());
            }

            let media_path = if subpath.is_empty() {
                file.path.clone()
            } else {
                subpath.clone()
            };
            let source_path = if subpath.is_empty() {
                file.path.clone()
            } else {
                format!("{}/:/{}", file.path, subpath)
            };
            let source = match state.fs.open_buffered(&source_path).await {
                Ok(source) => source,
                Err(error) => {
                    tracing::error!(
                        "Failed to open thumbnail source {} ({}): {}",
                        file.path,
                        subpath,
                        error
                    );
                    return Ok((
                        axum::http::StatusCode::NOT_FOUND,
                        "Failed to open thumbnail source".to_owned(),
                    )
                        .into_response());
                }
            };
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
                        file.path,
                        subpath,
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
                id,
                subpath,
                thumbnail,
            )
            .execute(state.db.as_ref())
            .await
            {
                tracing::error!(
                    "Failed to update thumbnail for {} ({}): {}",
                    id,
                    subpath,
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
                id,
                subpath,
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

async fn physical_thumbnail_handler(
    State(state): State<AppState>,
    axum::extract::Path(id): axum::extract::Path<i64>,
) -> Response<Body> {
    thumbnail_response(&state, id, String::new()).await
}

async fn archive_thumbnail_handler(
    State(state): State<AppState>,
    axum::extract::Path((id, subpath)): axum::extract::Path<(i64, String)>,
) -> Response<Body> {
    thumbnail_response(&state, id, subpath).await
}

pub fn build(db: Database, root: PathBuf) -> Router {
    let fs = FsHandler::new(root.clone());
    let raw_fs = fs.clone();
    let file_handler = Router::new().fallback(
        async move |state: State<AppState>, uri: Uri, headers: HeaderMap| {
            let path = uri.path();
            let path = path.trim_start_matches('/');
            // Query fs for the file mtime
            let metadata = sqlx::query!(
                r#"SELECT mtime AS "mtime: DateTime<Utc>", length FROM files WHERE path = ?"#,
                path
            )
            .fetch_optional(state.db.as_ref())
            .await;

            if let Some(etag) = headers
                .get(axum::http::header::IF_NONE_MATCH)
                .and_then(|e| e.to_str().ok())
                && let Ok(Some(metadata)) = metadata {
                    // TODO: correctly set subpath
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

            raw_fs.handle(path, &headers).await.into_response()
        },
    );
    let render_handler = Router::new()
        .fallback(async move |state: State<AppState>, uri: Uri| {
            // Trim prefixing & suffixing "/"
            let path = uri.path().trim_start_matches('/').trim_end_matches('/');
            tracing::info!("Render request: {}", path);
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

            if parsed.first().map(|e| e.name()) == Some("file") {
                match render_file(&state.db, &state.fs, parsed).await {
                    Err(e) => (
                        axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                        format!("Internal server error: {}", e),
                    )
                        .into_response(),
                    Ok(None) => {
                        (axum::http::StatusCode::NOT_FOUND, "Not found".to_string()).into_response()
                    }
                    Ok(Some(layout)) => Json(layout).into_response(),
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
        .route("/v1/raw/thumbnail/{id}", get(physical_thumbnail_handler))
        .route(
            "/v1/raw/thumbnail/{id}/{subpath}",
            get(archive_thumbnail_handler),
        )
        .nest("/v1/raw/file/", file_handler)
        .nest("/v1/render/", render_handler)
        .with_state(state)
        .layer(cors)
}

#[cfg(test)]
mod tests {
    use super::is_archive_path;

    #[test]
    fn detects_archive_paths() {
        assert!(is_archive_path("gallery.ZIP"));
        assert!(!is_archive_path("gallery.png"));
    }
}
