//! `skills/browser-wayland/reference.md`, generated from the code so it can't drift: the route table
//! here, the JSON schemas of the API types, and the MCP tools with their input schemas. A test keeps
//! the checked-in file current.

use bw_core::{ControlMsg, InputMsg, WindowInfo};
use schemars::{JsonSchema, schema_for};

use crate::{elements::Page, mcp::Mcp};

const ROUTES: &str = "\
| Method and path | Body or query | Result |
|---|---|---|
| `GET /api/windows` | | JSON array of **Window** |
| `GET /api/windows/{id}/elements` | | **Elements**; `501` without `--elements`, `503` tree unreadable, `404` unknown window |
| `GET /api/windows/{id}/snapshot.png?scale=` | `scale` 0.05–2, default 1 | PNG of the window; `404`, `429` another snapshot in flight, `500` render failed, `503` |
| `GET /api/screenshot.png?scale=` | `scale` 0.05–2, default 1 | PNG of the whole output; `429`, `500`, `503` as for a window |
| `POST /api/control` | **Control** | `202`; fire-and-forget; `503` compositor gone |
| `POST /api/input` | **Input** | `202`; `404` unknown window; `503` compositor gone |
| `GET /api/clipboard` | | the last text an application copied, `text/plain`; `204` before any |
| `PUT /api/clipboard` | UTF-8 text body | becomes the desktop clipboard; `202`; `413` over 1 MiB |
| `POST /api/token/rotate` | | `{\"token\": …}`: a new token replaces the old one at once (file, viewers, API); the server prints the new URLs |
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
    out.push_str("## HTTP API\n\nEvery `/api` request carries `Authorization: Bearer <token>`; `401` (empty body) otherwise. The\nstatuses in the table come with a JSON body `{\"error\": \"...\"}`. A request body the server can't read is\nrejected before that with a plain-text message: `400` invalid JSON, `415` missing\n`Content-Type: application/json`, `422` wrong shape. Coordinates are logical pixels.\n\n");
    out.push_str(ROUTES);
    for (name, s) in [("Window", schema::<WindowInfo>()), ("Control", schema::<ControlMsg>()), ("Input", schema::<InputMsg>()), ("Elements", schema::<Page>())] {
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
