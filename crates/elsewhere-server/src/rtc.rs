//! WebRTC data-channel transport for the video: the same messages the WebSocket carries, over a `video`
//! data channel the browser opens (ordered, reliable), for viewers who need UDP to reach the desktop
//! at all: through NAT, or through a TURN relay. It is a viewer's choice, not the default, because the
//! socket carries the picture better under packet loss (measured in the README). str0m does ICE (lite,
//! host candidates), DTLS and SCTP without I/O of its own; one hub task drives every session's peer
//! connection over one UDP socket per local address (a received packet's destination must be one of the
//! candidates, so a socket per address knows it). Signalling goes over the session's WebSocket (`RTC`
//! messages: the browser's offer, our answer); the frame path in `ws.rs` hands frames to the hub while
//! the session's channel is open and to the socket otherwise. A frame goes as numbered fragments the page
//! reassembles, written as the SCTP send buffer (128 kB, freed by the browser's acknowledgements) has
//! room; frames wait their turn in a queue as they would for the socket, and the queue's depth is
//! congestion to the session's rate controller. A keyframe replaces whatever waits (the page needs it
//! whatever else it gets, and a keyframe behind a queue on a lossy link is a stall of seconds; what was
//! written of the frame in flight still has to go out), and a frame arriving at a full queue is dropped
//! rather than waiting longer; either is a seq gap the page answers with a keyframe request. A fragment
//! on its way is retransmitted if it is lost, so what the page misses is what was dropped here, never
//! what the network ate.

use std::{
    collections::{HashMap, VecDeque},
    net::{IpAddr, SocketAddr},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use elsewhere_core::Bytes;
use str0m::{Candidate, Event, Input, Output, Rtc, change::SdpOffer, channel::ChannelId, net::{Protocol, Receive}};
use tokio::{net::UdpSocket, sync::{mpsc, oneshot}};

use crate::protocol;

/// Fragments of this size go down the channel: every browser takes them, and a lost one is retransmitted.
const FRAGMENT: usize = 16 * 1024;

/// What the CLI decided: the UDP port, the address to advertise, and the ICE servers the browser should use.
pub struct Config {
    pub port: u16,
    /// The one address browsers reach us at, when it isn't ours (a Docker bridge maps the host's port to
    /// the container); without it the answer advertises the viewer's page endpoint.
    pub addr: Option<IpAddr>,
    /// STUN and TURN servers as the page's `RTCPeerConnection` wants them (`urls`, `username`, `credential`).
    pub ice_servers: Vec<serde_json::Value>,
}

enum Msg {
    /// A session's browser offered (`g` numbers its attempt; the answer carries it back through `reply`,
    /// the session's own event queue).
    Offer { session: u64, sdp: String, g: u64, reply: mpsc::Sender<Bytes>, endpoint: Option<Vec<SocketAddr>> },
    /// A frame for a session's channel.
    Frame { session: u64, data: Bytes },
    /// The session ended, or went back to its socket.
    Close { session: u64, g: Option<u64>, reply: Option<oneshot::Sender<bool>> },
}

/// A session's way to the hub; cheap to clone.
#[derive(Clone)]
pub struct Hub {
    tx: mpsc::Sender<Msg>,
    /// Channel claims and pressure. Closed claims remain until acknowledged, so even a pending
    /// browser attempt that missed its final frame can request a refresh.
    open: Arc<Open>,
    pub ice_servers: Arc<Vec<serde_json::Value>>,
    page_endpoint: bool,
}

impl Hub {
    /// Binds the port on every local address (or on any, advertised as `cfg.addr`) and starts the hub task.
    pub async fn start(cfg: Config) -> Result<Hub> {
        let mut sockets = HashMap::new();
        match cfg.addr {
            Some(ip) => {
                let any = if ip.is_ipv4() { IpAddr::from([0, 0, 0, 0]) } else { IpAddr::from([0u16; 8]) };
                sockets.insert(SocketAddr::new(ip, cfg.port), Arc::new(UdpSocket::bind(SocketAddr::new(any, cfg.port)).await?));
            }
            None => {
                for ip in local_ips() {
                    let addr = SocketAddr::new(ip, cfg.port);
                    match UdpSocket::bind(addr).await {
                        Ok(s) => {
                            sockets.insert(addr, Arc::new(s));
                        }
                        Err(e) => tracing::warn!("WebRTC: can't listen on {addr}: {e}"),
                    }
                }
            }
        }
        anyhow::ensure!(!sockets.is_empty(), "no local address to listen on");
        let (tx, rx) = mpsc::channel(256);
        let hub = Hub { tx, open: Default::default(), ice_servers: Arc::new(cfg.ice_servers), page_endpoint: cfg.addr.is_none() };
        tokio::spawn(run(sockets, rx, hub.open.clone()));
        tracing::info!(port = cfg.port, "WebRTC: data channels on UDP");
        Ok(hub)
    }

    /// The session's channel, while it is open, under pressure: frames dropped since the last call, and
    /// frames waiting in its queue now; congestion, to its rate controller.
    pub fn pressure(&self, session: u64) -> Option<(u32, usize)> {
        self.open.lock().unwrap().get_mut(&session).filter(|c| c.active).map(|c| (std::mem::take(&mut c.dropped), c.queued))
    }

    /// Signalling waits for room in the queue: an offer without an answer, or a close that never arrives,
    /// would be a stuck viewer or a lingering peer.
    pub async fn offer(&self, session: u64, sdp: String, g: u64, reply: mpsc::Sender<Bytes>, endpoint: Option<&serde_json::Value>) {
        let endpoint = if self.page_endpoint {
            match resolve_endpoint(endpoint).await {
                Ok(endpoint) => Some(endpoint),
                Err(e) => {
                    tracing::warn!(session, "WebRTC endpoint refused: {e:#}");
                    let _ = reply.try_send(protocol::rtc(&serde_json::json!({ "close": true, "g": g, "reason": "Page endpoint resolution failed" })));
                    return;
                }
            }
        } else { None };
        let _ = self.tx.send(Msg::Offer { session, sdp, g, reply, endpoint }).await;
    }

    /// A frame for the session's channel; one that doesn't fit the hub's mailbox is dropped (the page asks
    /// for a keyframe when it sees the gap).
    pub fn frame(&self, session: u64, data: Bytes) {
        if self.tx.try_send(Msg::Frame { session, data }).is_err() {
            dropped(&self.open, session, 1);
        }
    }

    /// Releases only the named attempt, returning whether it held video and needs a refresh.
    /// Delayed client closes cannot remove its successor.
    pub async fn close_attempt(&self, session: u64, g: u64) -> bool {
        let (reply, rx) = oneshot::channel();
        let _ = self.tx.send(Msg::Close { session, g: Some(g), reply: Some(reply) }).await;
        rx.await.unwrap_or(false)
    }

    pub async fn close(&self, session: u64) {
        let _ = self.tx.send(Msg::Close { session, g: None, reply: None }).await;
    }
}

/// Resolve outside the shared hub so a slow DNS lookup cannot stall other viewers.
async fn resolve_endpoint(value: Option<&serde_json::Value>) -> Result<Vec<SocketAddr>> {
    let value = value.context("page endpoint is required")?;
    #[derive(serde::Deserialize)]
    struct Endpoint { host: String, port: u16 }
    let endpoint: Endpoint = serde_json::from_value(value.clone())?;
    anyhow::ensure!(!endpoint.host.is_empty() && endpoint.host.len() <= 253 && endpoint.port != 0, "invalid page endpoint");
    let mut addrs: Vec<_> = tokio::time::timeout(Duration::from_secs(2), tokio::net::lookup_host((endpoint.host.as_str(), endpoint.port)))
        .await.context("page endpoint DNS timeout")??.collect();
    addrs.sort_unstable();
    addrs.dedup();
    anyhow::ensure!(!addrs.is_empty(), "page endpoint has no addresses");
    for addr in &addrs { Candidate::host(*addr, "udp").context("invalid page endpoint address")?; }
    Ok(addrs)
}

/// Only the wire SDP uses external candidates. ICE receives packets on the actual local sockets.
fn advertise_endpoint(sdp: &str, addrs: &[SocketAddr]) -> Result<String> {
    anyhow::ensure!(sdp.lines().any(|line| line.starts_with("a=candidate:")), "answer has no candidate line");
    let candidates = addrs.iter().map(|addr| Candidate::host(*addr, "udp")
        .map(|c| format!("a={}\r\n", c.to_sdp_string()))).collect::<Result<Vec<_>, _>>()?.concat();
    let mut out = String::new();
    let mut inserted = false;
    for line in sdp.lines() {
        if line.starts_with("m=") { inserted = false; }
        if line.starts_with("a=candidate:") {
            if !inserted { out.push_str(&candidates); inserted = true; }
        } else {
            out.push_str(line);
            out.push_str("\r\n");
        }
    }
    Ok(out)
}

/// Include loopback for viewers opened locally. Link-local IPv6 needs an interface scope.
fn local_ips() -> Vec<IpAddr> {
    if_addrs::get_if_addrs()
        .unwrap_or_default()
        .into_iter()
        .map(|i| i.ip())
        .filter(|ip| !matches!(ip, IpAddr::V6(v6) if v6.is_unicast_link_local()))
        .collect()
}

struct Peer {
    rtc: Rtc,
    channel: Option<ChannelId>,
    /// Numbers the fragmented messages, so the page can tell one frame's fragments from the next's.
    frame_id: u32,
    /// Frames waiting for the send buffer, the front one `sent` fragments in since `front_since`.
    queue: VecDeque<Bytes>,
    sent: usize,
    front_since: Instant,
    /// The session's socket and the offer's number, for the word that the channel is given up.
    reply: mpsc::Sender<Bytes>,
    g: u64,
    close_reason: &'static str,
}

/// A frame at the front of the queue this long, moving or not, is too slow for a desktop: the channel
/// is given up and the socket takes the video.
const STALL: Duration = Duration::from_secs(3);

/// Frames a channel's queue holds before a new one is dropped: half a second at 60 fps.
// ponytail: a cap; blocking the session as the socket does if a viewer wants the latency bounded tighter
const QUEUE: usize = 30;

/// The latest channel claim, with live queue pressure or an inactive claim awaiting close acknowledgement.
struct Claim {
    g: u64,
    active: bool,
    dropped: u32,
    queued: usize,
}
type Open = Mutex<HashMap<u64, Claim>>;

/// One datagram in: the candidate address it came to, from where, the bytes.
type Datagram = (SocketAddr, SocketAddr, Vec<u8>);

async fn run(sockets: HashMap<SocketAddr, Arc<UdpSocket>>, mut rx: mpsc::Receiver<Msg>, open: Arc<Open>) {
    let (dtx, mut drx) = mpsc::channel::<Datagram>(1024);
    for (addr, socket) in &sockets {
        let (addr, socket, dtx) = (*addr, socket.clone(), dtx.clone());
        tokio::spawn(async move {
            let mut buf = vec![0u8; 2000];
            while let Ok((n, source)) = socket.recv_from(&mut buf).await {
                if dtx.send((addr, source, buf[..n].to_vec())).await.is_err() {
                    break;
                }
            }
        });
    }
    let addrs: Vec<SocketAddr> = sockets.keys().copied().collect();
    let mut peers: HashMap<u64, Peer> = HashMap::new();
    loop {
        // every change to an Rtc is followed by draining its output, down to when it next wants a timeout;
        // a frame waiting for room in the send buffer goes on first (acknowledgements came in as datagrams)
        let mut deadline = Instant::now() + Duration::from_secs(1);
        for (id, peer) in peers.iter_mut() {
            if !peer.queue.is_empty() && peer.front_since.elapsed() > STALL {
                tracing::info!(session = id, "WebRTC: a frame {STALL:?} at the queue's front; the socket takes the video");
                peer.close_reason = "Server queue stalled";
                peer.rtc.disconnect(); // and the peer goes below, with its channel's claim on the frames
            }
            flush(peer, &open, *id);
            loop {
                match peer.rtc.poll_output() {
                    Ok(Output::Timeout(t)) => {
                        deadline = deadline.min(t);
                        break;
                    }
                    Ok(Output::Transmit(t)) => {
                        let socket = sockets.get(&t.source).or_else(|| sockets.values().next()).unwrap();
                        let _ = socket.send_to(&t.contents, t.destination).await;
                    }
                    Ok(Output::Event(e)) => event(*id, peer, e, &open),
                    Err(e) => {
                        tracing::warn!(session = id, "WebRTC: {e}");
                        peer.rtc.disconnect();
                        break;
                    }
                }
            }
        }
        peers.retain(|id, p| {
            let alive = p.rtc.is_alive();
            if !alive {
                if let Some(claim) = open.lock().unwrap().get_mut(id) { claim.active = false; }
                let _ = p.reply.try_send(protocol::rtc(&serde_json::json!({ "close": true, "g": p.g, "reason": p.close_reason })));
                tracing::debug!(session = id, "WebRTC: peer connection over");
            }
            alive
        });
        tokio::select! {
            Some((destination, source, bytes)) = drx.recv() => {
                let Ok(contents) = bytes.as_slice().try_into() else { continue };
                let input = Input::Receive(Instant::now(), Receive { proto: Protocol::Udp, source, destination, contents });
                if let Some(peer) = peers.values_mut().find(|p| p.rtc.accepts(&input))
                    && let Err(e) = peer.rtc.handle_input(input)
                {
                    tracing::debug!("WebRTC: {e}");
                }
            }
            _ = tokio::time::sleep_until(deadline.into()) => {
                let now = Instant::now();
                for peer in peers.values_mut() {
                    let _ = peer.rtc.handle_input(Input::Timeout(now));
                }
            }
            msg = rx.recv() => match msg {
                Some(Msg::Offer { session, sdp, g, reply, endpoint }) => {
                    peers.remove(&session);
                    open.lock().unwrap().remove(&session);
                    match answer(&sdp, g, &addrs, reply.clone(), endpoint.as_deref()) {
                        Ok(peer) => {
                            peers.insert(session, peer); // a second offer replaces the first connection
                        }
                        Err(e) => {
                            tracing::warn!(session, "WebRTC offer refused: {e:#}");
                            let _ = reply.try_send(protocol::rtc(&serde_json::json!({ "close": true, "g": g, "reason": "Offer rejected" })));
                        }
                    }
                }
                Some(Msg::Frame { session, data }) => {
                    if let Some(peer) = peers.get_mut(&session) && peer.channel.is_some() {
                        // a keyframe replaces what waits, the frame in flight included (its number is spent)
                        if data.get(1).is_some_and(|f| f & 1 != 0) {
                            dropped(&open, session, peer.queue.len() as u32);
                            peer.queue.clear();
                            peer.sent = 0;
                            peer.frame_id = peer.frame_id.wrapping_add(1);
                        }
                        if peer.queue.len() < QUEUE {
                            peer.queue.push_back(data);
                            if peer.queue.len() == 1 {
                                peer.front_since = Instant::now();
                            }
                            flush(peer, &open, session);
                        } else {
                            dropped(&open, session, 1);
                        }
                    }
                }
                Some(Msg::Close { session, g, reply }) => {
                    let mut claims = open.lock().unwrap();
                    let claimed = claims.get(&session).is_some_and(|c| g.is_none() || g == Some(c.g));
                    if g.is_none() || peers.get(&session).is_some_and(|p| g == Some(p.g)) {
                        peers.remove(&session);
                    }
                    if claimed { claims.remove(&session); }
                    if let Some(reply) = reply { let _ = reply.send(claimed); }
                }
                None => return,
            },
        }
    }
}

/// ICE lite uses local candidates internally. The wire answer advertises the page endpoint unless
/// --rtc-addr supplied the advertised address. The answer goes out through `reply`.
fn answer(sdp: &str, g: u64, addrs: &[SocketAddr], reply: mpsc::Sender<Bytes>, endpoint: Option<&[SocketAddr]>) -> Result<Peer> {
    let offer = SdpOffer::from_sdp_string(sdp).context("offer")?;
    let mut rtc = Rtc::builder().set_ice_lite(true).set_stats_interval(None).build(Instant::now());
    for addr in addrs {
        if let Ok(c) = Candidate::host(*addr, "udp") {
            rtc.add_local_candidate(c);
        }
    }
    let answer = rtc.sdp_api().accept_offer(offer).context("accept offer")?.to_sdp_string();
    let answer = match endpoint {
        Some(addrs) => advertise_endpoint(&answer, addrs)?,
        None => answer,
    };
    let _ = reply.try_send(protocol::rtc(&serde_json::json!({ "answer": answer, "g": g })));
    Ok(Peer { rtc, channel: None, frame_id: 0, queue: VecDeque::new(), sent: 0, front_since: Instant::now(), reply, g, close_reason: "Peer connection closed" })
}

fn event(session: u64, peer: &mut Peer, e: Event, open: &Open) {
    match e {
        Event::ChannelOpen(cid, label) => {
            tracing::info!(session, label, "WebRTC: data channel open");
            peer.channel = Some(cid);
            open.lock().unwrap().insert(session, Claim { g: peer.g, active: true, dropped: 0, queued: 0 });
        }
        Event::ChannelClose(cid) => {
            if peer.channel == Some(cid) {
                peer.channel = None;
                peer.queue.clear();
                peer.sent = 0;
                if let Some(claim) = open.lock().unwrap().get_mut(&session) { claim.active = false; }
            }
        }
        Event::IceConnectionStateChange(state) => {
            tracing::debug!(session, ?state, "WebRTC: ICE");
            if state == str0m::IceConnectionState::Disconnected {
                peer.rtc.disconnect(); // the UDP path died: frames go back to the socket at once, not when the browser gives up
            }
        }
        _ => {}
    }
}

fn dropped(open: &Open, session: u64, count: u32) {
    if let Some(claim) = open.lock().unwrap().get_mut(&session) {
        claim.dropped += count;
    }
}

/// The queue's frames, front first, as fragments `[FRAGMENT][u32 id][u16 index][u16 count]` then the bytes,
/// as far as the send buffer takes them; the rest waits for the next round. The depth left is the
/// session's to see.
fn flush(peer: &mut Peer, open: &Open, session: u64) {
    let mut msg = Vec::with_capacity(FRAGMENT + 9);
    'frames: while let (Some(cid), Some(data)) = (peer.channel, peer.queue.front()) {
        let count = data.chunks(FRAGMENT).count();
        while peer.sent < count {
            let chunk = &data[peer.sent * FRAGMENT..data.len().min((peer.sent + 1) * FRAGMENT)];
            msg.clear();
            msg.push(protocol::FRAGMENT);
            msg.extend_from_slice(&peer.frame_id.to_le_bytes());
            msg.extend_from_slice(&(peer.sent as u16).to_le_bytes());
            msg.extend_from_slice(&(count as u16).to_le_bytes());
            msg.extend_from_slice(chunk);
            let Some(mut channel) = peer.rtc.channel(cid) else { break 'frames };
            match channel.write(true, &msg) {
                Ok(true) => peer.sent += 1,
                Ok(false) => break 'frames, // no room: the browser's acknowledgements make some
                Err(_) => break 'frames,    // the frame stays at the front, and a channel that keeps refusing is given up above
            }
        }
        peer.queue.pop_front();
        peer.sent = 0;
        peer.front_since = Instant::now();
        peer.frame_id = peer.frame_id.wrapping_add(1);
    }
    if let Some(claim) = open.lock().unwrap().get_mut(&session) {
        claim.queued = peer.queue.len();
    }
}

#[cfg(test)]
mod endpoint_tests {
    use super::*;

    #[tokio::test]
    async fn endpoint_failure_does_not_wait_on_full_events_and_override_skips_dns() {
        let (tx, mut rx) = mpsc::channel(1);
        let (reply, _events) = mpsc::channel(1);
        reply.try_send(protocol::rtc(&serde_json::json!({}))).unwrap();
        let mut hub = Hub { tx, open: Default::default(), ice_servers: Default::default(), page_endpoint: true };
        let invalid = serde_json::json!({"host": "", "port": 0});
        tokio::time::timeout(Duration::from_millis(100), hub.offer(1, String::new(), 1, reply.clone(), Some(&invalid))).await.unwrap();
        assert!(rx.try_recv().is_err());
        tokio::time::timeout(Duration::from_millis(100), hub.offer(1, String::new(), 1, reply.clone(), None)).await.unwrap();
        assert!(rx.try_recv().is_err());
        hub.page_endpoint = false;
        hub.offer(1, String::new(), 1, reply.clone(), Some(&invalid)).await;
        assert!(matches!(rx.recv().await, Some(Msg::Offer { endpoint: None, .. })));
        hub.offer(1, String::new(), 1, reply, None).await;
        assert!(matches!(rx.recv().await, Some(Msg::Offer { endpoint: None, .. })));
    }

    #[tokio::test]
    async fn endpoint_resolution_and_wire_candidates() {
        assert!(resolve_endpoint(None).await.is_err());
        for value in [serde_json::json!({"host": "", "port": 443}), serde_json::json!({"host": "localhost", "port": 0}), serde_json::json!({"host": "localhost", "port": 65536})] {
            assert!(resolve_endpoint(Some(&value)).await.is_err());
        }
        assert!(resolve_endpoint(Some(&serde_json::json!({"host": "0.0.0.0", "port": 443}))).await.is_err());
        let mut endpoints = vec![vec!["192.0.2.1:443".parse().unwrap(), "[2001:db8::1]:443".parse().unwrap()]];
        for host in ["127.0.0.1", "::1", "localhost"] {
            let value = serde_json::json!({"host": host, "port": 9443});
            let addrs = resolve_endpoint(Some(&value)).await.unwrap();
            assert!(addrs.iter().all(|addr| addr.port() == 9443 && addr.ip().is_loopback()));
            endpoints.push(addrs);
        }
        assert!(advertise_endpoint("v=0\r\n", &endpoints[0]).is_err());
        for addrs in endpoints {
            let sdp = "v=0\r\nm=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\na=ice-ufrag:test\r\na=candidate:1 1 udp 1 192.0.2.1 8443 typ host\r\na=candidate:2 1 udp 1 192.0.2.2 8443 typ host\r\na=end-of-candidates\r\n";
            let wire = advertise_endpoint(sdp, &addrs).unwrap();
            assert!(wire.contains("a=ice-ufrag:test\r\n"));
            assert!(wire.ends_with("a=end-of-candidates\r\n"));
            let candidates: Vec<_> = wire.lines().filter_map(|l| l.strip_prefix("a=candidate:")).map(|c| Candidate::from_sdp_string(&format!("candidate:{c}")).unwrap().addr()).collect();
            assert_eq!(candidates, addrs);
        }
    }
}
