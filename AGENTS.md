# AGENTS.md

This file applies to the entire repository. BAG is an early-stage, general-purpose
gallery implemented as a Rust workspace. Keep changes narrow and preserve the
existing separation between the shared protocol, media/filesystem support, server,
and browser client.

## Mandatory Working Agreement

1. Never use Git to commit code, create commits, amend commits, rebase, reset, or
   otherwise alter repository history. The user will review and commit changes
   manually. Read-only Git commands such as `git status`, `git diff`, `git log`,
   `git show`, and `git blame` are allowed for inspection.
2. Never modify code without explicit confirmation from the user. For every task,
   first inspect the relevant context, form a concrete plan, present that plan to
   the user, and ask them to confirm or amend it. Only edit code after confirmation.
   Do not treat a request to investigate, diagnose, explain, or review as permission
   to implement a fix. When scope changes materially during implementation, pause
   and obtain confirmation for the revised plan.
3. Ask questions and raise objections whenever requirements, assumptions, or
   consequences are uncertain. The user can make mistakes in both instructions and
   answers. Do not over-analyze dubious wording or silently choose the most pleasing
   interpretation; identify the uncertainty and ask.
4. This repository is in an isolated NixOS container. Installing software and using
   the environment are allowed. Be especially careful with deletion in this
   repository and its sibling directories: resolve and verify exact targets first,
   avoid broad recursive deletion, and ask the user if deletion scope or required
   permissions are uncertain.

Documentation-only edits are still edits: unless the user explicitly requested the
documentation change, apply the same plan-and-confirm rule.

## Repository Map

- `bag-lib`: Backend/frontend protocol and extended-path representation. Its Serde
  types are the cross-crate wire contract.
  - `action.rs`: remotely supplied client actions; currently only navigation.
  - `ui.rs`: render response schema (`Layout`, `Component`, galleries, media, etc.).
  - `path.rs`: URL-encoded path segments with comma-separated key/value arguments.
- `bag-fs`: Filesystem asset delivery and thumbnail generation.
  - `fs.rs`: Axum responses, ETags, byte ranges, and password-aware nested ZIP
    access using structured `bag_lib::path::Path` values. It currently buffers
    archive members and response bodies in memory.
  - `render.rs`: reusable conversion from an `ArchiveListing` into a protocol
    `Layout`; HTTP routing and redirects remain the caller's responsibility.
  - `thumb.rs`: image and video thumbnails encoded as WebP; video decoding uses
    FFmpeg libraries through `ffmpeg-next`.
- `bag-serve`: Axum/SQLite backend.
  - `main.rs`: CLI for `upgrade`, `rescan`, optional continuous `--watch`, and
    `serve` (default bind `0.0.0.0`, port `6102`).
  - `scan.rs`: filesystem indexing and Linux inotify watch handling.
  - `db.rs` and `migrations/`: SQLite setup and schema validation.
  - `serve.rs`: render, raw-file, and on-demand thumbnail handlers.
- `bag-web`: Leptos 0.8 client-side-rendered WASM application built with Trunk.
  - `main.rs`: backend selection/local storage, browser history, preloaded adjacent
    panels, touch swipe state, and spring animation.
  - `panel.rs`: rendering of the `bag-lib::ui` component protocol.
  - `style.css`: all application styling and responsive layout.
- `flake.nix` and `flake.lock`: reproducible Nix development shell. Inputs,
  toolchains, and the matching `wasm-bindgen-cli` are pinned.
- `doc/overview.md`: design notes. Treat it as intent, not guaranteed current
  behavior; compare it with routes and types in source.

Generated/local data must not be hand-edited or committed: `target/`,
`bag-web/dist/`, and SQLite files such as `bag-serve/db.sqlite*` are ignored.
`Cargo.lock` is tracked and should remain tracked for this application workspace.

## Architecture And Runtime Flow

1. `bag-serve rescan` indexes a configured filesystem root into SQLite. The empty
   relative path is the root record; stored file paths are relative to that root.
   Archives remain physical file records and are not descended during scanning.
2. The browser requests a structured render response. The server resolves the
   outer physical path through SQLite, then lists archive contents dynamically when
   the path contains an archive marker. It serializes `bag-lib` UI components or an
   action.
3. The Leptos client renders those components and preloads `Layout.left` and
   `Layout.right` as adjacent panels. Navigation updates browser history; horizontal
   swipe navigation replaces the current history entry.
4. Media is fetched separately from raw-file or thumbnail endpoints. Thumbnails are
   generated lazily and cached in the `thumbnails` SQLite table by outer physical
   file ID and a parameter-free archive subpath.

Changing a `bag-lib` wire type normally requires coordinated server serialization
and frontend rendering changes. Preserve Serde tagging and optional-field behavior
unless an API compatibility change is explicitly intended.

## Current HTTP And Path Contracts

The routes built by `bag-serve/src/serve.rs` are:

- `GET /v1/render/{extended_path}`: JSON `LayoutOrAction`.
- `GET /v1/raw/file/{structured_relative_path}`: raw physical or archive-member
  content, including ETag and single-range support.
- `GET /v1/raw/thumbnail/{structured_relative_path}`: cached or newly generated
  WebP thumbnail for the full source path.

The frontend appends `/render/...` and `/raw/...` to its configured backend string,
so the value entered in browser settings should include the `/v1` prefix (for
example, `http://host:6102/v1`). CORS is currently permissive.

Extended paths are slash-separated `bag_lib::path::Segment`s. A segment is encoded
as `name,key=value,key2=value2`; names, keys, and values use URL encoding. Directory
pagination is stored as `limit` and `offset` arguments on the last segment. Segment
arguments use a `BTreeMap`, so serialization is canonical and ordered by key. The
empty render path redirects by action to `file`; normal browsable render paths begin
with the `file` segment. Raw file and thumbnail routes omit that leading namespace.

A `:` segment means "open the preceding file as an archive"; it is URL-encoded as
`%3A`. Consequently, the top-level listing for `archive.zip` is
`archive.zip/%3A`, and nested archives add another marker, for example
`outer.zip/%3A/inner.zip/%3A`. ZIP passwords belong to the marker as its `pw`
argument, such as `%3A,pw=secret`, and each nested archive has an independent
marker/password. `FsHandler::handle`, `open_buffered`, and `archive_fetch` accept
structured `Path` values so these parameters survive segment-wise traversal.
`archive_fetch` returns either a directory listing or a file with its parent
listing; rendering and archive redirects are decided by `bag-serve`.

Thumbnail URLs preserve arguments only on `:` segments. The database lookup strips
all arguments and stores nested archive paths with the decoded `/:/` delimiter.
Archive access, including password validation, is checked before returning a cached
thumbnail. `bag-fs::Error::ArchivePassword` reports a sanitized path to the failing
archive, while `NotArchive` distinguishes an unsupported path or directory from a
recognized `.zip` file whose ZIP data is invalid.

`doc/overview.md` describes `/render`, `/action`, and `/asset` conceptually, but the
implementation currently uses the versioned routes above and has no action POST
route. Verify source before extending the documented protocol.

## Development Prerequisites

The crates use Rust edition 2024 and workspace resolver 3. Use the pinned Nix
development shell rather than relying on host tools:

```sh
nix develop
```

The shell provides Rust, Cargo, rustfmt, Clippy, rust-src, the
`wasm32-unknown-unknown` target, Trunk, the exact `wasm-bindgen-cli` version required
by `Cargo.lock`, SQLite, `pkg-config`, Clang/libclang, and FFmpeg development
libraries. `flake.nix` and `flake.lock` pin the Nix inputs, Rust toolchain, and
flake-local wasm-bindgen package. Update those pins deliberately and re-run all
native and browser build checks after doing so.

The flake intentionally has no database initialization shell hook and does not set
`DATABASE_URL`. Entering the development shell must not create, migrate, replace,
or otherwise modify a database.

SQLx queries are compile-time checked. `.envrc` sets:

```sh
DATABASE_URL=sqlite:$PWD/bag-serve/db.sqlite
```

Ensure `.envrc` is authorized and loaded through direnv (or pass an equivalent
variable explicitly), and ensure the referenced database already exists with the
current migration before compiling `bag-serve`. Do not assume `nix develop` loaded
`.envrc` or prepared the database.

Typical verification commands from the repository root, inside the Nix shell:

```sh
cargo fmt --all -- --check
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets --all-features -- -D warnings
nix flake check
```

Run Trunk from `bag-web/`; invoking it against `bag-web/index.html` from the
workspace root does not resolve the target crate correctly. This container may
export `NO_COLOR=1`, which Trunk 0.21 rejects as a boolean value. Remove that
variable for the invocation when necessary:

```sh
cd bag-web
env -u NO_COLOR trunk build --locked index.html
env -u NO_COLOR trunk serve index.html
```

Current baseline: the workspace builds, tests, and bundles with Trunk, and both
`cargo fmt --all -- --check` and Clippy with `-D warnings` pass.

The server CLI always requires `--root`, including for `upgrade`. A representative
local sequence is:

```sh
export DATABASE_URL="sqlite:$PWD/bag-serve/db.sqlite"
cargo run -p bag-serve -- --root /absolute/gallery/root upgrade
cargo run -p bag-serve -- --root /absolute/gallery/root rescan
RUST_LOG=info cargo run -p bag-serve -- --root /absolute/gallery/root serve
```

Do not run scans or server integration checks against valuable user media without
confirming the root. Prefer a dedicated temporary fixture tree and database.

## Change And Review Guidance

- For code changes, run rustfmt and Clippy and leave no formatting or Clippy
  problems. If a problem is unreasonable to fix or demonstrably preexisting, do
  not silently suppress it or expand the task into unrelated cleanup; notify the
  user and explain the exception.
- For review tasks, run the check-only formatting and Clippy commands to verify the
  reviewed state, but do not modify code to resolve their findings. If either check
  reports a problem, report it and ask the user for concrete reasons before
  treating it as an acceptable exception.
- Add focused behavioral tests for changed logic. Existing tests cover path
  serialization, archive traversal and encryption, thumbnail extraction/cache
  authorization, and scan behavior, but they are not comprehensive integration
  coverage.
- For server changes, check success and error status codes, SQLite effects, ETag/
  cache headers, range behavior, and path handling.
- For frontend changes, test desktop and mobile widths, direct URL loading,
  back/forward navigation, adjacent-panel preloading, touch gestures, settings, and
  image/video behavior as relevant. Verify the WASM app in a browser, not only with
  a native Cargo check.
- Keep potentially blocking filesystem, archive, image, and video work off async
  executor threads; the existing media code uses `spawn_blocking` for this reason.
- Migrations are append-only once shared. Add a new numbered migration rather than
  rewriting an applied migration unless the user explicitly confirms a development
  database reset strategy.
- Preserve inotify watch-map invariants documented in `scan.rs`; watcher changes are
  race-sensitive and deserve targeted temporary-filesystem tests.
- Preserve the critically damped relationship between `STIFFNESS` and `DAMPING` in
  the swipe code. Navigation direction is tied to the `prev | current | next` panel
  layout, so gesture changes need browser-level verification.

## Known Risks And Incomplete Areas

- `bag-fs/src/fs.rs` explicitly marks path sanitization and canonicalization as
  FIXME items. Raw paths are joined to the configured root. Treat traversal and
  symlink containment as security-sensitive and do not expose the server to
  untrusted networks without addressing the threat model.
- Archive members are intentionally not indexed in `files`; listings are generated
  dynamically. Large or deeply nested archives may therefore make listing and
  traversal expensive.
- Directory filtering/sorting controls, remote actions, and several protocol ideas
  in `doc/overview.md` are not implemented.
- Raw file reads and nested archive reads currently buffer requested content in
  memory; consider memory impact when reviewing large-media changes.
- Archive passwords are carried in URL path parameters. Although application error
  text and render-request logs omit them, URLs may still be retained by browsers,
  reverse proxies, or external access logs. Use TLS and treat those paths as
  credentials when changing logging, caching, or URL handling.
- The watcher is Linux/inotify-specific and documents hardlink, symlink, and event
  race assumptions in `scan.rs`.
- The browser client assumes Web APIs and local storage calls succeed in many paths;
  changes around storage, history, media playback, or DOM observers should account
  for browser errors and lifecycle cleanup.
