/* Iterate through the filesystem tree */

use std::{
    borrow::Cow,
    collections::VecDeque,
    path::{Path, PathBuf},
    sync::Arc,
};

use chrono::{DateTime, Utc};
use indicatif::ProgressStyle;
use inotify::{StreamExt, WatchDescriptor, Watches};

use crate::db::Database;

fn inotify_mask() -> inotify::WatchMask {
    inotify::WatchMask::CREATE
        | inotify::WatchMask::DELETE
        | inotify::WatchMask::CLOSE_WRITE
        | inotify::WatchMask::MOVED_FROM
        | inotify::WatchMask::MOVED_TO
        | inotify::WatchMask::ATTRIB
        // Erroring types
        | inotify::WatchMask::DELETE_SELF
        | inotify::WatchMask::MOVE_SELF
}

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
    rescan_inner(root, base, db, true, None).await
}

// Single thread / async flow access.
struct WatchContext {
    handle: Watches,
    // paths is **relative** to the **root path**
    // fwd is the **speculative** state, only used for removing watches
    //
    // fwd is a superset of kernel mapping at wall clock
    fwd: std::collections::HashMap<PathBuf, WatchDescriptor>,
    // rev is the **queued** state, or the nonspeculative state w.r.t. current inotify
    // handler pointer. Adding a watch will append to rev
    // rev is essentially a timeline for unprocessed IN_IGNORE and live mapping
    rev: std::collections::HashMap<WatchDescriptor, VecDeque<PathBuf>>,
    // Invariant:
    // fwd is a bijection: at most one wd will be mapped
    // fwd is a superset of what could've been watched at the wall clock
    // if fwd(path) == wd, then rev(wd).last() == path (but not in reverse)
}

impl WatchContext {
    // Returns false if already exists
    pub fn add(
        &mut self,
        root: &Path,
        path: &Path,
        mask: inotify::WatchMask,
    ) -> anyhow::Result<bool> {
        // We never add watches if there is a possibility of it being already added
        // So every inotify_add_watch returns an actual newly allocated wd
        //
        // This will unfortunately cause us occasionally not tracking some directoy,
        // in the case of fs / event handling race. Consider if a directory is deleted and immediately
        // recreated. Since this is a new inode, the CREATE is triggered on the parent inode, and may
        // race with IGNORE
        //
        // DELETE_SELF -> DELETE -> CREATE -> IGNORE
        //
        // Further more, if during DELETE_SELF's processing, we already read the created directory,
        // then in the handling of DELETE_SELF, DELETE and CREATE, we won't be tracking anything new.
        // Then the ignore will leave this path untracked.
        //
        // TODO: this assumption is false for system with hardlinks & symlinks
        if self.fwd.contains_key(path) {
            return Ok(false);
        }
        let wd = self.handle.add(root.join(path), mask)?;

        // Remove all path mapping to wd in rev,
        // so that we'll not be errornously removed
        let rev = self.rev.entry(wd.clone()).or_default();
        let last = rev.back();
        if last.is_some() && self.fwd.get(last.unwrap()) == Some(&wd) && last.unwrap() != path {
            self.fwd.remove(last.unwrap());
        }

        self.fwd.insert(path.to_path_buf(), wd.clone());
        // We know we must be a new watch, push into rev
        rev.push_back(path.to_path_buf());
        Ok(true)
    }

    pub fn query_wd_concrete(&self, wd: WatchDescriptor) -> Option<&PathBuf> {
        self.rev.get(&wd)?.front()
    }

    pub fn remove_start(&mut self, path: &Path) -> anyhow::Result<bool> {
        // fwd is a superset
        // if path is not in fwd, it must not be watched
        let Some(wd) = self.fwd.remove(path) else {
            return Ok(false);
        };
        if let Err(e) = self.handle.remove(wd)
            && e.kind() != std::io::ErrorKind::InvalidInput {
                return Err(e.into());
            }
            // EINVAL means the watch descriptor is already removed (maybe automatically)
            // which should be fine
        // Don't modify rev yet
        Ok(true)
    }

    pub fn remove_commit(&mut self, wd: WatchDescriptor) -> anyhow::Result<PathBuf> {
        let ret = self
            .rev
            .get_mut(&wd)
            .and_then(VecDeque::pop_front)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "Watch descriptor {} has no paths",
                    wd.get_watch_descriptor_id()
                )
            })?;

        // self.rev must have wd now
        if self.rev.get(&wd).unwrap().is_empty() {
            self.rev.remove(&wd);
            // What we've just removed is the last for wd, so try to remove fwd mapping
            if self.fwd.get(&ret) == Some(&wd) {
                self.fwd.remove(&ret);
            }
        }
        Ok(ret)
    }
}

async fn rescan_inner<P1: AsRef<Path>, P2: AsRef<Path>>(
    root: P1, // The root of the entire tree
    base: P2, // Scanning from here
    db: &Database,
    ind: bool,
    watches: Option<&tokio::sync::Mutex<WatchContext>>, // Watches to update, if any
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
    ) -> Result<(i64, bool), sqlx::Error> {
        let mut tx = db.as_ref().begin_with("BEGIN IMMEDIATE").await?;
        let path = &path.to_str().expect("Path is not valid UTF-8");
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
            // Parent may be deleted by a newer scan right now
            // Note that the ID field is INTEGER PRIMARY KEY AUTOINCREMENT, so it's never going to be reused
            // This will be handled by the walk function
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

    // Phase 1: iterate through all parent directories in base, ensure that they are created, get their IDs.
    let mut cur = PathBuf::new();
    let mut parent = None;
    if !base.as_ref().as_os_str().is_empty() {
        for seg in base.as_ref().components() {
            let joined = root.as_ref().join(&cur);
            let metadata = joined.metadata().map_err(|e| {
                anyhow::anyhow!(
                    "Failed to get metadata for parent directory {} in base: {}",
                    cur.display(),
                    e
                )
            })?;
            // TODO: allow scan base to be inside archive files
            let is_dir = metadata.is_dir();
            if !is_dir {
                return Err(anyhow::anyhow!(
                    "Base path {} is not a directory",
                    joined.display()
                ));
            }

            let (id, _) = update_file(db, &cur, &metadata, parent, scan_id, false).await?;
            parent = Some(id);
            cur.push(seg.as_os_str());
        }
    }

    // Phase 2: walk the subtree under base
    // This is done through a recursion
    // Assume the base is already updated last level. ID is the ID of the base, not its parent.
    // Calling this function currently implies that base is a directory

    macro_rules! ignore_missing {
        ($e:expr, $b:block) => {
            match $e {
                Ok(e) => e,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => $b,
                Err(e) => return Err(e.into()),
            }
        };
    }

    async fn walk(
        db: &Database,
        root: &Path,
        scan_id: i64,
        base: &Path,
        id: i64,
        counter: &mut i64,
        bar: Option<&indicatif::ProgressBar>,
        watches: Option<&tokio::sync::Mutex<WatchContext>>,
    ) -> anyhow::Result<()> {
        // Before reading dir, add watches
        // TODO: same as below: we may've been deleted in FS
        if let Some(watches) = watches {
            watches.lock().await.add(root, base, inotify_mask())?;
        }

        let joined = root.join(base);
        // TODO: we may've been deleted in FS
        let mut entries = ignore_missing!(tokio::fs::read_dir(joined).await, { return Ok(()) });
        while let Some(entry) = entries.next_entry().await? {
            *counter += 1;
            let name = entry.file_name().to_string_lossy().into_owned();
            let child_path = base.join(name);
            let metadata = ignore_missing!(entry.metadata().await, { continue });

            if let Some(bar) = bar {
                bar.inc(1);
                bar.set_message(format!("Scanning {}: {}", *counter, child_path.display()));
            }

            let (cid, _) = match update_file(
                db,
                child_path.as_path(),
                &metadata,
                Some(id),
                scan_id,
                true,
            )
            .await
            {
                Ok(r) => r,
                Err(sqlx::Error::Database(e))
                    if e.kind() == sqlx::error::ErrorKind::ForeignKeyViolation =>
                {
                    // Some newer scan has delete the parent
                    // just return
                    return Ok(());
                }
                Err(e) => return Err(e.into()),
            };
            tracing::debug!("Scanned: {} (id: {})", child_path.display(), cid);

            if metadata.is_dir() {
                Box::pin(walk(
                    db,
                    root,
                    scan_id,
                    &child_path,
                    cid,
                    counter,
                    bar,
                    watches,
                ))
                .await?;
            }
        }

        Ok(())
    }

    // Now parent above us are all created
    // parent = base's parent ID
    // cur == base

    // Read my own metadata
    // Myself maybe missing
    let base_metadata = root.as_ref().join(base.as_ref()).metadata();
    let (scanned, deleted) = match base_metadata {
        Ok(metadata) => {
            let (id, _) = update_file(db, base.as_ref(), &metadata, parent, scan_id, true).await?;

            let scanned = if metadata.is_dir() {
                let mut counter = 1;
                let bar = if ind {
                    Some(
                        indicatif::ProgressBar::new_spinner()
                            .with_style(
                                ProgressStyle::with_template(
                                    "{spinner} {elapsed_precise} [{per_sec}] {msg}",
                                )
                                .unwrap(),
                            )
                            .with_message(format!("Scanning 1: {}", base.as_ref().display())),
                    )
                } else {
                    None
                };
                walk(
                    db,
                    root.as_ref(),
                    scan_id,
                    base.as_ref(),
                    id,
                    &mut counter,
                    bar.as_ref(),
                    watches,
                )
                .await?;

                counter
            } else {
                1
            };

            // Phase 3: remove all stale entries that has scan_id < current scan.
            let deleted = sqlx::query!(
                "DELETE FROM files WHERE is_stale = TRUE AND scan_id = ?",
                scan_id,
            )
            .execute(db.as_ref())
            .await?
            .rows_affected();
            (scanned, deleted)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // Base path does not exists, judge by whether
            // base == "" or not
            if base.as_ref().as_os_str().is_empty() {
                return Err(anyhow::anyhow!(
                    "Root path {} does not exists: {}",
                    root.as_ref().display(),
                    e
                ));
            } else {
                // We're fine with this, just cascade delete this entry
                // Note that here we use scan_id < current scan id
                // Because we've not even updated stale, and this deletion alone should be atomic
                // And if this entry's scan_id is < current scan, its (transitive) children must haven't been seen
                // by future scans
                //
                // This can cause a race, where a older scan is appending child to the subtree
                // under this path.
                //
                // This is handled on the walking side

                let base_str = base.as_ref().to_str().unwrap();
                let deleted = sqlx::query!(
                    "DELETE FROM files WHERE path = ? AND scan_id < ?",
                    base_str,
                    scan_id,
                )
                .execute(db.as_ref())
                .await?
                .rows_affected();

                if let Some(w) = watches {
                    w.lock().await.remove_start(base.as_ref())?;
                }

                (0, deleted)
            }
        }
        Err(e) => return Err(e.into()),
    };

    tracing::info!("Scanned {} entries, deleted {} entries", scanned, deleted);

    // 3. Finalize the scan metadata. file_num comes from the iteration counter
    //    rather than a COUNT(*) so that concurrent scans don't corrupt it.
    let finished_at = Utc::now();
    sqlx::query!(
        "UPDATE scans SET file_num = ?, finished_at = ? WHERE id = ?",
        scanned,
        finished_at,
        scan_id,
    )
    .execute(db.as_ref())
    .await?;

    Ok(())
}

/**
 * Watch for updates in the filesystem tree
 *
 * It also triggers at lease one initial rescan of the base directory.
 */
pub async fn watch<P1: AsRef<Path>, P2: AsRef<Path>>(
    root: P1, // The root of the entire tree
    base: P2, // Scanning from here
    db: &Database,
    mut shutdown: tokio::sync::oneshot::Receiver<()>,
) -> anyhow::Result<()> {
    sanitize_base_path(base.as_ref())?;
    let watcher = inotify::Inotify::init()?;
    let fullbase = root.as_ref().join(base.as_ref());
    // Adding watches for base directory AOT.
    // Since if there is absolutely no base directory, watch should immediate
    // fail.
    // TODO: This is a pretty fast action, we shouldn't need to spawn
    // a blocking operation for this.
    let mut ctx = WatchContext {
        handle: watcher.watches(),
        fwd: std::collections::HashMap::new(),
        rev: std::collections::HashMap::new(),
    };
    ctx.add(root.as_ref(), base.as_ref(), inotify_mask())?;
    let ctx = Arc::new(tokio::sync::Mutex::new(ctx));

    let mut stream = watcher.into_event_stream(vec![0; 4096])?;

    // Initial scan
    let root_cloned = root.as_ref().to_path_buf();
    let base_cloned = base.as_ref().to_path_buf();
    let db_cloned = db.clone();
    let ctx_cloned = ctx.clone();
    tokio::spawn(async move {
        tracing::info!("Initial scan...");
        if let Err(e) = rescan_inner(
            &root_cloned,
            &base_cloned,
            &db_cloned,
            true,
            Some(&*ctx_cloned),
        )
        .await
        {
            tracing::error!("Initial scan failed: {}", e);
        }
        tracing::info!("Initial scan completed");
    });

    tracing::info!("Watching {}", fullbase.display());
    loop {
        tokio::select! {
            _ = &mut shutdown => {
                tracing::info!("Stopping watch");
                return Ok(());
            }
            ev = stream.next() => {
                let Some(ev) = ev else {
                    anyhow::bail!("Watch stream ended unexpectedly");
                };

                let ev = ev?;

                if ev.mask.contains(inotify::EventMask::IGNORED) {
                    // This is a retirement of a watch descriptor
                    ctx.lock().await.remove_commit(ev.wd)?;
                    continue;
                } else if ev.mask.contains(inotify::EventMask::Q_OVERFLOW) {
                    anyhow::bail!("Event queue overflowed")
                }

                let evbase = {
                    ctx.lock().await.query_wd_concrete(ev.wd.clone()).ok_or_else(|| anyhow::anyhow!("Watch descriptor {} not found", ev.wd.get_watch_descriptor_id()))?.to_path_buf()
                };
                if (inotify::EventMask::DELETE_SELF | inotify::EventMask::MOVE_SELF).intersects(ev.mask)
                    && evbase == base.as_ref() {
                        anyhow::bail!("Base directory {} was moved / deleted", fullbase.display());
                    };
                    // For other cases, watches will get removed inside rescan_inner

                // The relative path (with root) of the updated file
                let mut file: Cow<'_, Path> = evbase.into();
                if let Some(name) = ev.name {
                    file = file.join(name).into();
                }
                tracing::debug!("Event: {:?} on {}", ev.mask, file.display());
                tracing::info!("Refresh {}", file.display());
                rescan_inner(&root, &file, db, false, Some(&*ctx)).await?;
            }
        }
    }
}
