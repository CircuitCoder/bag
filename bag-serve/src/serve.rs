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
}

const DEFAULT_PAGE_SIZE: usize = 100;

pub async fn render_file(db: &Database, path: Path<'_>) -> anyhow::Result<Option<Layout>> {
    let serialized = path.to_string();
    let bare = path
        .next()
        .map(|e| e.to_bare_string())
        .unwrap_or("".to_owned());
    let _parent = path.parent();
    let file = sqlx::query!(
        r#"
        SELECT
          id,
          mtime as "mtime: DateTime<Utc>",
          is_directory,
          parent
        FROM files WHERE path = ?
    "#,
        bare
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
    if let Some(parent) = path.parent() {
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
            action: Action::Navigate {
                to: parent.to_string(),
            },
        }));
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
            ORDER BY is_directory DESC, mtime DESC, path DESC
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
            .into_iter()
            .map(|row| GalleryImage {
                ty: if row.is_directory {
                    GalleryImageType::Directory
                } else {
                    GalleryImageType::File
                },
                thumbnail: if row.is_directory {
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

pub fn build(db: Database, root: PathBuf) -> Router {
    let fs = FsHandler::new(root.clone());
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
            {
                if let Ok(Some(metadata)) = metadata {
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
            }
            // FIXME: get length

            fs.handle(path, &headers).await.into_response()
        },
    );
    let thumbnail_handler = {
        let db = db.clone();
        let root = root.clone();
        async move |axum::extract::Path(id): axum::extract::Path<i64>| -> axum::response::Response {
            let resp: anyhow::Result<axum::response::Response> = async {
                let existing = sqlx::query!("SELECT thumbnail, mime FROM thumbnails WHERE file_id = ?", id)
                    .fetch_optional(db.as_ref())
                    .await?;
                let (tb, mime) = if let Some(existing) = existing {
                    (existing.thumbnail, existing.mime)
                } else {
                    let file = sqlx::query!("SELECT path, is_directory FROM files WHERE id = ?", id)
                        .fetch_optional(db.as_ref())
                        .await?;
                    let Some(file) = file else {
                        return Ok((axum::http::StatusCode::NOT_FOUND, "Not Found".to_string()).into_response());
                    };
                    if file.is_directory {
                        return Ok((axum::http::StatusCode::BAD_REQUEST, "Directories does not have thumbnails".to_string()).into_response());
                    }
                    let is_video = mime_guess::from_path(&file.path)
                        .first_or_octet_stream()
                        .type_()
                        .as_str() == "video";

                    let path = root.join(&file.path);

                    let tb = if is_video {
                        extract_thumbnail_video(path, 150).await
                    } else {
                        extract_thumbnail_img(path, 150).await
                    };

                    let tb = match tb {
                        Ok(tb) => tb,
                        Err(e) => {
                            tracing::error!("Failed to extract thumbnail for file {}: {}", file.path, e);
                            // TODO: cache this also
                            return Ok((axum::http::StatusCode::NOT_FOUND, "Failed to extract thumbnail".to_string()).into_response());
                        }
                    };

                    // Tries to update thumbnail, squash error
                    if let Err(e) = sqlx::query!(
                        "INSERT INTO thumbnails (file_id, thumbnail, mime) VALUES (?, ?, 'image/webp') ON CONFLICT(file_id) DO UPDATE SET thumbnail = excluded.thumbnail, mime = excluded.mime",
                        id,
                        tb,
                    ).execute(db.as_ref()).await {
                        tracing::error!("Failed to update thumbnail for file {}: {}", id, e);
                    }

                    (tb, "image/webp".to_owned())
                };

                return Ok(Response::builder()
                    .header("Content-Type", mime)
                    .header("Cache-Control", "max-age=3600, stale-while-revalidate=86400")
                    .body(tb.into())
                    .unwrap());
            }.await;

            match resp {
                Ok(resp) => resp,
                Err(e) => {
                    tracing::error!("Failed to handle thumbnail request for file {}: {}", id, e);
                    (
                        axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                        "Internal Server Error".to_string(),
                    )
                        .into_response()
                }
            }
        }
    };

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
                match render_file(&state.db, parsed).await {
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

    let state = AppState { db };

    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    let app = Router::new()
        .route("/v1/raw/thumbnail/{id}", get(thumbnail_handler))
        .nest("/v1/raw/file/", file_handler)
        .nest("/v1/render/", render_handler)
        .with_state(state)
        .layer(cors);
    app
}
