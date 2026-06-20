-- Add migration script here

CREATE TABLE files (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  path TEXT NOT NULL, -- Filename already embedded here. Relative to the root of the tree
  mtime DATETIME NOT NULL,
  scan_id INTEGER NOT NULL, -- The epoch of the last scanning task that saw this file. Used to remove the file at the end of the scan.

  -- The DIRECT PARENT of this file (a directory). NULL if this is at top-level inside the root directory
  parent INTEGER,

  -- The container of this file (an archive). This is used to delegate the file reader
  -- right now we don't support archives, so this is currently commented out.
  -- container INTEGER,

  -- Whether this file BEHAVES like a directory during render.
  -- Right now this always coincide with actually being a directory. But
  -- later when we support archives, archives will also have is_directory set to true
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
)
