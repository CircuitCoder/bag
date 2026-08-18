CREATE TABLE thumbnails_new (
    file_id INTEGER NOT NULL,
    -- NULL identifies the physical file; otherwise this is the decoded path
    -- below an archive. Nested archives use the same /:/ delimiter as raw paths.
    subpath TEXT CHECK (subpath IS NULL OR subpath <> ''),
    thumbnail BLOB NOT NULL,
    mime TEXT NOT NULL,

    -- The primary thumbnail lookup shape and uniqueness rule for archive members.
    UNIQUE (file_id, subpath),

    FOREIGN KEY (file_id)
        REFERENCES files(id)
        ON DELETE CASCADE
);

INSERT INTO thumbnails_new (file_id, subpath, thumbnail, mime)
SELECT file_id, NULL, thumbnail, mime
FROM thumbnails;

DROP TABLE thumbnails;
ALTER TABLE thumbnails_new RENAME TO thumbnails;

-- SQLite considers NULL values distinct in a UNIQUE constraint, so the partial
-- index separately enforces one physical-file thumbnail per file.
CREATE UNIQUE INDEX thumbnails_whole_file
ON thumbnails (file_id)
WHERE subpath IS NULL;
