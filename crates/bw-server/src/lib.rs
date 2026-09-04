//! HTTPS + WebSocket front end: serves the viewer page, authenticates the WebSocket in-band and the
//! HTTP API with a bearer token (never a cookie), streams encoded video out and turns browser input
//! into `Command`s.

mod elements;
mod protocol;
mod ws;

use std::{
    collections::HashMap,
    fs, io,
    net::SocketAddr,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use anyhow::{Context, Result};
use axum::{
    Router,
    extract::{Path as UrlPath, Query, State, ws::WebSocketUpgrade},
    Json,
    http::{HeaderMap, StatusCode, header},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
};
use bw_core::{Bytes, Codec, Command, ControlMsg, Event, Snapshot, SnapshotReply, StreamControl, StreamInfo, StreamMsg};
use tokio::sync::mpsc;

/// `None` = automatic: HEVC, then VP9, then H.264, first one the browser decodes in hardware.
pub type CodecPolicy = Option<Codec>;

pub struct Config {
    pub listen: SocketAddr,
    pub tls: bool,
    pub codec: CodecPolicy,
    /// Where `cert.pem`, `key.pem` and `token` live.
    pub data_dir: PathBuf,
    /// Serve /api/windows/{id}/elements (see `elements.rs`).
    pub elements: bool,
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
    token: String,
    commands: calloop::channel::Sender<Command>,
    control: Box<dyn StreamControl>,
    policy: CodecPolicy,
    viewer: Mutex<Viewer>,
    snapshot_lock: tokio::sync::Semaphore,
    elements: bool,
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
    /// Last WINDOWS message, replayed to a new viewer and served on /api/windows.
    windows: Option<Bytes>,
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
        token,
        commands,
        control,
        policy: cfg.codec,
        viewer: Mutex::default(),
        snapshot_lock: tokio::sync::Semaphore::new(1),
        elements: cfg.elements,
    });
    tokio::spawn(ws::distribute(app.clone(), stream_rx));
    tokio::spawn(ws::forward_events(app.clone(), events_rx));

    let router = Router::new()
        .route("/", get(index))
        .route("/app.js", get(|| async { js(include_str!("../../../web/app.js")) }))
        .route("/keycodes.js", get(|| async { js(include_str!("../../../web/keycodes.js")) }))
        .route("/desktop.js", get(|| async { js(include_str!("../../../web/desktop.js")) }))
        .route("/ws", get(websocket))
        .route("/api/windows", get(api_windows))
        .route("/api/control", post(api_control))
        .route("/api/windows/{id}/snapshot.png", get(api_window_snapshot))
        .route("/api/screenshot.png", get(api_screenshot))
        .route("/api/windows/{id}/elements", get(api_window_elements))
        .with_state(app.clone());

    let tls_pem = if cfg.tls { Some(load_or_create_cert(&cfg.data_dir)?) } else { None };
    if let Some((cert, _)) = &tls_pem {
        // Compare this with the browser's certificate viewer before accepting the warning.
        println!("certificate SHA-256: {}", fingerprint(cert)?);
    }
    let scheme = if cfg.tls { "https" } else { "http" };
    for ip in lan_ips() {
        println!("{scheme}://{ip}:{}/?token={}", cfg.listen.port(), app.token);
    }

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

/// The page is public; it authenticates its WebSocket with the token it finds in its own URL.
async fn index() -> Html<&'static str> {
    Html(include_str!("../../../web/index.html"))
}

fn js(src: &'static str) -> Response {
    ([(header::CONTENT_TYPE, "text/javascript")], src).into_response()
}

/// Unauthenticated until the first message (see `ws::session`).
async fn websocket(ws: WebSocketUpgrade, State(app): State<Arc<App>>) -> Response {
    ws.max_message_size(64 << 10).on_upgrade(move |socket| ws::session(socket, app))
}

/// The current window list as JSON (what the viewer was last sent).
async fn api_windows(headers: HeaderMap, State(app): State<Arc<App>>) -> Response {
    if !app.authorized(&headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let body = app.viewer.lock().unwrap().windows.as_ref().map(|b| b.slice(1..)).unwrap_or_else(|| Bytes::from_static(b"[]"));
    ([(header::CONTENT_TYPE, "application/json")], body).into_response()
}

async fn api_window_snapshot(UrlPath(id): UrlPath<u64>, Query(q): Query<HashMap<String, String>>, headers: HeaderMap, State(app): State<Arc<App>>) -> Response {
    snapshot(&app, &headers, &q, Some(id)).await
}

async fn api_screenshot(Query(q): Query<HashMap<String, String>>, headers: HeaderMap, State(app): State<Arc<App>>) -> Response {
    snapshot(&app, &headers, &q, None).await
}

/// `?scale=` (windows only, 0.05..=2, default 1) → PNG. 404 for an unknown window, 503 if the compositor doesn't answer.
async fn snapshot(app: &App, headers: &HeaderMap, q: &HashMap<String, String>, id: Option<u64>) -> Response {
    if !app.authorized(headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    // One at a time: the compositor renders these on its own thread and a queued request can't be cancelled.
    let Ok(_busy) = app.snapshot_lock.try_acquire() else { return StatusCode::TOO_MANY_REQUESTS.into_response() };
    let scale = q.get("scale").and_then(|s| s.parse::<f64>().ok()).unwrap_or(1.0).clamp(0.05, 2.0);
    let (tx, rx) = tokio::sync::oneshot::channel::<Option<Snapshot>>();
    let reply = SnapshotReply(Box::new(move |s| {
        let _ = tx.send(s);
    }));
    if app.commands.send(Command::Snapshot { id, scale, reply }).is_err() {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    let snap = match tokio::time::timeout(std::time::Duration::from_secs(2), rx).await {
        Ok(Ok(Some(s))) => s,
        Ok(Ok(None)) => return StatusCode::NOT_FOUND.into_response(),
        _ => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
    };
    let png = tokio::task::spawn_blocking(move || -> Result<Vec<u8>> {
        let mut out = Vec::new();
        let mut enc = png::Encoder::new(&mut out, snap.width, snap.height);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        enc.write_header()?.write_image_data(&snap.rgba)?;
        Ok(out)
    })
    .await;
    match png {
        Ok(Ok(bytes)) => ([(header::CONTENT_TYPE, "image/png"), (header::CACHE_CONTROL, "no-store")], bytes).into_response(),
        _ => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

/// 501 without `--elements`, 503 when the accessibility bus can't be reached, 404 for an unknown window,
/// else `{level, toolkit, elements}` (see `elements.rs`).
async fn api_window_elements(UrlPath(id): UrlPath<u64>, headers: HeaderMap, State(app): State<Arc<App>>) -> Response {
    if !app.authorized(&headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    if !app.elements {
        return (StatusCode::NOT_IMPLEMENTED, Json(serde_json::json!({ "error": "started without --elements" }))).into_response();
    }
    let (list, scale) = {
        let v = app.viewer.lock().unwrap();
        let list = v.windows.as_ref().and_then(|b| serde_json::from_slice::<Vec<bw_core::WindowInfo>>(&b[1..]).ok()).unwrap_or_default();
        (list, v.info.as_ref().map_or(1.0, |i| i.scale))
    };
    let Some(win) = list.into_iter().find(|w| w.id == id) else { return StatusCode::NOT_FOUND.into_response() };
    match tokio::time::timeout(std::time::Duration::from_secs(2), elements::elements(&win, scale)).await {
        Ok(Ok(page)) => Json(page).into_response(),
        Ok(Err(e)) => (StatusCode::SERVICE_UNAVAILABLE, Json(serde_json::json!({ "error": format!("{e:#}") }))).into_response(),
        Err(_) => (StatusCode::SERVICE_UNAVAILABLE, Json(serde_json::json!({ "error": "timed out reading the tree" }))).into_response(),
    }
}

/// Fire-and-forget: the compositor ignores unknown ids and impossible requests.
async fn api_control(headers: HeaderMap, State(app): State<Arc<App>>, Json(msg): Json<ControlMsg>) -> Response {
    if !app.authorized(&headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let _ = app.commands.send(Command::Control(msg));
    StatusCode::ACCEPTED.into_response()
}

impl App {
    /// HTTP API: `Authorization: Bearer <token>`, nothing else (no cookies, no query strings in logs).
    fn authorized(&self, headers: &HeaderMap) -> bool {
        let bearer = headers.get(header::AUTHORIZATION).and_then(|a| a.to_str().ok()).and_then(|a| a.strip_prefix("Bearer "));
        bearer.is_some_and(|t| self.token_ok(t))
    }

    pub(crate) fn token_ok(&self, t: &str) -> bool {
        // constant-time compare, no dependency needed
        t.len() == self.token.len()
            && t.bytes().zip(self.token.bytes()).fold(0u8, |acc, (a, b)| acc | (a ^ b)) == 0
    }
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
