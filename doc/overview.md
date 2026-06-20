# Overview of BAG

## Request types

There are three types of requests:

- Render, which returns a structured response that can be rendered into a page. This includes:
  - Gallery lists, search page & results
  - Individual gallery items, their metadata and related actions
  - Gallery-wide metadata, dashboards, etc.
  Render requests are indexed by a extended path (so each path segments can have extra string parameters besides the standard path segments). It's still structured as a path instead of a arbitrary JSON object (although they are isomorphic) is because we want to store it in frontend URL history.
- Remote Action, which are actions that are sent to the server. The server may response another action, which will be executed subsequently. This can be used to support:
  - Gallery item actions, such as marking, tagging, archiving
  - Gallery-wide actions, such as rescanning the filesystem
- Asset, which are expected to return static assets with standard HTTP responses. These requests are expected to be used to serve the final displayed images, videos, etc.
  Note that these may not be standard filesystem paths. For example, BAG supports direct indexing into compressed archives.
  However we recommend just use a filesystem path with extended path to avoid another database lookup. The default asset request handler does it this way.

These requests are sent into the following paths in the backend:
- GET `/render/{encoded_path}`. The encoding is the same as the frontend path encoding.
- POST `/action`. The body of the remote action is passed as the body of the POST.
- GET `/asset/{encoded_path}`. The path is given as-is

## Render response schema

## Action schema
