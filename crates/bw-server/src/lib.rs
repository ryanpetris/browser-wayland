//! HTTPS + WebSocket front end: serves the viewer page, authenticates with a
//! shared token, streams encoded video out and turns browser input into `Command`s.

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
    extract::{Query, State, ws::WebSocketUpgrade},
    http::{HeaderMap, StatusCode, header},
    response::{Html, IntoResponse, Redirect, Response},
    routing::get,
};
use bw_core::{Bytes, Codec, Command, Event, StreamControl, StreamInfo, StreamMsg};
use tokio::sync::mpsc;

/// `None` = automatic: HEVC, then VP9, then H.264, first one the browser decodes in hardware.
pub type CodecPolicy = Option<Codec>;

pub struct Config {
    pub listen: SocketAddr,
    pub tls: bool,
    pub codec: CodecPolicy,
    /// Where `cert.pem`, `key.pem` and `token` live.
    pub data_dir: PathBuf,
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
    tls: bool,
    commands: calloop::channel::Sender<Command>,
    control: Box<dyn StreamControl>,
    policy: CodecPolicy,
    viewer: Mutex<Viewer>,
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
        tls: cfg.tls,
        commands,
        control,
        policy: cfg.codec,
        viewer: Mutex::default(),
    });
    tokio::spawn(ws::distribute(app.clone(), stream_rx));
    tokio::spawn(ws::forward_events(app.clone(), events_rx));

    let router = Router::new()
        .route("/", get(index))
        .route("/app.js", get(|| async { js(include_str!("../../../web/app.js")) }))
        .route("/keycodes.js", get(|| async { js(include_str!("../../../web/keycodes.js")) }))
        .route("/ws", get(websocket))
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

/// `/?token=…` sets the auth cookie and redirects to `/`; otherwise serve the page.
async fn index(Query(q): Query<HashMap<String, String>>, State(app): State<Arc<App>>) -> Response {
    match q.get("token") {
        Some(t) if app.token_ok(t) => {
            let secure = if app.tls { "; Secure" } else { "" };
            (
                [(header::SET_COOKIE, format!("bw_token={t}; Path=/ws; HttpOnly; SameSite=Strict{secure}"))],
                Redirect::to("/"),
            )
                .into_response()
        }
        _ => Html(include_str!("../../../web/index.html")).into_response(),
    }
}

fn js(src: &'static str) -> Response {
    ([(header::CONTENT_TYPE, "text/javascript")], src).into_response()
}

async fn websocket(
    ws: WebSocketUpgrade,
    Query(q): Query<HashMap<String, String>>,
    headers: HeaderMap,
    State(app): State<Arc<App>>,
) -> Response {
    // Same-site cookies still travel cross-origin from another port on this host: require a matching Origin.
    let host = headers.get(header::HOST).and_then(|h| h.to_str().ok());
    let origin_host = headers.get(header::ORIGIN).and_then(|o| o.to_str().ok()).map(|o| o.trim_start_matches("https://").trim_start_matches("http://"));
    if origin_host.is_some_and(|o| Some(o) != host) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let cookie_token = headers
        .get(header::COOKIE)
        .and_then(|c| c.to_str().ok())
        .and_then(|c| c.split(';').find_map(|kv| kv.trim().strip_prefix("bw_token=")));
    let ok = q.get("token").map(String::as_str).or(cookie_token).is_some_and(|t| app.token_ok(t));
    if !ok {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    ws.max_message_size(64 << 10).on_upgrade(move |socket| ws::session(socket, app))
}

impl App {
    fn token_ok(&self, t: &str) -> bool {
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
