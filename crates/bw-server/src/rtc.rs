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
use bw_core::Bytes;
use str0m::{Candidate, Event, Input, Output, Rtc, change::SdpOffer, channel::ChannelId, net::{Protocol, Receive}};
use tokio::{net::UdpSocket, sync::mpsc};

use crate::protocol;

/// Fragments of this size go down the channel: every browser takes them, and a lost one is retransmitted.
const FRAGMENT: usize = 16 * 1024;

/// What the CLI decided: the UDP port, the address to advertise, and the ICE servers the browser should use.
pub struct Config {
    pub port: u16,
    /// The one address browsers reach us at, when it isn't ours (a Docker bridge maps the host's port to
    /// the container); without it every local address is a candidate.
    pub addr: Option<IpAddr>,
    /// STUN and TURN servers as the page's `RTCPeerConnection` wants them (`urls`, `username`, `credential`).
    pub ice_servers: Vec<serde_json::Value>,
}

enum Msg {
    /// A session's browser offered (`g` numbers its attempt; the answer carries it back through `reply`,
    /// the session's own event queue).
    Offer { session: u64, sdp: String, g: serde_json::Value, reply: mpsc::Sender<Bytes> },
    /// A frame for a session's channel.
    Frame { session: u64, data: Bytes },
    /// The session ended, or went back to its socket.
    Close { session: u64 },
}

/// A session's way to the hub; cheap to clone.
#[derive(Clone)]
pub struct Hub {
    tx: mpsc::Sender<Msg>,
    /// Sessions whose `video` channel is open right now, with the frames dropped for each since it last asked
    /// and the frames waiting in its queue.
    open: Arc<Open>,
    pub ice_servers: Arc<Vec<serde_json::Value>>,
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
        let hub = Hub { tx, open: Default::default(), ice_servers: Arc::new(cfg.ice_servers) };
        tokio::spawn(run(sockets, rx, hub.open.clone()));
        tracing::info!(port = cfg.port, "WebRTC: data channels on UDP");
        Ok(hub)
    }

    /// The session's channel, while it is open, under pressure: frames dropped since the last call, and
    /// frames waiting in its queue now; congestion, to its rate controller.
    pub fn pressure(&self, session: u64) -> Option<(u32, usize)> {
        self.open.lock().unwrap().get_mut(&session).map(|(dropped, queued)| (std::mem::take(dropped), *queued))
    }

    /// Signalling waits for room in the queue: an offer without an answer, or a close that never arrives,
    /// would be a stuck viewer or a lingering peer.
    pub async fn offer(&self, session: u64, sdp: String, g: serde_json::Value, reply: mpsc::Sender<Bytes>) {
        let _ = self.tx.send(Msg::Offer { session, sdp, g, reply }).await;
    }

    /// A frame for the session's channel; one that doesn't fit the hub's mailbox is dropped (the page asks
    /// for a keyframe when it sees the gap).
    pub fn frame(&self, session: u64, data: Bytes) {
        if self.tx.try_send(Msg::Frame { session, data }).is_err() {
            dropped(&self.open, session, 1);
        }
    }

    pub async fn close(&self, session: u64) {
        let _ = self.tx.send(Msg::Close { session }).await;
    }
}

/// Every address a browser could reach us at (the certificate's too): the non-loopback ones (a link-local
/// IPv6 address can't be bound without its scope).
fn local_ips() -> Vec<IpAddr> {
    if_addrs::get_if_addrs()
        .unwrap_or_default()
        .into_iter()
        .map(|i| i.ip())
        .filter(|ip| !ip.is_loopback() && !matches!(ip, IpAddr::V6(v6) if v6.is_unicast_link_local()))
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
    g: serde_json::Value,
}

/// A frame at the front of the queue this long, moving or not, is too slow for a desktop: the channel
/// is given up and the socket takes the video.
const STALL: Duration = Duration::from_secs(3);

/// Frames a channel's queue holds before a new one is dropped: half a second at 60 fps.
// ponytail: a cap; blocking the session as the socket does if a viewer wants the latency bounded tighter
const QUEUE: usize = 30;

/// Per open channel: frames dropped since the session last asked, frames waiting in the queue.
type Open = Mutex<HashMap<u64, (u32, usize)>>;

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
                let _ = peer.reply.try_send(protocol::rtc(&serde_json::json!({ "close": true, "g": peer.g })));
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
                open.lock().unwrap().remove(id);
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
                Some(Msg::Offer { session, sdp, g, reply }) => {
                    open.lock().unwrap().remove(&session);
                    match answer(&sdp, g, &addrs, reply) {
                        Ok(peer) => {
                            peers.insert(session, peer); // a second offer replaces the first connection
                        }
                        Err(e) => tracing::warn!(session, "WebRTC offer refused: {e:#}"),
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
                Some(Msg::Close { session }) => {
                    peers.remove(&session);
                    open.lock().unwrap().remove(&session);
                }
                None => return,
            },
        }
    }
}

/// A peer connection for a browser's offer: ICE lite with our addresses as host candidates; the answer
/// goes out through `reply`.
fn answer(sdp: &str, g: serde_json::Value, addrs: &[SocketAddr], reply: mpsc::Sender<Bytes>) -> Result<Peer> {
    let offer = SdpOffer::from_sdp_string(sdp).context("offer")?;
    let mut rtc = Rtc::builder().set_ice_lite(true).set_stats_interval(None).build(Instant::now());
    for addr in addrs {
        if let Ok(c) = Candidate::host(*addr, "udp") {
            rtc.add_local_candidate(c);
        }
    }
    let answer = rtc.sdp_api().accept_offer(offer).context("accept offer")?;
    let _ = reply.try_send(protocol::rtc(&serde_json::json!({ "answer": answer.to_sdp_string(), "g": g })));
    Ok(Peer { rtc, channel: None, frame_id: 0, queue: VecDeque::new(), sent: 0, front_since: Instant::now(), reply, g })
}

fn event(session: u64, peer: &mut Peer, e: Event, open: &Open) {
    match e {
        Event::ChannelOpen(cid, label) => {
            tracing::info!(session, label, "WebRTC: data channel open");
            peer.channel = Some(cid);
            open.lock().unwrap().insert(session, (0, 0));
        }
        Event::ChannelClose(cid) => {
            if peer.channel == Some(cid) {
                peer.channel = None;
                peer.queue.clear();
                peer.sent = 0;
                open.lock().unwrap().remove(&session);
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
    if let Some((n, _)) = open.lock().unwrap().get_mut(&session) {
        *n += count;
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
    if let Some((_, queued)) = open.lock().unwrap().get_mut(&session) {
        *queued = peer.queue.len();
    }
}
