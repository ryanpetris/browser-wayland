//! HTTPS + WebSocket front end: serves the viewer page, authenticates the WebSocket in-band and the
//! HTTP API with a bearer token (never a cookie), streams encoded video out and turns browser input
//! into `Command`s.

mod api;
mod elements;
mod mcp;
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
    Json, Router,
    extract::{Path as UrlPath, Query, Request, State, ws::WebSocketUpgrade},
    http::{HeaderMap, StatusCode, header},
    middleware::{self, Next},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
};
use bw_core::{Bytes, Codec, Command, ControlMsg, Event, FrameSink, InputMsg, StreamControl, StreamInfo, StreamMsg, WindowInfo};
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager};
use tokio::sync::mpsc;

/// `None` = automatic: HEVC, then VP9, then H.264, first one the browser decodes in hardware.
pub type CodecPolicy = Option<Codec>;
/// Makes an encoder for one window stream: the sink the compositor feeds and a control handle that
/// must not keep the pipeline alive (the stream ends when the compositor drops the sink).
pub type SinkFactory = Box<dyn Fn(mpsc::Sender<StreamMsg>) -> Result<(Box<dyn FrameSink>, Box<dyn StreamControl>)> + Send + Sync>;

pub struct Config {
    pub listen: SocketAddr,
    pub tls: bool,
    pub codec: CodecPolicy,
    /// Where `cert.pem`, `key.pem` and `token` live.
    pub data_dir: PathBuf,
    /// Serve /api/windows/{id}/elements (see `elements.rs`).
    pub elements: bool,
    /// Reported to MCP clients.
    pub version: &'static str,
    /// Per-window streams (`/ws/window/{id}`); `None` without a compositor.
    pub window_sinks: Option<SinkFactory>,
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
    token: RwLock<String>,
    data_dir: PathBuf,
    commands: calloop::channel::Sender<Command>,
    control: Box<dyn StreamControl>,
    policy: CodecPolicy,
    viewer: Mutex<Viewer>,
    window_sinks: Option<SinkFactory>,
    /// Event senders of the window-stream sessions (cursor, clipboard, window list go to them too).
    window_viewers: Mutex<HashMap<u64, mpsc::Sender<Bytes>>>,
    snapshot_lock: tokio::sync::Semaphore,
    elements: bool,
    version: &'static str,
    tls: bool,
    port: u16,
}

#[derive(Default)]
pub(crate) struct Viewer {
    /// Bumped per connection so a replaced session can't act for the current one.
    generation: u64,
    tx: Option<mpsc::Sender<Bytes>>,
    /// Audio has its own small queue so it can't push video into the keyframe path.
    audio_tx: Option<mpsc::Sender<Bytes>>,
    info: Option<StreamInfo>,
    /// Stream id whose Config the current viewer has been sent.
    announced: Option<u32>,
    need_key: bool,
    /// Last cursor message, replayed to a new viewer.
    cursor: Option<Bytes>,
    /// Whether a client currently holds a pointer lock, replayed to a new viewer.
    locked: bool,
    /// Last WINDOWS message, replayed to a new viewer, and the list it encodes (the API's view).
    windows: Option<Bytes>,
    window_list: Vec<WindowInfo>,
    /// Per-stream message counters (every produced frame or packet, sent or dropped); wrap at u16.
    video_seq: u16,
    audio_seq: u16,
    /// The last clipboard text, from an application, the browser or the API (served on the API; not replayed to a viewer).
    clipboard: Option<String>,
}

pub async fn run(
    cfg: Config,
    commands: calloop::channel::Sender<Command>,
    stream_rx: mpsc::Receiver<StreamMsg>,
    events_rx: mpsc::UnboundedReceiver<Event>,
    control: Box<dyn StreamControl>,
) -> Result<()> {
    fs::create_dir_all(&cfg.data_dir)?;
    let token = load_or_create(&cfg.data_dir.join("token"), || Ok(random_hex(32)))?;
    let app = Arc::new(App {
        token: RwLock::new(token),
        data_dir: cfg.data_dir.clone(),
        commands,
        control,
        policy: cfg.codec,
        viewer: Mutex::default(),
        window_sinks: cfg.window_sinks,
        window_viewers: Mutex::default(),
        snapshot_lock: tokio::sync::Semaphore::new(1),
        elements: cfg.elements,
        version: cfg.version,
        tls: cfg.tls,
        port: cfg.listen.port(),
    });
    tokio::spawn(ws::distribute(app.clone(), stream_rx));
    tokio::spawn(ws::forward_events(app.clone(), events_rx));

    let router = Router::new()
        .route("/", get(index))
        .route("/app.js", get(|| async { js(include_str!("../../../web/app.js")) }))
        .route("/keycodes.js", get(|| async { js(include_str!("../../../web/keycodes.js")) }))
        .route("/desktop.js", get(|| async { js(include_str!("../../../web/desktop.js")) }))
        .route("/ws", get(websocket))
        .route("/ws/window/{id}", get(window_websocket))
        .merge(
            Router::new()
                .route("/api/windows", get(api_windows))
                .route("/api/control", post(api_control))
                .route("/api/input", post(api_input))
                .route("/api/windows/{id}/snapshot.png", get(api_window_snapshot))
                .route("/api/screenshot.png", get(api_screenshot))
                .route("/api/windows/{id}/elements", get(api_window_elements))
                .route("/api/token/rotate", post(api_token_rotate))
                .route("/api/clipboard", get(api_clipboard).put(api_set_clipboard))
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
async fn index() -> Html<&'static str> {
    Html(include_str!("../../../web/index.html"))
}

/// Revalidated on every load, so an upgraded server never runs a stale page.
fn js(src: &'static str) -> Response {
    ([(header::CONTENT_TYPE, "text/javascript"), (header::CACHE_CONTROL, "no-cache")], src).into_response()
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

/// `Authorization: Bearer <token>` for everything under /api and /mcp; nothing else (no cookies, no query strings in logs).
async fn bearer(State(app): State<Arc<App>>, req: Request, next: Next) -> Response {
    if app.authorized(req.headers()) { next.run(req).await } else { StatusCode::UNAUTHORIZED.into_response() }
}

const NO_STORE: [(header::HeaderName, &str); 1] = [(header::CACHE_CONTROL, "no-store")];

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

async fn api_window_elements(UrlPath(id): UrlPath<u64>, State(app): State<Arc<App>>) -> Response {
    match app.elements(id).await {
        Ok(page) => (NO_STORE, Json(page)).into_response(),
        Err(e) => e.into_response(),
    }
}

async fn api_control(State(app): State<Arc<App>>, Json(msg): Json<ControlMsg>) -> Response {
    match app.control(msg) {
        Ok(()) => StatusCode::ACCEPTED.into_response(),
        Err(e) => e.into_response(),
    }
}

/// A new token: written to the data directory, printed like at startup, returned to the caller; the
/// current viewer is closed with "wrong token" so a leaked link stops working at once.
async fn api_token_rotate(headers: HeaderMap, State(app): State<Arc<App>>) -> Response {
    let presented = headers.get(header::AUTHORIZATION).and_then(|a| a.to_str().ok()).and_then(|a| a.strip_prefix("Bearer ")).unwrap_or_default();
    match app.rotate_token(presented) {
        Ok(token) => (NO_STORE, Json(serde_json::json!({ "token": token }))).into_response(),
        Err(e) => e.into_response(),
    }
}

/// The last text a desktop application copied, or 204 if none yet.
async fn api_clipboard(State(app): State<Arc<App>>) -> Response {
    match app.clipboard() {
        Some(text) => (NO_STORE, [(header::CONTENT_TYPE, "text/plain; charset=utf-8")], text).into_response(),
        None => (StatusCode::NO_CONTENT, NO_STORE).into_response(),
    }
}

/// The body (UTF-8 text, up to 1 MiB) becomes the desktop clipboard.
async fn api_set_clipboard(State(app): State<Arc<App>>, body: String) -> Response {
    match app.set_clipboard(body) {
        Ok(()) => StatusCode::ACCEPTED.into_response(),
        Err(e) => e.into_response(),
    }
}

async fn api_input(State(app): State<Arc<App>>, Json(msg): Json<InputMsg>) -> Response {
    match app.input(msg) {
        Ok(()) => StatusCode::ACCEPTED.into_response(),
        Err(e) => e.into_response(),
    }
}

impl App {
    fn authorized(&self, headers: &HeaderMap) -> bool {
        let bearer = headers.get(header::AUTHORIZATION).and_then(|a| a.to_str().ok()).and_then(|a| a.strip_prefix("Bearer "));
        bearer.is_some_and(|t| self.token_ok(t))
    }

    pub(crate) fn token_ok(&self, t: &str) -> bool {
        same(t, &self.token.read().unwrap())
    }

    /// The token rides in the URL fragment, which browsers never send, so no server or proxy logs it.
    fn print_urls(&self) {
        let scheme = if self.tls { "https" } else { "http" };
        let token = self.token.read().unwrap();
        for ip in lan_ips() {
            println!("{scheme}://{ip}:{}/#token={token}", self.port);
        }
    }

    /// Only the holder of the current token may rotate, checked again under the write lock so two
    /// rotations with the same old token can't both go through.
    fn rotate_token(&self, presented: &str) -> Result<String, ApiError> {
        let mut current = self.token.write().unwrap();
        if !same(presented, &current) {
            return Err(ApiError::Unauthorized);
        }
        let token = random_hex(32);
        // written next to the old file and renamed over it, so a crash leaves one whole token or the other
        let path = self.data_dir.join("token");
        let tmp = self.data_dir.join("token.new");
        let _ = fs::remove_file(&tmp);
        write_private(&tmp, token.as_bytes()).and_then(|()| fs::rename(&tmp, &path)).map_err(|e| ApiError::Internal(format!("token file: {e}")))?;
        *current = token.clone();
        drop(current);
        {
            // The connected viewer authenticated with the old token: its session ends as "token rotated",
            // and the compositor hears what it would have on a disconnect.
            let mut v = self.viewer.lock().unwrap();
            v.generation += 1;
            if v.tx.take().is_some() {
                let _ = self.commands.send(Command::ReleaseAllInput);
                let _ = self.commands.send(Command::ViewerDisconnected);
            }
            v.audio_tx = None;
        }
        println!("token rotated; new viewer URLs:");
        self.print_urls();
        Ok(token)
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
