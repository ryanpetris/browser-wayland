//! WebRTC data-channel transport for the video: the same messages the WebSocket carries, over a `video`
//! data channel the browser opens (unordered, reliable), for viewers who need UDP to reach the desktop
//! at all: through NAT, or through a TURN relay. It is a viewer's choice, not the default, because the
//! socket carries the picture better under packet loss (measured in the README). str0m does ICE (lite,
//! host candidates), DTLS and SCTP without I/O of its own; one hub task drives every session's peer
//! connection over one UDP socket per local address (a received packet's destination must be one of the
//! candidates, so a socket per address knows it). Signalling goes over the session's WebSocket (`RTC`
//! messages: the browser's offer, our answer); the frame path in `ws.rs` hands frames to the hub while
//! the session's channel is open and to the socket otherwise. A frame goes as numbered fragments the page
//! reassembles, written as the SCTP send buffer (128 kB, freed by the browser's acknowledgements) has
//! room: a keyframe of a few hundred kB takes a few rounds, and frames that arrive meanwhile are dropped
//! (a keyframe replaces whatever waits, though what was written of it still has to go out), which the
//! session's rate controller hears of. A fragment on its way is retransmitted if it is lost, so what the
//! page misses is what was dropped here, never what the network ate.

use std::{
    collections::HashMap,
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
    /// Sessions whose `video` channel is open right now, with the frames dropped for each since it last asked.
    open: Arc<Mutex<HashMap<u64, u32>>>,
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

    pub fn is_open(&self, session: u64) -> bool {
        self.open.lock().unwrap().contains_key(&session)
    }

    /// Frames of the session dropped since the last call: congestion, to its rate controller.
    pub fn take_dropped(&self, session: u64) -> u32 {
        self.open.lock().unwrap().get_mut(&session).map(std::mem::take).unwrap_or(0)
    }

    /// Signalling waits for room in the queue: an offer without an answer, or a close that never arrives,
    /// would be a stuck viewer or a lingering peer.
    pub async fn offer(&self, session: u64, sdp: String, g: serde_json::Value, reply: mpsc::Sender<Bytes>) {
        let _ = self.tx.send(Msg::Offer { session, sdp, g, reply }).await;
    }

    /// A frame for the session's channel; one that doesn't fit the hub's queue is dropped (the page asks
    /// for a keyframe when it sees the gap).
    pub fn frame(&self, session: u64, data: Bytes) {
        if self.tx.try_send(Msg::Frame { session, data }).is_err()
            && let Some(dropped) = self.open.lock().unwrap().get_mut(&session)
        {
            *dropped += 1;
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
    /// The frame being written, and how many of its fragments are down the channel.
    pending: Option<(Bytes, usize)>,
}

/// One datagram in: the candidate address it came to, from where, the bytes.
type Datagram = (SocketAddr, SocketAddr, Vec<u8>);

async fn run(sockets: HashMap<SocketAddr, Arc<UdpSocket>>, mut rx: mpsc::Receiver<Msg>, open: Arc<Mutex<HashMap<u64, u32>>>) {
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
            flush(peer);
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
                        // a frame still on its way keeps its place unless a keyframe comes (the page needs that one)
                        let key = data.get(1).is_some_and(|f| f & 1 != 0);
                        if peer.pending.is_none() || key {
                            if peer.pending.take().is_some() {
                                dropped(&open, session);
                            }
                            peer.frame_id = peer.frame_id.wrapping_add(1);
                            peer.pending = Some((data, 0));
                            flush(peer);
                        } else {
                            dropped(&open, session);
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
    Ok(Peer { rtc, channel: None, frame_id: 0, pending: None })
}

fn event(session: u64, peer: &mut Peer, e: Event, open: &Mutex<HashMap<u64, u32>>) {
    match e {
        Event::ChannelOpen(cid, label) => {
            tracing::info!(session, label, "WebRTC: data channel open");
            peer.channel = Some(cid);
            open.lock().unwrap().insert(session, 0);
        }
        Event::ChannelClose(cid) => {
            if peer.channel == Some(cid) {
                peer.channel = None;
                peer.pending = None;
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

fn dropped(open: &Mutex<HashMap<u64, u32>>, session: u64) {
    if let Some(n) = open.lock().unwrap().get_mut(&session) {
        *n += 1;
    }
}

/// The pending frame's remaining fragments, `[FRAGMENT][u32 id][u16 index][u16 count]` then the bytes, as
/// far as the send buffer takes them; the rest waits for the next round.
fn flush(peer: &mut Peer) {
    let (Some(cid), Some((data, next))) = (peer.channel, peer.pending.as_mut()) else { return };
    let count = data.chunks(FRAGMENT).count();
    let mut msg = Vec::with_capacity(FRAGMENT + 9);
    while *next < count {
        let chunk = &data[*next * FRAGMENT..data.len().min((*next + 1) * FRAGMENT)];
        msg.clear();
        msg.push(protocol::FRAGMENT);
        msg.extend_from_slice(&peer.frame_id.to_le_bytes());
        msg.extend_from_slice(&(*next as u16).to_le_bytes());
        msg.extend_from_slice(&(count as u16).to_le_bytes());
        msg.extend_from_slice(chunk);
        let Some(mut channel) = peer.rtc.channel(cid) else { break };
        match channel.write(true, &msg) {
            Ok(true) => *next += 1,
            Ok(false) => return, // no room: the browser's acknowledgements make some
            Err(_) => break,
        }
    }
    peer.pending = None;
}
