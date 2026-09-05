//! HTTPS + WebSocket front end: serves the viewer page, authenticates the WebSocket in-band and the
//! HTTP API with a bearer token (never a cookie), streams encoded video out (one encoder per viewer)
//! and turns the controlling viewer's input into `Command`s. Two tokens: the control token acts, the
//! viewer token only looks.

mod api;
mod apps;
mod elements;
mod mcp;
pub mod files;
mod notify;
mod protocol;
#[cfg(test)]
mod reference;
mod ws;

use std::{
    collections::HashMap,
    fs, io,
    net::SocketAddr,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, RwLock},
};

use anyhow::{Context, Result};
use api::ApiError;
use axum::{
    Extension, Json, Router,
    extract::{Path as UrlPath, Query, Request, State, ws::WebSocketUpgrade},
    http::{HeaderMap, StatusCode, header},
    middleware::{self, Next},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
};
use bw_core::{Bytes, Codec, Command, ControlMsg, Event, FrameSink, InputMsg, OutputGeometry, StreamControl, StreamMsg, WindowInfo};
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager};
use tokio::sync::mpsc;

/// `None` = automatic: the first of the available codecs (best first) the browser decodes in hardware, else at all.
pub type CodecPolicy = Option<Codec>;
/// Makes an encoder for one viewer or window stream: the sink the compositor feeds and a control
/// handle that must not keep the pipeline alive (the stream ends when the compositor drops the sink).
pub type SinkFactory = Box<dyn Fn(mpsc::Sender<StreamMsg>) -> Result<(Box<dyn FrameSink>, Box<dyn StreamControl>)> + Send + Sync>;

/// Which token a request came with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    /// Acts: input, windows, programs, the clipboard; may control the desktop.
    Control,
    /// Looks: the window list, elements, snapshots, the clipboard's text, the video.
    Viewer,
}

pub struct Config {
    pub listen: SocketAddr,
    pub tls: bool,
    pub codec: CodecPolicy,
    /// What the encoder side can produce (`bw_stream::codecs`), best first, and whether on the CPU.
    pub codecs: Vec<Codec>,
    pub software: bool,
    /// `--bitrate`: what a viewer on Auto starts at and never exceeds.
    pub bitrate_kbps: u32,
    /// The compositor's frame clock, which every resize keeps.
    pub refresh_mhz: i32,
    /// Where `cert.pem`, `key.pem` and `token` live.
    pub data_dir: PathBuf,
    /// Serve /api/windows/{id}/elements (see `elements.rs`).
    pub elements: bool,
    /// Where dropped files land and downloads come from (`files.rs`).
    pub files_dir: PathBuf,
    /// Reported to MCP clients.
    pub version: &'static str,
    /// One encoder per viewer and per window stream.
    pub sinks: SinkFactory,
    /// Where the browser's microphone packets (Opus) go to be played into the desktop's virtual source;
    /// `None` without a microphone.
    pub mic: Option<mpsc::Sender<Bytes>>,
}

impl Config {
    pub fn default_data_dir() -> Result<PathBuf> {
        let base = match std::env::var_os("XDG_CONFIG_HOME") {
            Some(d) => PathBuf::from(d),
            None => PathBuf::from(std::env::var_os("HOME").context("neither XDG_CONFIG_HOME nor HOME is set")?).join(".config"),
        };
        Ok(base.join("browser-wayland"))
    }
}

pub struct App {
    /// The control token and the viewer token.
    tokens: RwLock<(String, String)>,
    data_dir: PathBuf,
    commands: calloop::channel::Sender<Command>,
    policy: CodecPolicy,
    codecs: Vec<Codec>,
    software: bool,
    bitrate_kbps: u32,
    viewers: Mutex<Viewers>,
    sinks: SinkFactory,
    mic: Option<mpsc::Sender<Bytes>>,
    /// Event senders of the window-stream sessions (cursor, clipboard, window list go to them too).
    window_viewers: Mutex<HashMap<u64, mpsc::Sender<Bytes>>>,
    snapshot_lock: tokio::sync::Semaphore,
    /// Open desktop notifications by id (`notify.rs`), the next id, and the bus we serve them on.
    notifications: Mutex<HashMap<u32, notify::Open>>,
    next_notification: std::sync::atomic::AtomicU32,
    notify_bus: std::sync::OnceLock<zbus::Connection>,
    files_dir: PathBuf,
    elements: bool,
    version: &'static str,
    tls: bool,
    port: u16,
}

/// The connected viewers and what they all see. One of them, the controller, drives the pointer and
/// keyboard and sizes the output; the others watch the same desktop scaled to their own window.
pub(crate) struct Viewers {
    sessions: HashMap<u64, ViewerSession>,
    controller: Option<u64>,
    /// The output as the controller last sized it.
    output: OutputGeometry,
    /// Last cursor message, replayed to a new viewer.
    cursor: Option<Bytes>,
    /// Whether a client currently holds a pointer lock, replayed to a new viewer.
    locked: bool,
    /// Last WINDOWS message, replayed to a new viewer, and the list it encodes (the API's view).
    windows: Option<Bytes>,
    window_list: Vec<WindowInfo>,
    /// The last clipboard contents (mime, bytes), from an application, the browser or the API (served on the API; not replayed to a viewer).
    clipboard: Option<(String, Bytes)>,
    next_id: u64,
}

impl Default for Viewers {
    fn default() -> Self {
        Viewers { sessions: HashMap::new(), controller: None, output: bw_core::INITIAL_OUTPUT, cursor: None, locked: false, windows: None, window_list: Vec::new(), clipboard: None, next_id: 1 }
    }
}

pub(crate) struct ViewerSession {
    key: Key,
    /// State messages (cursor, windows, clipboard, role) and audio, each with a small queue of its own.
    events: mpsc::Sender<Bytes>,
    audio: mpsc::Sender<Bytes>,
    audio_seq: u16,
    /// The viewer's stage in device pixels, from its last Resize.
    size: Option<OutputGeometry>,
    /// Its encoder: codec, size, quality and keyframes.
    control: Box<dyn StreamControl>,
    /// What the browser decodes (the `Hello` masks), what it asked for, and what it got.
    hw: u8,
    sw: u8,
    want_codec: Option<Codec>,
    codec: Codec,
    preset: protocol::Preset,
    quality: bw_core::Quality,
}

/// `audio_rx` carries the clients' Opus packets, for every viewer.
pub async fn run(cfg: Config, commands: calloop::channel::Sender<Command>, audio_rx: mpsc::Receiver<StreamMsg>, events_rx: mpsc::UnboundedReceiver<Event>) -> Result<()> {
    fs::create_dir_all(&cfg.data_dir)?;
    let token = load_or_create(&cfg.data_dir.join("token"), || Ok(random_hex(32)))?;
    let viewer_token = load_or_create(&cfg.data_dir.join("viewer-token"), || Ok(random_hex(32)))?;
    let app = Arc::new(App {
        tokens: RwLock::new((token, viewer_token)),
        data_dir: cfg.data_dir.clone(),
        commands,
        policy: cfg.codec,
        codecs: cfg.codecs,
        software: cfg.software,
        bitrate_kbps: cfg.bitrate_kbps,
        viewers: Mutex::new(Viewers { output: bw_core::OutputGeometry { refresh_mhz: cfg.refresh_mhz, ..bw_core::INITIAL_OUTPUT }, ..Default::default() }),
        sinks: cfg.sinks,
        mic: cfg.mic,
        window_viewers: Mutex::default(),
        snapshot_lock: tokio::sync::Semaphore::new(1),
        notifications: Mutex::default(),
        next_notification: std::sync::atomic::AtomicU32::new(1),
        notify_bus: std::sync::OnceLock::new(),
        files_dir: cfg.files_dir,
        elements: cfg.elements,
        version: cfg.version,
        tls: cfg.tls,
        port: cfg.listen.port(),
    });
    tokio::spawn(ws::distribute_audio(app.clone(), audio_rx));
    tokio::spawn(ws::forward_events(app.clone(), events_rx));
    tokio::spawn(notify::serve(app.clone()));

    let router = Router::new()
        .route("/", get(index))
        .route("/app.js", get(|| async { asset("text/javascript", include_str!("../../../web/dist/app.js")) }))
        .route("/app.css", get(|| async { asset("text/css", include_str!("../../../web/dist/app.css")) }))
        .route("/ws", get(websocket))
        .route("/ws/window/{id}", get(window_websocket))
        .merge(
            Router::new()
                .route("/api/windows", get(api_windows))
                .route("/api/codecs", get(api_codecs))
                .route("/api/applications", get(api_applications))
                .route("/api/applications/{id}/icon", get(api_application_icon))
                .route("/api/control", post(api_control))
                .route("/api/input", post(api_input))
                .route("/api/windows/{id}/snapshot.png", get(api_window_snapshot))
                .route("/api/screenshot.png", get(api_screenshot))
                .route("/api/windows/{id}/elements", get(api_window_elements))
                .route("/api/windows/{id}/icon", get(api_window_icon))
                .route("/api/files", get(api_files))
                .route("/api/files/{name}", get(api_file).put(api_put_file).delete(api_delete_file))
                .route("/api/notifications", get(api_notifications))
                .route("/api/notifications/{id}", post(api_notification_action))
                .route("/api/notifications/{id}/icon", get(api_notification_icon))
                .route("/api/token/rotate", post(api_token_rotate))
                .route("/api/clipboard", get(api_clipboard).put(api_set_clipboard))
                .route("/api/clipboard/files", post(api_clipboard_files))
                .route("/api/clipboard/files/{index}", get(api_clipboard_file))
                .nest_service("/mcp", mcp_service(app.clone()))
                .layer(middleware::from_fn_with_state(app.clone(), bearer)),
        )
        .route("/skill/SKILL.md", get(|| async { markdown(mcp::SKILL) }))
        .route("/skill/reference.md", get(|| async { markdown(mcp::REFERENCE) }))
        .with_state(app.clone());

    let tls_pem = if cfg.tls { Some(load_or_create_cert(&cfg.data_dir)?) } else { None };
    if let Some((cert, _)) = &tls_pem {
        // Compare this with the browser's certificate viewer before accepting the warning.
        println!("certificate SHA-256: {}", fingerprint(cert)?);
    }
    app.print_urls();

    if let Some((cert, key)) = tls_pem {
        let tls = axum_server::tls_rustls::RustlsConfig::from_pem(cert, key).await?;
        // WebSocket upgrades need HTTP/1.1; keep browsers off h2.
        let mut sc = (*tls.get_inner()).clone();
        sc.alpn_protocols = vec![b"http/1.1".to_vec()];
        let tls = axum_server::tls_rustls::RustlsConfig::from_config(Arc::new(sc));
        axum_server::bind_rustls(cfg.listen, tls).serve(router.into_make_service()).await?;
    } else {
        axum_server::bind(cfg.listen).serve(router.into_make_service()).await?;
    }
    Ok(())
}

/// The page is public; it authenticates its WebSocket with the token from its URL fragment (or sessionStorage).
/// The viewer is built from `web/` into `web/dist` (`npm run build`), which is committed and embedded here.
async fn index() -> Html<&'static str> {
    Html(include_str!("../../../web/dist/index.html"))
}

/// Revalidated on every load, so an upgraded server never runs a stale page.
fn asset(mime: &'static str, src: &'static str) -> Response {
    ([(header::CONTENT_TYPE, mime), (header::CACHE_CONTROL, "no-cache")], src).into_response()
}

fn markdown(src: &'static str) -> Response {
    ([(header::CONTENT_TYPE, "text/markdown; charset=utf-8")], src).into_response()
}

/// MCP over Streamable HTTP; the bearer middleware in front of it replaces rmcp's host allow-list.
fn mcp_service(app: Arc<App>) -> StreamableHttpService<mcp::Mcp, LocalSessionManager> {
    StreamableHttpService::new(move || Ok(mcp::Mcp::new(app.clone())), Arc::new(LocalSessionManager::default()), StreamableHttpServerConfig::default().disable_allowed_hosts())
}

/// Unauthenticated until the first message (see `ws::session`).
async fn websocket(ws: WebSocketUpgrade, State(app): State<Arc<App>>) -> Response {
    ws.max_message_size(1 + (1 << 20)).on_upgrade(move |socket| ws::session(socket, app)) // a pasted clipboard can be 1 MiB
}

/// One window as its own stream (see `ws::window_session`).
async fn window_websocket(ws: WebSocketUpgrade, UrlPath(id): UrlPath<u64>, State(app): State<Arc<App>>) -> Response {
    ws.max_message_size(1 + (1 << 20)).on_upgrade(move |socket| ws::window_session(socket, app, id))
}

/// `Authorization: Bearer <token>` for everything under /api and /mcp; nothing else (no cookies, no query
/// strings in logs). Which token it was rides along as a `Key` extension for the handlers and tools.
async fn bearer(State(app): State<Arc<App>>, mut req: Request, next: Next) -> Response {
    match app.key_of(req.headers()) {
        Some(key) => {
            req.extensions_mut().insert(key);
            next.run(req).await
        }
        None => StatusCode::UNAUTHORIZED.into_response(),
    }
}

/// The viewer token may look but not act.
fn writable(key: Key) -> Result<(), ApiError> {
    if key == Key::Control { Ok(()) } else { Err(ApiError::Forbidden) }
}

const NO_STORE: [(header::HeaderName, &str); 1] = [(header::CACHE_CONTROL, "no-store")];

/// The codecs this server encodes, in the order Auto prefers them, and whether on the GPU.
async fn api_codecs(State(app): State<Arc<App>>) -> Response {
    let list: Vec<serde_json::Value> = app.codecs.iter().map(|&c| serde_json::json!({ "codec": protocol::codec_name(c), "hardware": !app.software })).collect();
    (NO_STORE, Json(list)).into_response()
}

async fn api_windows(State(app): State<Arc<App>>) -> Response {
    (NO_STORE, Json(app.windows())).into_response()
}

fn scale_of(q: &HashMap<String, String>) -> f64 {
    q.get("scale").and_then(|s| s.parse().ok()).unwrap_or(1.0)
}

fn png(result: Result<Vec<u8>, ApiError>) -> Response {
    match result {
        Ok(bytes) => ([(header::CONTENT_TYPE, "image/png"), (header::CACHE_CONTROL, "no-store")], bytes).into_response(),
        Err(e) => e.into_response(),
    }
}

async fn api_window_snapshot(UrlPath(id): UrlPath<u64>, Query(q): Query<HashMap<String, String>>, State(app): State<Arc<App>>) -> Response {
    png(app.snapshot(Some(id), scale_of(&q)).await)
}

async fn api_screenshot(Query(q): Query<HashMap<String, String>>, State(app): State<Arc<App>>) -> Response {
    png(app.snapshot(None, scale_of(&q)).await)
}

async fn api_applications(State(app): State<Arc<App>>) -> Response {
    (NO_STORE, Json(app.applications().await)).into_response()
}

async fn api_application_icon(UrlPath(id): UrlPath<String>, State(app): State<Arc<App>>) -> Response {
    match app.application_icon(id).await {
        Ok((bytes, mime)) => ([(header::CONTENT_TYPE, mime), (header::CACHE_CONTROL, "private, max-age=86400")], bytes).into_response(),
        Err(e) => e.into_response(),
    }
}

async fn api_window_icon(UrlPath(id): UrlPath<u64>, State(app): State<Arc<App>>) -> Response {
    match app.window_icon(id).await {
        Ok((bytes, mime)) => ([(header::CONTENT_TYPE, mime), (header::CACHE_CONTROL, "private, max-age=300")], bytes).into_response(),
        Err(e) => e.into_response(),
    }
}

async fn api_files(State(app): State<Arc<App>>) -> Response {
    match app.files().await {
        Ok(list) => (NO_STORE, Json(list)).into_response(),
        Err(e) => e.into_response(),
    }
}

/// The body is streamed to the folder; `201` with `{"name": …}`, the name it got.
async fn api_put_file(Extension(key): Extension<Key>, UrlPath(name): UrlPath<String>, State(app): State<Arc<App>>, req: Request) -> Response {
    if let Err(e) = writable(key) {
        return e.into_response();
    }
    match app.store_file(&name, req.into_body()).await {
        Ok(name) => (StatusCode::CREATED, NO_STORE, Json(serde_json::json!({ "name": name }))).into_response(),
        Err(e) => e.into_response(),
    }
}

/// A file for the browser to save under `name`, streamed.
fn attachment(name: &str, len: u64, body: axum::body::Body) -> Response {
    (
        NO_STORE,
        [
            (header::CONTENT_TYPE, "application/octet-stream".to_string()),
            (header::CONTENT_LENGTH, len.to_string()),
            (header::CONTENT_DISPOSITION, format!("attachment; filename=\"{}\"; filename*=UTF-8''{}", name.chars().map(|c| if c.is_ascii_graphic() && c != '"' && c != '\\' || c == ' ' { c } else { '_' }).collect::<String>(), files::percent(name))),
        ],
        body,
    )
        .into_response()
}

async fn api_file(UrlPath(name): UrlPath<String>, State(app): State<Arc<App>>) -> Response {
    match app.open_file(&name).await {
        Ok((len, body)) => attachment(&name, len, body),
        Err(e) => e.into_response(),
    }
}

async fn api_delete_file(Extension(key): Extension<Key>, UrlPath(name): UrlPath<String>, State(app): State<Arc<App>>) -> Response {
    match writable(key) {
        Ok(()) => match app.delete_file(&name).await {
            Ok(()) => StatusCode::NO_CONTENT.into_response(),
            Err(e) => e.into_response(),
        },
        Err(e) => e.into_response(),
    }
}

async fn api_notifications(State(app): State<Arc<App>>) -> Response {
    (NO_STORE, Json(app.notifications())).into_response()
}

/// `{"action": "default" | "<key>"}`, or `{}` to dismiss; `202`, `404` unknown id.
async fn api_notification_action(Extension(key): Extension<Key>, UrlPath(id): UrlPath<u32>, State(app): State<Arc<App>>, Json(msg): Json<serde_json::Value>) -> Response {
    match writable(key) {
        Ok(()) => match app.notification_action(id, msg.get("action").and_then(|a| a.as_str())).await {
            Ok(()) => StatusCode::ACCEPTED.into_response(),
            Err(e) => e.into_response(),
        },
        Err(e) => e.into_response(),
    }
}

async fn api_notification_icon(UrlPath(id): UrlPath<u32>, State(app): State<Arc<App>>) -> Response {
    match app.notification_icon(id).await {
        Ok((bytes, mime)) => (NO_STORE, [(header::CONTENT_TYPE, mime)], bytes).into_response(),
        Err(e) => e.into_response(),
    }
}

async fn api_window_elements(UrlPath(id): UrlPath<u64>, State(app): State<Arc<App>>) -> Response {
    match app.elements(id).await {
        Ok(page) => (NO_STORE, Json(page)).into_response(),
        Err(e) => e.into_response(),
    }
}

async fn api_control(Extension(key): Extension<Key>, State(app): State<Arc<App>>, Json(msg): Json<ControlMsg>) -> Response {
    match writable(key).and_then(|()| app.control(msg)) {
        Ok(()) => StatusCode::ACCEPTED.into_response(),
        Err(e) => e.into_response(),
    }
}

/// New tokens, both: written to the data directory, printed like at startup, returned to the caller;
/// every viewer is closed with "token rotated" so a leaked link stops working at once.
async fn api_token_rotate(headers: HeaderMap, State(app): State<Arc<App>>) -> Response {
    let presented = headers.get(header::AUTHORIZATION).and_then(|a| a.to_str().ok()).and_then(|a| a.strip_prefix("Bearer ")).unwrap_or_default();
    match app.rotate_tokens(presented) {
        Ok((token, viewer)) => (NO_STORE, Json(serde_json::json!({ "token": token, "viewer_token": viewer }))).into_response(),
        Err(e) => e.into_response(),
    }
}

/// What a desktop application last copied: text, a PNG or a file list (its Content-Type says which), or 204 if nothing yet.
async fn api_clipboard(State(app): State<Arc<App>>) -> Response {
    match app.clipboard() {
        Some((mime, data)) => (NO_STORE, [(header::CONTENT_TYPE, match mime.as_str() { api::PNG | api::URI_LIST => mime.clone(), _ => "text/plain; charset=utf-8".into() })], data).into_response(),
        None => (StatusCode::NO_CONTENT, NO_STORE).into_response(),
    }
}

/// The body becomes the desktop clipboard: a PNG with `Content-Type: image/png` (up to 16 MiB), a file list
/// with `text/uri-list`, else UTF-8 text (up to 1 MiB).
/// The body is read only for a control token, and only up to its mime's limit.
async fn api_set_clipboard(Extension(key): Extension<Key>, State(app): State<Arc<App>>, req: Request) -> Response {
    if let Err(e) = writable(key) {
        return e.into_response();
    }
    let content_type = req.headers().get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or_default();
    let mime = if content_type.starts_with(api::PNG) { api::PNG } else if content_type.starts_with(api::URI_LIST) { api::URI_LIST } else { api::TEXT };
    let Ok(body) = axum::body::to_bytes(req.into_body(), api::clipboard_limit(mime)).await else { return ApiError::TooLarge.into_response() };
    let body = if mime == api::PNG { body } else { Bytes::from(String::from_utf8_lossy(&body).into_owned()) };
    match app.set_clipboard(mime, body) {
        Ok(()) => StatusCode::ACCEPTED.into_response(),
        Err(e) => e.into_response(),
    }
}

/// `{"names": [...]}`: files of the transfer folder become the desktop clipboard as a URI list, as a file
/// manager's copy would; `202`.
async fn api_clipboard_files(Extension(key): Extension<Key>, State(app): State<Arc<App>>, Json(msg): Json<serde_json::Value>) -> Response {
    let names: Vec<String> = msg.get("names").and_then(|n| n.as_array()).map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect()).unwrap_or_default();
    match writable(key).and_then(|()| app.set_clipboard_files(&names)) {
        Ok(()) => StatusCode::ACCEPTED.into_response(),
        Err(e) => e.into_response(),
    }
}

/// The `index`th file of the URI list on the desktop clipboard, as an attachment; `404` if the clipboard
/// holds no such list or entry.
async fn api_clipboard_file(UrlPath(index): UrlPath<usize>, State(app): State<Arc<App>>) -> Response {
    match app.clipboard_file(index).await {
        Ok((name, len, body)) => attachment(&name, len, body),
        Err(e) => e.into_response(),
    }
}

async fn api_input(Extension(key): Extension<Key>, State(app): State<Arc<App>>, Json(msg): Json<InputMsg>) -> Response {
    let warning = match &msg {
        InputMsg::Click { window: Some(id), x, y, .. } => app.x11_edge_warning(*id, *x, *y),
        _ => None,
    };
    match writable(key).and_then(|()| app.input(msg)) {
        Ok(()) => match warning {
            Some(w) => (StatusCode::ACCEPTED, NO_STORE, Json(serde_json::json!({ "warning": w }))).into_response(),
            None => StatusCode::ACCEPTED.into_response(),
        },
        Err(e) => e.into_response(),
    }
}

impl App {
    /// Which token the request carries, if a current one.
    pub(crate) fn key_of(&self, headers: &HeaderMap) -> Option<Key> {
        let bearer = headers.get(header::AUTHORIZATION)?.to_str().ok()?.strip_prefix("Bearer ")?;
        self.key_for(bearer)
    }

    pub(crate) fn key_for(&self, token: &str) -> Option<Key> {
        let (control, viewer) = &*self.tokens.read().unwrap();
        if same(token, control) {
            Some(Key::Control)
        } else if same(token, viewer) {
            Some(Key::Viewer)
        } else {
            None
        }
    }

    /// The tokens ride in the URL fragment, which browsers never send, so no server or proxy logs them.
    fn print_urls(&self) {
        let scheme = if self.tls { "https" } else { "http" };
        let (control, viewer) = &*self.tokens.read().unwrap();
        for ip in lan_ips() {
            println!("{scheme}://{ip}:{}/#token={control}", self.port);
            println!("{scheme}://{ip}:{}/#token={viewer}   (view only)", self.port);
        }
    }

    /// Only the holder of the current control token may rotate, checked again under the write lock so
    /// two rotations with the same old token can't both go through. Both tokens change.
    fn rotate_tokens(&self, presented: &str) -> Result<(String, String), ApiError> {
        let mut current = self.tokens.write().unwrap();
        if !same(presented, &current.0) {
            return Err(if same(presented, &current.1) { ApiError::Forbidden } else { ApiError::Unauthorized });
        }
        let fresh = (random_hex(32), random_hex(32));
        for (name, token) in [("token", &fresh.0), ("viewer-token", &fresh.1)] {
            // written next to the old file and renamed over it, so a crash leaves one whole token or the other
            let path = self.data_dir.join(name);
            let tmp = self.data_dir.join(format!("{name}.new"));
            let _ = fs::remove_file(&tmp);
            write_private(&tmp, token.as_bytes()).and_then(|()| fs::rename(&tmp, &path)).map_err(|e| ApiError::Internal(format!("token file: {e}")))?;
        }
        *current = fresh.clone();
        drop(current);
        // every session authenticated with an old token: dropping its senders ends it as "token rotated"
        {
            let mut v = self.viewers.lock().unwrap();
            v.sessions.clear();
            v.controller = None;
        }
        self.window_viewers.lock().unwrap().clear();
        let _ = self.commands.send(Command::ReleaseAllInput);
        println!("tokens rotated; new viewer URLs:");
        self.print_urls();
        Ok(fresh)
    }
}

/// Constant-time string compare, no dependency needed.
fn same(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn load_or_create(path: &Path, make: impl FnOnce() -> Result<String>) -> Result<String> {
    match fs::read_to_string(path) {
        Ok(s) => Ok(s.trim().to_string()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            let s = make()?;
            write_private(path, s.as_bytes())?;
            Ok(s)
        }
        Err(e) => Err(e).with_context(|| path.display().to_string()),
    }
}

fn write_private(path: &Path, data: &[u8]) -> io::Result<()> {
    use io::Write;
    fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(path)?.write_all(data)
}

fn random_hex(n: usize) -> String {
    use io::Read;
    let mut buf = vec![0u8; n];
    fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut buf)).expect("/dev/urandom");
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

fn lan_ips() -> Vec<std::net::IpAddr> {
    if_addrs::get_if_addrs()
        .unwrap_or_default()
        .into_iter()
        .filter(|i| !i.is_loopback() && i.ip().is_ipv4())
        .map(|i| i.ip())
        .collect()
}

/// Self-signed cert with every local address as a SAN. Delete the files to regenerate.
fn load_or_create_cert(dir: &Path) -> Result<(Vec<u8>, Vec<u8>)> {
    let (cert_path, key_path) = (dir.join("cert.pem"), dir.join("key.pem"));
    if let (Ok(c), Ok(k)) = (fs::read(&cert_path), fs::read(&key_path)) {
        return Ok((c, k));
    }
    let _ = fs::remove_file(&key_path); // never leave a mismatched pair behind
    let mut sans = vec!["localhost".to_string()];
    sans.extend(if_addrs::get_if_addrs()?.iter().filter(|i| !i.is_loopback()).map(|i| i.ip().to_string()));
    let mut params = rcgen::CertificateParams::new(sans)?;
    params.distinguished_name.push(rcgen::DnType::CommonName, "browser-wayland");
    let key = rcgen::KeyPair::generate()?;
    let cert = params.self_signed(&key)?;
    fs::write(&cert_path, cert.pem())?;
    write_private(&key_path, key.serialize_pem().as_bytes())?;
    Ok((cert.pem().into_bytes(), key.serialize_pem().into_bytes()))
}

/// Colon-separated SHA-256 of the certificate DER, as browsers display it.
fn fingerprint(cert_pem: &[u8]) -> Result<String> {
    use rustls::pki_types::{CertificateDer, pem::PemObject};
    use sha2::Digest;
    let der = CertificateDer::from_pem_slice(cert_pem)?;
    Ok(sha2::Sha256::digest(&der).iter().map(|b| format!("{b:02X}")).collect::<Vec<_>>().join(":"))
}
