use std::path::PathBuf;

use axum::{Json, Router, extract::State, http::{HeaderMap, Uri}, response::IntoResponse};
use bag_fs::{etag::Etag, fs::FsHandler};
use bag_lib::{action::Action, path::{Path, SegmentParseError}, ui::{Component, Gallery, GalleryImage, GalleryImageType, Image, Layout, Text}};
use chrono::{DateTime, Utc};
use tower_http::cors::{Any, CorsLayer};

use crate::db::Database;

#[derive(Clone)]
struct AppState {
    db: Database,
}

const DEFAULT_PAGE_SIZE: usize = 100;

pub async fn render(db: &Database, path: Path<'_>) -> anyhow::Result<Option<Layout>> {
    let bare = path.to_bare_string();
    let _parent = path.parent();
    let file = sqlx::query!("SELECT * FROM files WHERE path = ?", bare)
        .fetch_optional(db.as_ref())
        .await?;

    let Some(file) = file else { return Ok(None) };

    // Fetch self
    let result = if file.is_directory {
        // TODO: filter
        // TODO: sort

        let limit = path.last().arg("limit").and_then(|s| s.parse::<usize>().ok()).unwrap_or(DEFAULT_PAGE_SIZE) as i64;
        let offset = path.last().arg("offset").and_then(|s| s.parse::<usize>().ok()).unwrap_or(0) as i64;

        let children = sqlx::query!(r#"
            SELECT path, is_directory FROM files WHERE parent = ?
            ORDER BY is_directory DESC, mtime DESC
            LIMIT ? OFFSET ?
        "#, file.id, limit, offset)
            .fetch_all(db.as_ref())
            .await?;

        let images = children.into_iter().map(
            |row| GalleryImage {
                ty: if row.is_directory { GalleryImageType::Directory } else { GalleryImageType::File },
                thumbnail: if row.is_directory { None } else { Some(row.path.clone()) },
                name: row.path.rsplit_once("/").map(|e| e.1.to_owned()).unwrap_or(row.path.clone()),
                action: Some(Action::Navigate { to: row.path }),
            }
        ).collect();

        Layout {
            top: vec![],
            main: vec![
                Component::Gallery(Gallery { images })
            ],
            metadata: vec![],
            left: None,
            right: None,
        }
    } else {
        let name = file.path.rsplit_once("/").map(|e| e.1.to_owned()).unwrap_or(file.path.clone());
        Layout {
            top: vec![],
            main: vec![
                Component::Image(Image { resource: file.path.clone() }),
                Component::Text(Text { content: name }),
            ],
            metadata: vec![],
            left: None,
            right: None,
        }
    };

    // TODO: fetch siblings

    Ok(Some(result))
}

pub fn build(db: Database, root: PathBuf) -> Router {
    let fs = FsHandler::new(root);
    let raw_handler = Router::new().fallback(async move |state: State<AppState>, uri: Uri, headers: HeaderMap| {
        let path = uri.path();
        let path = path.trim_start_matches('/');
        // Query fs for the file mtime
        let metadata = sqlx::query!(r#"SELECT mtime AS "mtime: DateTime<Utc>", length FROM files WHERE path = ?"#, path)
            .fetch_optional(state.db.as_ref())
            .await;

        if let Some(etag) = headers.get(axum::http::header::IF_NONE_MATCH).and_then(|e| e.to_str().ok()) {
            if let Ok(Some(metadata)) = metadata {
                let mtime: std::time::SystemTime = metadata.mtime.into();
                let ref_etag = Etag { mtime, length: metadata.length as u64 }.hash_string();
                if etag == ref_etag {
                    return (axum::http::StatusCode::NOT_MODIFIED, "Not modified".to_string()).into_response();
                }
            }
        }
        // FIXME: get length

        fs.handle(path).await.into_response()
    });
    let render_handler = Router::new().fallback(async move |state: State<AppState>, uri: Uri| {
        // Trim prefixing & suffixing "/"
        let path = uri.path().trim_start_matches('/').trim_end_matches('/');
        tracing::info!("Render request: {}", path);
        let parsed = match Path::try_from(path) {
            Ok(p) => p,
            Err(SegmentParseError::InvalidArgument(arg)) => return (axum::http::StatusCode::BAD_REQUEST, format!("Invalid argument: {}", arg)).into_response(),
            Err(SegmentParseError::DecodeError(value)) => return (axum::http::StatusCode::BAD_REQUEST, format!("Decode error: {}", value)).into_response(),
        };

        match render(&state.db, parsed).await {
            Err(e) => (axum::http::StatusCode::INTERNAL_SERVER_ERROR, format!("Internal server error: {}", e)).into_response(),
            Ok(None) => (axum::http::StatusCode::NOT_FOUND, "Not found".to_string()).into_response(),
            Ok(Some(layout)) => Json(layout).into_response(),
        }
    })
    .layer(tower_http::compression::CompressionLayer::new());

    let state = AppState {
        db
    };

    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    let app = Router::new()
        .nest("/v1/raw/", raw_handler)
        .nest("/v1/render/", render_handler)
        .with_state(state)
        .layer(cors);
    app
}
