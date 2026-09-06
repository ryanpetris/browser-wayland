//! MCP server (Streamable HTTP at `/mcp`, bearer token like the rest of the API). Every tool is a thin
//! wrapper over the `App` operations in `api.rs`; the skill documents are served as instructions and
//! resources so a client gets the manual with the connection.

use std::sync::Arc;

use base64::Engine;
use elsewhere_core::{Button, ControlMsg, ControlOp, InputMsg};
use axum::http::request::Parts;
use rmcp::{
    ErrorData as McpError, RoleServer, ServerHandler,
    handler::server::{router::tool::ToolRouter, tool::Extension, wrapper::Parameters},
    model::*,
    service::RequestContext,
    tool, tool_handler, tool_router,
};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::{App, Key, api::{self, ApiError}};

pub const SKILL: &str = include_str!("../../../skills/elsewhere/SKILL.md");
pub const REFERENCE: &str = include_str!("../../../skills/elsewhere/reference.md");

// rmcp stores tool arguments in a JSON map, which otherwise discards repeated keys.
fn duplicate_arguments<T: serde::de::DeserializeOwned>(raw: &str) -> bool {
    serde_json::from_str::<T>(raw).is_err()
        && serde_json::from_str::<serde_json::Value>(raw).is_ok_and(|value| serde_json::from_value::<T>(value).is_ok())
}

// MCP uses single requests with object parameters; revisit this guard if rmcp adds batches.
pub async fn validate_capture_body(request: axum::extract::Request, next: axum::middleware::Next) -> axum::response::Response {
    use axum::response::IntoResponse;
    if request.method() != axum::http::Method::POST { return next.run(request).await; }
    let (parts, body) = request.into_parts();
    let bytes = match axum::body::to_bytes(body, 4 << 20).await {
        Ok(bytes) => bytes,
        Err(_) => return ApiError::TooLarge.into_response(),
    };
    #[derive(Deserialize)]
    struct Call { method: Option<String>, params: Option<Args> }
    #[derive(Deserialize)]
    struct Args { name: Option<String>, arguments: Option<Box<serde_json::value::RawValue>> }
    let call = match serde_json::from_slice::<Call>(&bytes) {
        Ok(call) => call,
        Err(_) => return ApiError::InvalidSize("duplicate or malformed MCP request fields").into_response(),
    };
    if let Call { method, params: Some(args) } = call
        && method.as_deref() == Some("tools/call")
        && let Some(raw) = args.arguments
    {
        let duplicate = match args.name.as_deref() {
            Some("screenshot") => duplicate_arguments::<ScreenshotArgs>(raw.get()),
            Some("snapshot") => duplicate_arguments::<SnapshotArgs>(raw.get()),
            _ => false,
        };
        if duplicate { return ApiError::InvalidSize("duplicate screenshot sizing arguments").into_response(); }
    }
    next.run(axum::extract::Request::from_parts(parts, axum::body::Body::from(bytes))).await
}

#[derive(Clone)]
pub struct Mcp {
    app: Arc<App>,
    tool_router: ToolRouter<Self>,
}

#[derive(Deserialize, JsonSchema)]
pub struct WindowArg {
    /// Window id from `windows`.
    pub window: u64,
}

type ScreenshotArgs = elsewhere_core::SnapshotSizing;

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SnapshotArgs {
    /// Window id from `windows`.
    pub window: u64,
    #[serde(flatten)]
    pub sizing: elsewhere_core::SnapshotSizing,
}

#[cfg(test)]
mod sizing_tests {
    use super::*;

    #[test]
    fn window_arguments_reject_unknown_and_repeated_sizing() {
        assert!(serde_json::from_str::<SnapshotArgs>(r#"{"window":1,"width":64}"#).is_ok());
        for raw in [r#"{"window":1,"widht":64}"#, r#"{"window":1,"width":32,"width":64}"#, r#"{"window":1,"percentage":null,"percentage":50}"#] {
            assert!(serde_json::from_str::<SnapshotArgs>(raw).is_err(), "{raw}");
        }
    }
}

#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum WindowOp {
    Activate,
    Close,
    Minimize,
    Unminimize,
    Maximize,
    Unmaximize,
    Fullscreen,
    Unfullscreen,
}

#[derive(Deserialize, JsonSchema)]
pub struct WindowControlArgs {
    /// Window id from `windows`.
    pub window: u64,
    pub op: WindowOp,
}

#[derive(Deserialize, JsonSchema)]
pub struct MoveWindowArgs {
    pub window: u64,
    /// New position of the window's geometry in output logical pixels.
    pub x: i32,
    pub y: i32,
}

#[derive(Deserialize, JsonSchema)]
pub struct ResizeWindowArgs {
    pub window: u64,
    /// New size of the window's geometry in logical pixels.
    pub w: i32,
    pub h: i32,
}

#[derive(Deserialize, JsonSchema)]
pub struct LaunchArgs {
    /// An `id` from `applications`.
    pub app: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct SpawnArgs {
    /// Shell command, run with `sh -c` as a client of this desktop.
    pub cmd: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct PointArgs {
    pub x: f64,
    pub y: f64,
    /// With a window id, `x`/`y` are relative to that window (as element rectangles are); else output pixels.
    #[serde(default)]
    pub window: Option<u64>,
}

#[derive(Deserialize, JsonSchema)]
pub struct ClickArgs {
    pub x: f64,
    pub y: f64,
    /// With a window id, `x`/`y` are relative to that window (as element rectangles are); else output pixels.
    #[serde(default)]
    pub window: Option<u64>,
    /// left (default), right or middle
    #[serde(default)]
    pub button: Button,
    /// 1 (default) to 3 clicks
    #[serde(default)]
    pub count: Option<u32>,
}

#[derive(Deserialize, JsonSchema)]
pub struct ButtonArgs {
    pub button: Button,
    pub pressed: bool,
}

#[derive(Deserialize, JsonSchema)]
pub struct ScrollArgs {
    /// Wheel lines; positive scrolls right.
    #[serde(default)]
    pub dx: f64,
    /// Wheel lines; positive scrolls down.
    #[serde(default)]
    pub dy: f64,
}

#[derive(Deserialize, JsonSchema)]
pub struct KeyArgs {
    /// A chord, `+`-separated: `ctrl+shift+t`, `alt+F4`, `Return`, `Escape`, `Down`, `Prior`, `F5`.
    pub keys: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct TextArgs {
    /// Typed through the keyboard layout into the focused field; `\n` is Return.
    pub text: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct ClipboardWriteArgs {
    /// Becomes the desktop clipboard (text only, up to 1 MiB).
    pub text: String,
}

type ToolResult = Result<CallToolResult, McpError>;

fn json(value: impl serde::Serialize) -> ToolResult {
    Ok(CallToolResult::success(vec![ContentBlock::text(serde_json::to_string(&value).map_err(|e| McpError::internal_error(e.to_string(), None))?)]))
}

fn done(result: Result<(), ApiError>) -> ToolResult {
    match result {
        Ok(()) => Ok(CallToolResult::success(vec![ContentBlock::text("ok")])),
        Err(e) => Ok(CallToolResult::error(vec![ContentBlock::text(e.to_string())])),
    }
}

impl Mcp {
    pub fn new(app: Arc<App>) -> Self {
        Mcp { app, tool_router: Self::tool_router() }
    }

    /// The tools that act need the control token; the bearer middleware left which one it was in the request.
    fn acting(&self, parts: &Parts) -> Result<(), ApiError> {
        if parts.extensions.get::<Key>() == Some(&Key::Control) { Ok(()) } else { Err(ApiError::Forbidden) }
    }

    fn control(&self, parts: &Parts, id: u64, op: ControlOp) -> ToolResult {
        done(self.acting(parts).and_then(|()| self.app.control(ControlMsg { id, op })))
    }

    fn input(&self, parts: &Parts, msg: InputMsg) -> ToolResult {
        done(self.acting(parts).and_then(|()| self.app.input(msg)))
    }

    async fn png(&self, id: Option<u64>, sizing: elsewhere_core::SnapshotSizing) -> ToolResult {
        match self.app.snapshot(id, sizing).await {
            Ok(png) => Ok(CallToolResult::success(vec![ContentBlock::image(base64::engine::general_purpose::STANDARD.encode(png), "image/png")])),
            Err(e) => Ok(CallToolResult::error(vec![ContentBlock::text(e.to_string())])),
        }
    }
}

#[tool_router(vis = "pub(crate)")]
impl Mcp {
    #[tool(description = "The windows on the desktop: id, title, app_id, icon (name the client set; its picture is at GET /api/windows/{id}/icon), content (video/game/photo when the client says so), pid, geometry x y w h (logical px), stacking z, maximized/fullscreen/minimized/focused, updated_ms (last redraw), popups (open menus, relative to x y).")]
    fn windows(&self) -> ToolResult {
        json(self.app.windows())
    }

    #[tool(description = "The UI elements of a window (buttons, links, text fields, menu items, tabs, ...): role, name, and x y w h relative to the window's own x y. `level` full means the list is complete; none/app/frame mean the application exposes nothing (use a snapshot).")]
    async fn elements(&self, Parameters(WindowArg { window }): Parameters<WindowArg>) -> ToolResult {
        match self.app.elements(window).await {
            Ok(page) => json(page),
            Err(e) => Ok(CallToolResult::error(vec![ContentBlock::text(e.to_string())])),
        }
    }

    #[tool(description = "PNG of the whole output (panels included, pointer excluded), native size by default. Supply at most one of width, height, or percentage.")]
    async fn screenshot(&self, Parameters(sizing): Parameters<ScreenshotArgs>) -> ToolResult {
        self.png(None, sizing).await
    }

    #[tool(description = "PNG of one window's own buffers (works for covered and minimized windows), popups included. Native size by default; supply at most one of width, height, or percentage.")]
    async fn snapshot(&self, Parameters(SnapshotArgs { window, sizing }): Parameters<SnapshotArgs>) -> ToolResult {
        self.png(Some(window), sizing).await
    }

    #[tool(description = "Change a window's state: activate (raise, focus, restore), close, minimize, unminimize, maximize, unmaximize, fullscreen, unfullscreen. Fire-and-forget; check `windows` afterwards.")]
    fn window_control(&self, Extension(parts): Extension<Parts>, Parameters(WindowControlArgs { window, op }): Parameters<WindowControlArgs>) -> ToolResult {
        let op = match op {
            WindowOp::Activate => ControlOp::Activate,
            WindowOp::Close => ControlOp::Close,
            WindowOp::Minimize => ControlOp::Minimize,
            WindowOp::Unminimize => ControlOp::Unminimize,
            WindowOp::Maximize => ControlOp::Maximize,
            WindowOp::Unmaximize => ControlOp::Unmaximize,
            WindowOp::Fullscreen => ControlOp::Fullscreen,
            WindowOp::Unfullscreen => ControlOp::Unfullscreen,
        };
        self.control(&parts, window, op)
    }

    #[tool(description = "Move a floating window's geometry to x y (output logical px).")]
    fn move_window(&self, Extension(parts): Extension<Parts>, Parameters(MoveWindowArgs { window, x, y }): Parameters<MoveWindowArgs>) -> ToolResult {
        self.control(&parts, window, ControlOp::Move { x, y })
    }

    #[tool(description = "Resize a floating window's geometry to w h (logical px).")]
    fn resize_window(&self, Extension(parts): Extension<Parts>, Parameters(ResizeWindowArgs { window, w, h }): Parameters<ResizeWindowArgs>) -> ToolResult {
        self.control(&parts, window, ControlOp::Resize { w, h })
    }

    #[tool(description = "Start a program as a client of this desktop (`sh -c cmd`). Its window appears in `windows` after a moment.")]
    fn spawn(&self, Extension(parts): Extension<Parts>, Parameters(SpawnArgs { cmd }): Parameters<SpawnArgs>) -> ToolResult {
        self.control(&parts, 0, ControlOp::Spawn { cmd })
    }

    #[tool(description = "The applications installed on the desktop (its .desktop launchers): id, name, comment, categories. `launch` starts one.")]
    async fn applications(&self) -> ToolResult {
        json(self.app.applications().await)
    }

    #[tool(description = "Control token required. List a directory by absolute path, @home, or @transfer. Returns FileListing with entries and pagination; hidden, sort, desc, offset, and limit match GET /api/files. Download entries with GET /api/files/{name}?path=... .")]
    async fn files(&self, Extension(parts): Extension<Parts>, Parameters(query): Parameters<crate::files::FileQuery>) -> ToolResult {
        if let Err(e) = self.acting(&parts) { return done(Err(e)); }
        match self.app.browse_files(query).await {
            Ok(list) => json(list),
            Err(e) => Ok(CallToolResult::error(vec![ContentBlock::text(e.to_string())])),
        }
    }

    #[tool(description = "The desktop notifications currently shown (id, app, summary, body, actions, timeout_ms): what applications reported. POST /api/notifications/{id} with {\"action\": key} acts on one, {} dismisses it.")]
    fn notifications(&self) -> ToolResult {
        json(self.app.notifications())
    }

    #[tool(description = "Start an installed application by its id from `applications`, as a click in the menu would. Its window appears in `windows` after a moment.")]
    fn launch(&self, Extension(parts): Extension<Parts>, Parameters(LaunchArgs { app }): Parameters<LaunchArgs>) -> ToolResult {
        self.control(&parts, 0, ControlOp::Launch { app })
    }

    #[tool(description = "Move the pointer there and click. With `window`, x y are relative to that window, e.g. the centre of an element's rectangle.")]
    fn click(&self, Extension(parts): Extension<Parts>, Parameters(ClickArgs { x, y, window, button, count }): Parameters<ClickArgs>) -> ToolResult {
        match self.acting(&parts).and_then(|()| self.app.input(InputMsg::Click { x, y, window, button, count })) {
            Ok(()) => Ok(CallToolResult::success(vec![ContentBlock::text(match window.and_then(|w| self.app.x11_edge_warning(w, x, y)) {
                Some(w) => format!("ok; {w}"),
                None => "ok".into(),
            })])),
            Err(e) => done(Err(e)),
        }
    }

    #[tool(description = "Move the pointer without clicking (hover, or the middle of a drag).")]
    fn move_pointer(&self, Extension(parts): Extension<Parts>, Parameters(PointArgs { x, y, window }): Parameters<PointArgs>) -> ToolResult {
        self.input(&parts, InputMsg::Move { x, y, window })
    }

    #[tool(description = "Press or release a pointer button where the pointer is (for drags: press, move_pointer, release).")]
    fn button(&self, Extension(parts): Extension<Parts>, Parameters(ButtonArgs { button, pressed }): Parameters<ButtonArgs>) -> ToolResult {
        self.input(&parts, InputMsg::Button { button, pressed })
    }

    #[tool(description = "Scroll the wheel under the pointer by lines; positive dy scrolls down.")]
    fn scroll(&self, Extension(parts): Extension<Parts>, Parameters(ScrollArgs { dx, dy }): Parameters<ScrollArgs>) -> ToolResult {
        self.input(&parts, InputMsg::Scroll { dx, dy })
    }

    #[tool(description = "Press a key chord and release it: `ctrl+s`, `ctrl+shift+t`, `alt+F4`, `Return`, `Escape`, `Tab`, `Down`, `Prior`, `F5`. Goes to the focused window.")]
    fn key(&self, Extension(parts): Extension<Parts>, Parameters(KeyArgs { keys }): Parameters<KeyArgs>) -> ToolResult {
        self.input(&parts, InputMsg::Key { keys })
    }

    #[tool(description = "The last text a desktop application copied to the clipboard (empty if none yet). An image says so; copied files require a control token and are their file:// URIs; GET /api/clipboard returns the bytes.")]
    fn clipboard_read(&self, Extension(parts): Extension<Parts>) -> ToolResult {
        let text = match self.app.clipboard() {
            Some((mime, _)) if mime == api::URI_LIST && self.acting(&parts).is_err() => return done(Err(ApiError::Forbidden)),
            Some((mime, data)) if mime == api::PNG => format!("[{} bytes of {mime}; GET /api/clipboard returns them]", data.len()),
            Some((_, data)) => String::from_utf8_lossy(&data).into_owned(),
            None => String::new(),
        };
        Ok(CallToolResult::success(vec![ContentBlock::text(text)]))
    }

    #[tool(description = "Put text on the desktop clipboard, for pasting into an application (images go through PUT /api/clipboard).")]
    fn clipboard_write(&self, Extension(parts): Extension<Parts>, Parameters(ClipboardWriteArgs { text }): Parameters<ClipboardWriteArgs>) -> ToolResult {
        done(self.acting(&parts).and_then(|()| self.app.set_clipboard(api::TEXT, text.into())))
    }

    #[tool(name = "type", description = "Type text into the focused field through the keyboard layout (click it first). `\\n` is Return.")]
    fn type_text(&self, Extension(parts): Extension<Parts>, Parameters(TextArgs { text }): Parameters<TextArgs>) -> ToolResult {
        self.input(&parts, InputMsg::Text { text })
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for Mcp {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().enable_resources().build())
            .with_server_info(Implementation::new("elsewhere", self.app.version))
            .with_instructions(SKILL)
    }

    async fn list_resources(&self, _: Option<PaginatedRequestParams>, _: RequestContext<RoleServer>) -> Result<ListResourcesResult, McpError> {
        let doc = |uri, name| {
            let mut r = Resource::new(uri, name);
            r.mime_type = Some("text/markdown".into());
            r
        };
        Ok(ListResourcesResult::with_all_items(vec![doc("skill://elsewhere/SKILL.md", "SKILL.md"), doc("skill://elsewhere/reference.md", "reference.md")]))
    }

    async fn read_resource(&self, request: ReadResourceRequestParams, _: RequestContext<RoleServer>) -> Result<ReadResourceResponse, McpError> {
        let text = match request.uri.as_str() {
            "skill://elsewhere/SKILL.md" => SKILL,
            "skill://elsewhere/reference.md" => REFERENCE,
            _ => return Err(McpError::resource_not_found(request.uri.clone(), None)),
        };
        Ok(ReadResourceResponse::Complete(ReadResourceResult::new(vec![ResourceContents::text(text, request.uri).with_mime_type("text/markdown")])))
    }
}
