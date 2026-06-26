/* Iterate through the filesystem tree */

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use indicatif::ProgressStyle;

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

        // Delete thumbnail if exists, so next time it gets regenerated.
        sqlx::query!("DELETE FROM thumbnails WHERE file_id = ?", cur.id,)
            .execute(&mut *tx)
            .await?;

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
        let (id, _) = update_file(&db, &cur, &metadata, Some(parent), scan_id, last).await?;
        parent = id;
    }

    // Phase 2: walk the subtree under base
    // This is done through a recursion
    // Assume the base is already updated last level. ID is the ID of the base, not its parent.
    // Calling this function currently implies that base is a directory
    async fn walk(
        db: &Database,
        root: &Path,
        scan_id: i64,
        base: &Path,
        id: i64,
        counter: &mut i64,
        bar: &indicatif::ProgressBar,
    ) -> anyhow::Result<()> {
        let joined = root.join(&base);
        let mut entries = tokio::fs::read_dir(joined).await?;
        while let Some(entry) = entries.next_entry().await? {
            *counter += 1;
            let name = entry.file_name().to_string_lossy().into_owned();
            let child_path = base.join(name);
            let metadata = entry.metadata().await?;

            bar.inc(1);
            bar.set_message(format!("Scanning {}: {}", *counter, child_path.display()));

            let (cid, _) = update_file(
                &db,
                child_path.as_path(),
                &metadata,
                Some(id),
                scan_id,
                true,
            )
            .await?;
            tracing::debug!("Scanned: {} (id: {})", child_path.display(), cid);

            if metadata.is_dir() {
                Box::pin(walk(db, root, scan_id, &child_path, cid, counter, bar)).await?;
            }
        }

        Ok(())
    }

    let mut counter = 1;
    let bar = indicatif::ProgressBar::new_spinner()
        .with_style(
            ProgressStyle::with_template("{spinner} {elapsed_precise} [{per_sec}] {msg}").unwrap(),
        )
        .with_message(format!("Scanning 1: {}", base.as_ref().display()));
    walk(
        db,
        root.as_ref(),
        scan_id,
        base.as_ref(),
        parent,
        &mut counter,
        &bar,
    )
    .await?;

    // Phase 3: remove all stale entries that has scan_id < current scan.
    tracing::info!("Scanned {} entries", counter);
    let deleted = sqlx::query!(
        "DELETE FROM files WHERE is_stale = TRUE AND scan_id = ?",
        scan_id,
    )
    .execute(db.as_ref())
    .await?
    .rows_affected();
    if deleted > 0 {
        tracing::info!("Deleted {} stale entries", deleted);
    }

    // 3. Finalize the scan metadata. file_num comes from the iteration counter
    //    rather than a COUNT(*) so that concurrent scans don't corrupt it.
    let finished_at = Utc::now();
    sqlx::query!(
        "UPDATE scans SET file_num = ?, finished_at = ? WHERE id = ?",
        counter,
        finished_at,
        scan_id,
    )
    .execute(db.as_ref())
    .await?;

    Ok(())
}
