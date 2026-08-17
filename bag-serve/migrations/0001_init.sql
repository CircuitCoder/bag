-- Add migration script here

CREATE TABLE files (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  path TEXT NOT NULL, -- Filename already embedded here. Relative to the root of the tree
  mtime DATETIME NOT NULL,
  length INTEGER NOT NULL,
  scan_id INTEGER NOT NULL, -- The epoch of the last scanning task that saw this file. Used to remove the file at the end of the scan.

  -- The DIRECT PARENT of this file (a directory). NULL if this is the top-level directory itself
  parent INTEGER,

  -- Archive members are discovered dynamically and are not represented in this table.

  -- Whether this file is a physical filesystem directory. Archive behavior is
  -- determined from the file's MIME type during rendering.
  is_directory BOOLEAN NOT NULL,

  -- Is marked as stale (by the scan with ID = scan_id). Will be removed at the end of that scan
  is_stale BOOLEAN NOT NULL DEFAULT FALSE,

  -- Some intrinsic tags
  marked BOOLEAN NOT NULL DEFAULT FALSE,
  hidden BOOLEAN NOT NULL DEFAULT FALSE,

  FOREIGN KEY (parent) REFERENCES files(id) ON DELETE CASCADE
  UNIQUE (path)
);

-- Previous scanning tasks
CREATE TABLE scans (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  started_at DATETIME NOT NULL,
  file_num INTEGER NOT NULL,
  finished_at DATETIME
);

-- Thumbnail data
CREATE TABLE thumbnails (
    file_id INTEGER NOT NULL,
    -- Empty for the physical file; otherwise the decoded path below an archive.
    -- Nested archives use the same /:/ delimiter as raw file paths.
    subpath TEXT NOT NULL DEFAULT '',
    thumbnail BLOB NOT NULL,
    mime TEXT NOT NULL,

    -- This is also the index for the thumbnail cache's primary lookup shape.
    PRIMARY KEY (file_id, subpath),

    FOREIGN KEY (file_id)
        REFERENCES files(id)
        ON DELETE CASCADE
) WITHOUT ROWID;
