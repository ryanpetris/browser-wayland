//! `skills/browser-wayland/reference.md`, generated from the code so it can't drift: the route table
//! here, the JSON schemas of the API types, and the MCP tools with their input schemas. A test keeps
//! the checked-in file current.

use bw_core::{ControlMsg, InputMsg, WindowInfo};
use schemars::{JsonSchema, schema_for};

use crate::{apps::AppInfo, elements::Page, mcp::Mcp};

const ROUTES: &str = "\
| Method and path | Body or query | Result |
|---|---|---|
| `GET /api/windows` | | JSON array of **Window** |
| `GET /api/codecs` | | JSON array of `{codec, hardware}`: what this server encodes, in the order Auto prefers |
| `GET /api/applications` | | JSON array of **Application**: the installed launchers, for `launch` |
| `GET /api/applications/{id}/icon` | | the application's icon, SVG or PNG; `404` none |
| `GET /api/windows/{id}/icon` | | the window's icon (its own, else its launcher's), SVG or PNG; `404` none |
| `GET /api/files` | **FileQuery** query, required `path` | control token; **FileListing** |
| `PUT /api/files/{name}` | bytes; required `path` query | control token; streaming upload with collision suffix; `201` **SavedFile** |
| `GET /api/files/{name}` | required `path` query | control token; regular file attachment |
| `DELETE /api/files/{name}` | required `path` query | control token; nonrecursive unlink; `204` |
| `POST /api/files` | **FileAction** | control token; mkdir or rename without replacement; `201` **SavedFile** |
| `PUT /api/drop/{batch}/{name}` | the file's bytes | staged in batch `batch` (a random id of the page's) for a drag or a paste onto the desktop, where the application picks the folder; the transfer folder is for uploads; `201` with `{\"name\": \"…\"}` |
| `GET /api/notifications` | | JSON array of **Notification**: what applications reported and the viewers show |
| `POST /api/notifications/{id}` | `{\"action\": \"default\" \\| \"<key>\"}`, or `{}` to dismiss | click, invoke an action of, or dismiss a notification; `202`, `404` |
| `GET /api/notifications/{id}/icon` | | the notification's picture (the application's, else its launcher's); `404` none |
| `GET /api/windows/{id}/elements` | | **Elements**; `501` without `--elements`, `503` tree unreadable, `404` unknown window |
| `GET /api/windows/{id}/snapshot.png` | one optional `width`, `height`, or `percentage`; default native | PNG of the window; `404`, `429` another snapshot in flight, `500` render failed, `503` |
| `GET /api/screenshot.png` | same sizing as window snapshots; default native | PNG of the whole output; `429`, `500`, `503` as for a window |
| `POST /api/control` | **Control** | `202`; fire-and-forget; `404` unknown application (`launch`); `503` compositor gone |
| `POST /api/input` | **Input** | `202`, with `{\"warning\": …}` when a click aims past the desktop's edge at an X11 window (Xwayland pins it to the edge); `404` unknown window; `503` compositor gone |
| `GET /api/clipboard` | | what an application last copied: `text/plain`, `image/png`, or `text/uri-list` (control token required for file lists); `204` before any |
| `PUT /api/clipboard` | UTF-8 text body, a PNG with `Content-Type: image/png`, or `file://` URIs with `text/uri-list` | becomes the desktop clipboard; `202`; `413` over 1 MiB (text) or 16 MiB (PNG) |
| `POST /api/clipboard/files` | `{\"names\": [...]}` from the transfer folder, or with `\"batch\"` from that staged batch | those files become the desktop clipboard, as a file manager's copy; `202` |
| `GET /api/clipboard/files/{index}` | | control token; the `index`th file on the desktop clipboard, as an attachment; `404` |
| `POST /api/token/rotate` | | `{\"token\": …, \"viewer_token\": …}`: new tokens replace both at once (files, viewers, API); the server prints the new URLs |
| `POST /mcp` | MCP Streamable HTTP | the tools below |
| `GET /skill/SKILL.md`, `GET /skill/reference.md` | no token needed | this documentation |
";

fn schema<T: JsonSchema>() -> String {
    serde_json::to_string_pretty(&schema_for!(T)).unwrap()
}

pub fn markdown() -> String {
    let mut out = String::new();
    out.push_str("# browser-wayland API and MCP reference\n\n");
    out.push_str("Generated from the code (`UPDATE_REFERENCE=1 cargo test -p bw-server reference`); do not edit.\n\n");
    out.push_str("## HTTP API\n\nEvery `/api` request carries `Authorization: Bearer <token>`; `401` (empty body) otherwise. The\nviewer token (the server prints it as \"view only\") reads: the acting routes and tools answer `403`\n`read-only token` to it. The\nstatuses in the table come with a JSON body `{\"error\": \"...\"}`. A request body the server can't read is\nrejected before that with a plain-text message: `400` invalid JSON, `415` missing\n`Content-Type: application/json`, `422` wrong shape. Coordinates are logical pixels.\n\n");
    out.push_str(ROUTES);
    for (name, s) in [("Window", schema::<WindowInfo>()), ("Application", schema::<AppInfo>()), ("Notification", schema::<crate::notify::Notification>()), ("FileQuery", schema::<crate::files::FileQuery>()), ("FileListing", schema::<crate::files::FileListing>()), ("FileAction", schema::<crate::files::FileAction>()), ("SavedFile", schema::<crate::files::SavedFile>()), ("Control", schema::<ControlMsg>()), ("Input", schema::<InputMsg>()), ("Elements", schema::<Page>())] {
        out.push_str(&format!("\n## {name}\n\n```json\n{s}\n```\n"));
    }
    out.push_str("\n## MCP tools\n\nStreamable HTTP at `/mcp`, same bearer token. Failures come back as tool errors with the same text as the API.\n");
    for tool in Mcp::tool_router().list_all() {
        out.push_str(&format!("\n### `{}`\n\n{}\n\n```json\n{}\n```\n", tool.name, tool.description.as_deref().unwrap_or(""), serde_json::to_string_pretty(&*tool.input_schema).unwrap()));
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn reference_is_current() {
        let want = super::markdown();
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../skills/browser-wayland/reference.md");
        if std::env::var_os("UPDATE_REFERENCE").is_some() {
            std::fs::write(path, &want).unwrap();
        }
        assert!(std::fs::read_to_string(path).unwrap() == want, "skills/browser-wayland/reference.md is stale: UPDATE_REFERENCE=1 cargo test -p bw-server reference");
    }
}
