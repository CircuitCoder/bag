/* Iterate through the filesystem tree */

use std::{path::{Path, PathBuf}, sync::{Arc, atomic::AtomicI64}};

use bag_fs::thumb::extract_thumbnail;
use chrono::{DateTime, Utc};
use indicatif::ProgressStyle;
use tokio::task::JoinSet;

use crate::db::Database;

pub fn sanitize_base_path<P: AsRef<Path>>(base: P) -> anyhow::Result<()> {
    if base
        .as_ref()
        .as_os_str()
        .as_encoded_bytes()
        .last()
        .copied()
        .is_some_and(|e| e == std::path::MAIN_SEPARATOR as u8)
    {
        return Err(anyhow::anyhow!(
            "Base path must not end with a trailing separator"
        ));
    }

    for seg in base.as_ref().components() {
        if let std::path::Component::Normal(_) = seg {
            // OK
        } else {
            return Err(anyhow::anyhow!("Base path must be normal components"));
        }
    }
    Ok(())
}

/**
 * root: absolute path
 * base: relative path, not ending with "/", contains no ".." or "."
 */
pub async fn rescan<P1: AsRef<Path>, P2: AsRef<Path>>(
    root: P1, // The root of the entire tree
    base: P2, // Scanning from here
    db: &Database,
) -> anyhow::Result<()> {
    sanitize_base_path(base.as_ref())?;

    let started_at = Utc::now();
    let scan_id = sqlx::query!(
        "INSERT INTO scans (started_at, file_num) VALUES (?, 0)",
        started_at,
    )
    .execute(db.as_ref())
    .await?
    .last_insert_rowid();

    // Upsert a file entry. Returns the ID whether the file was actually updated (so if it's a archive, a recursion descent is needed)
    // Note that if the file has newer scan id than our current scan, it's not updated at all.
    async fn update_file(
        db: &Database,
        path: &Path,
        metadata: &std::fs::Metadata,
        parent: Option<i64>,
        scan_id: i64,
        mark_stale: bool,
        thumb: Option<&[u8]>,
    ) -> anyhow::Result<(i64, bool)> {
        let mut tx = db.as_ref().begin_with("BEGIN IMMEDIATE").await?;
        let path = &path
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("Path is not valid UTF-8: {}", path.display()))?;
        let cur = sqlx::query!(
            r#"SELECT id AS "id!", scan_id, mtime AS "mtime: DateTime<Utc>" FROM files WHERE path = ?"#,
            path
        ).fetch_optional(&mut *tx).await?;
        let mtime = DateTime::<Utc>::from(metadata.modified()?);
        let length = metadata.len() as i64;
        let is_dir = metadata.is_dir();

        async fn set_thumbnail(tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>, id: i64, thumb: &[u8]) -> anyhow::Result<()> {
            // Upsert into the thumbnails table
            sqlx::query!(
                "INSERT INTO thumbnails (file_id, thumbnail, mime) VALUES (?, ?, 'image/webp') ON CONFLICT(file_id) DO UPDATE SET thumbnail = excluded.thumbnail, mime = excluded.mime",
                id,
                thumb,
            ).execute(&mut **tx).await?;
            Ok(())
        }

        let Some(cur) = cur else {
            let inserted = sqlx::query!(
                "INSERT INTO files (path, mtime, length, scan_id, parent, is_directory) VALUES (?, ?, ?, ?, ?, ?)",
                path,
                mtime,
                length,
                scan_id,
                parent,
                is_dir, // TODO: archive
            ).execute(&mut *tx).await?.last_insert_rowid();
            if let Some(t) = thumb {
                set_thumbnail(&mut tx, inserted, t).await?;
            }
            tx.commit().await?;
            return Ok((inserted, true));
        };

        // Another scan raced ahead of us
        // Note that cur.scan_id == scan_id is possible.
        // We update the scan_id field when marking files as stale
        if cur.scan_id > scan_id {
            return Ok((cur.id, false));
        }

        // Update ourselves
        // Inside the same TX, so it's guaranteed that scan_id < current scan
        // This clears the is_stale flag
        sqlx::query!(
            "UPDATE files SET scan_id = ?, mtime = ?, length = ?, parent = ?, is_directory = ?, is_stale = FALSE WHERE id = ?",
            scan_id,
            mtime,
            length,
            parent,
            is_dir, // TODO: archive
            cur.id,
        ).execute(&mut *tx).await?;

        if let Some(t) = thumb {
            set_thumbnail(&mut tx, cur.id, t).await?;
        } else {
            // Delete thumbnail if exists
            sqlx::query!(
                "DELETE FROM thumbnails WHERE file_id = ?",
                cur.id,
            ).execute(&mut *tx).await?;
        }

        // If unchanged, don't mark children as stale
        if cur.mtime >= mtime {
            if cur.mtime > mtime {
                tracing::warn!(
                    "File {} has mtime {} but database has {}, which is newer. Time unwinded?",
                    path,
                    mtime,
                    cur.mtime
                );
            }
            tx.commit().await?;
            return Ok((cur.id, false));
        }

        if mark_stale {
            // Mark all direct children of this entry as stale
            // Only mark them if scan_id < current scan
            sqlx::query!(
                "UPDATE files SET is_stale = TRUE, scan_id = ? WHERE parent = ? AND scan_id < ?",
                scan_id,
                cur.id,
                scan_id,
            )
            .execute(&mut *tx)
            .await?;
        }

        tx.commit().await?;

        Ok((cur.id, true))
    }

    let root_metadata = root.as_ref().metadata().map_err(|e| {
        anyhow::anyhow!(
            "Failed to get metadata for root directory {}: {}",
            root.as_ref().display(),
            e
        )
    })?;
    let (mut parent, _) = update_file(
        &db,
        Path::new(""),
        &root_metadata,
        None,
        scan_id,
        base.as_ref().is_empty(),
        None,
    )
    .await?;

    // Phase 1: iterate through all parent directories in base, ensure that they are created, get their IDs.
    let mut cur = PathBuf::new();
    for seg in base.as_ref().components() {
        cur.push(seg.as_os_str());
        let joined = root.as_ref().join(&cur);
        let metadata = joined.metadata().map_err(|e| {
            anyhow::anyhow!(
                "Failed to get metadata for parent directory {} in base: {}",
                cur.display(),
                e
            )
        })?;
        // TODO: allow scan base to be inside archive files
        if !metadata.is_dir() {
            return Err(anyhow::anyhow!(
                "Base path {} is not a directory",
                joined.display()
            ));
        }

        // TODO: to_string_lossy here is OK?
        let last = cur == base.as_ref();
        let (id, _) = update_file(&db, &cur, &metadata, Some(parent), scan_id, last, None).await?;
        parent = id;
    }

    // Phase 2: walk the subtree under base
    // This is done through a recursion
    // Assume the base is already updated last level. ID is the ID of the base, not its parent.
    // Calling this function currently implies that base is a directory
    fn walk(
        db: Database,
        root: PathBuf,
        scan_id: i64,
        base: PathBuf,
        id: i64,
        counter: Arc<AtomicI64>,
        bar: indicatif::ProgressBar,
        sem: Option<Arc<tokio::sync::Semaphore>>,
    ) -> impl Future<Output = anyhow::Result<()>> + Send {
        async move {
            let joined = root.join(&base);
            let mut entries = tokio::fs::read_dir(joined).await?;
            let mut children = JoinSet::new();
            while let Some(entry) = entries.next_entry().await? {
                let name = entry.file_name().to_string_lossy().into_owned();
                let child_path = base.join(name);
                let metadata = entry.metadata().await?;

                let permit = if let Some(ref s) = sem {
                    Some(s.clone().acquire_owned().await?)
                } else {
                    None
                };

                let bar = bar.clone();
                let counter = counter.clone();
                let db = db.clone();
                let root = root.clone();
                let sem = sem.clone();
                children.spawn(async move {
                    let new = counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                    bar.inc(1);
                    bar.set_message(format!("Scanning {}: {}", new, child_path.display()));

                    // Try to extract thumbnail
                    // TODO: skipping generating thumbnail if mtime did not change

                    // Parallelize thumbnail extraction
                    let mut thumb = None;
                    if metadata.is_file() {
                        let mime = mime_guess::from_path(&child_path).first_or_octet_stream();
                        let ty = mime.type_().as_str();
                        if ty == "video" {
                            match extract_thumbnail(entry.path().as_ref()).await {
                                Err(e) => {
                                    tracing::error!("Failed to extract thumbnail for {}: {}", child_path.display(), e);
                                }
                                Ok(v) => thumb = Some(v),
                            }
                        }
                    }
                    let (cid, _) =
                        update_file(&db, child_path.as_path(), &metadata, Some(id), scan_id, true, thumb.as_deref()).await?;
                    tracing::debug!("Scanned: {} (id: {})", child_path.display(), cid);

                    // Explicitly drops the permit inside the closure to move
                    // it into the async block
                    // Dropped before recursion to avoid deadlock.
                    drop(permit);

                    if metadata.is_dir() {
                        walk(db, root, scan_id, child_path, cid, counter, bar, sem).await?;
                    }
                    Ok::<_, anyhow::Error>(())
                });
            }

            while let Some(res) = children.join_next().await {
                res??;
            }

            Ok(())
        }
    }

    let counter = Arc::new(AtomicI64::new(1));
    let bar = indicatif::ProgressBar::new_spinner()
        .with_style(
            ProgressStyle::with_template("{spinner} {elapsed_precise} [{per_sec}] {msg}").unwrap(),
        )
        .with_message(format!("Scanning 1: {}", base.as_ref().display()));
    let sem = Arc::new(tokio::sync::Semaphore::new(16));
    walk(
        db.clone(),
        root.as_ref().to_path_buf(),
        scan_id,
        base.as_ref().to_path_buf(),
        parent,
        counter.clone(),
        bar,
        Some(sem),
    )
    .await?;

    // Phase 3: remove all stale entries that has scan_id < current scan.
    sqlx::query!(
        "DELETE FROM files WHERE is_stale = TRUE AND scan_id = ?",
        scan_id,
    )
    .execute(db.as_ref())
    .await?;

    // 3. Finalize the scan metadata. file_num comes from the iteration counter
    //    rather than a COUNT(*) so that concurrent scans don't corrupt it.
    let finished_at = Utc::now();
    let counter_val = counter.load(std::sync::atomic::Ordering::Relaxed);
    sqlx::query!(
        "UPDATE scans SET file_num = ?, finished_at = ? WHERE id = ?",
        counter_val,
        finished_at,
        scan_id,
    )
    .execute(db.as_ref())
    .await?;

    Ok(())
}
