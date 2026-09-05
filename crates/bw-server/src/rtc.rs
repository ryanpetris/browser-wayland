//! WebRTC data-channel transport for the video: the same messages the WebSocket carries, over a `video`
//! data channel the browser opens (unordered, no retransmits), so a lost packet costs one frame instead
//! of stalling everything after it, and the transport reports real loss and RTT. str0m does ICE (lite,
//! host candidates), DTLS and SCTP without I/O of its own; one hub task drives every session's peer
//! connection over one UDP socket per local address (a received packet's destination must be one of the
//! candidates, so a socket per address knows it). Signalling goes over the session's WebSocket (`RTC`
//! messages: the browser's offer, our answer); the frame path in `ws.rs` hands frames to the hub while
//! the session's channel is open and to the socket otherwise. A message above the fragment size goes as
//! numbered fragments the page reassembles (a keyframe is a few hundred kB; SCTP takes 64 kB at most).

use std::{
    collections::{HashMap, HashSet},
    net::{IpAddr, SocketAddr},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use bw_core::Bytes;
use str0m::{Candidate, Event, Input, Output, Rtc, change::SdpOffer, channel::ChannelId, net::{Protocol, Receive}};
use tokio::{net::UdpSocket, sync::mpsc};

use crate::protocol;

/// Fragments of this size go down the channel: every browser takes them, and a lost one costs one frame.
const FRAGMENT: usize = 16 * 1024;

/// What the CLI decided: the UDP port and the ICE servers the browser should use.
pub struct Config {
    pub port: u16,
    /// STUN and TURN servers as the page's `RTCPeerConnection` wants them (`urls`, `username`, `credential`).
    pub ice_servers: Vec<serde_json::Value>,
}

enum Msg {
    /// A session's browser offered; the answer goes back through `reply` (the session's own event queue).
    Offer { session: u64, sdp: String, reply: mpsc::Sender<Bytes> },
    /// A frame for a session's channel.
    Frame { session: u64, data: Bytes },
    /// The session ended, or went back to its socket.
    Close { session: u64 },
}

/// A session's way to the hub; cheap to clone.
#[derive(Clone)]
pub struct Hub {
    tx: mpsc::Sender<Msg>,
    /// Sessions whose `video` channel is open right now.
    open: Arc<Mutex<HashSet<u64>>>,
    pub ice_servers: Arc<Vec<serde_json::Value>>,
}

impl Hub {
    /// Binds the port on every local address and starts the hub task.
    pub async fn start(cfg: Config) -> Result<Hub> {
        let mut sockets = HashMap::new();
        for ip in local_ips() {
            let addr = SocketAddr::new(ip, cfg.port);
            match UdpSocket::bind(addr).await {
                Ok(s) => {
                    sockets.insert(addr, Arc::new(s));
                }
                Err(e) => tracing::warn!("WebRTC: can't listen on {addr}: {e}"),
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
        self.open.lock().unwrap().contains(&session)
    }

    pub fn offer(&self, session: u64, sdp: String, reply: mpsc::Sender<Bytes>) {
        let _ = self.tx.try_send(Msg::Offer { session, sdp, reply });
    }

    /// A frame for the session's channel; one that doesn't fit the hub's queue is dropped (the page asks
    /// for a keyframe when it sees the gap).
    pub fn frame(&self, session: u64, data: Bytes) {
        let _ = self.tx.try_send(Msg::Frame { session, data });
    }

    pub fn close(&self, session: u64) {
        let _ = self.tx.try_send(Msg::Close { session });
    }
}

/// Every address a browser could reach us at (the certificate's too): the non-loopback ones.
fn local_ips() -> Vec<IpAddr> {
    if_addrs::get_if_addrs().unwrap_or_default().into_iter().map(|i| i.ip()).filter(|ip| !ip.is_loopback()).collect()
}

struct Peer {
    rtc: Rtc,
    channel: Option<ChannelId>,
    /// Numbers the fragmented messages, so the page can tell one frame's fragments from the next's.
    frame_id: u32,
}

/// One datagram in: which socket (its local address is the candidate hit), from where, the bytes.
type Datagram = (SocketAddr, SocketAddr, Vec<u8>);

async fn run(sockets: HashMap<SocketAddr, Arc<UdpSocket>>, mut rx: mpsc::Receiver<Msg>, open: Arc<Mutex<HashSet<u64>>>) {
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
        // every change to an Rtc is followed by draining its output, down to when it next wants a timeout
        let mut deadline = Instant::now() + Duration::from_secs(1);
        for (id, peer) in peers.iter_mut() {
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
                Some(Msg::Offer { session, sdp, reply }) => {
                    open.lock().unwrap().remove(&session);
                    match answer(&sdp, &addrs, reply) {
                        Ok(peer) => {
                            peers.insert(session, peer); // a second offer replaces the first connection
                        }
                        Err(e) => tracing::warn!(session, "WebRTC offer refused: {e:#}"),
                    }
                }
                Some(Msg::Frame { session, data }) => {
                    if let Some(peer) = peers.get_mut(&session) && let Some(cid) = peer.channel {
                        send(peer, cid, &data);
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
fn answer(sdp: &str, addrs: &[SocketAddr], reply: mpsc::Sender<Bytes>) -> Result<Peer> {
    let offer = SdpOffer::from_sdp_string(sdp).context("offer")?;
    let mut rtc = Rtc::builder().set_ice_lite(true).set_stats_interval(None).build(Instant::now());
    for addr in addrs {
        if let Ok(c) = Candidate::host(*addr, "udp") {
            rtc.add_local_candidate(c);
        }
    }
    let answer = rtc.sdp_api().accept_offer(offer).context("accept offer")?;
    let _ = reply.try_send(protocol::rtc(&serde_json::json!({ "answer": answer.to_sdp_string() })));
    Ok(Peer { rtc, channel: None, frame_id: 0 })
}

fn event(session: u64, peer: &mut Peer, e: Event, open: &Mutex<HashSet<u64>>) {
    match e {
        Event::ChannelOpen(cid, label) => {
            tracing::info!(session, label, "WebRTC: data channel open");
            peer.channel = Some(cid);
            open.lock().unwrap().insert(session);
        }
        Event::ChannelClose(cid) => {
            if peer.channel == Some(cid) {
                peer.channel = None;
                open.lock().unwrap().remove(&session);
            }
        }
        Event::IceConnectionStateChange(state) => tracing::debug!(session, ?state, "WebRTC: ICE"),
        _ => {}
    }
}

/// One message down the channel, as fragments: `[FRAGMENT][u32 id][u16 index][u16 count]` then the bytes.
/// A write the channel has no room for drops the rest of the frame (the page asks for a keyframe).
fn send(peer: &mut Peer, cid: ChannelId, data: &[u8]) {
    peer.frame_id = peer.frame_id.wrapping_add(1);
    let count = data.chunks(FRAGMENT).count() as u16;
    let mut msg = Vec::with_capacity(FRAGMENT + 9);
    for (index, chunk) in data.chunks(FRAGMENT).enumerate() {
        msg.clear();
        msg.push(protocol::FRAGMENT);
        msg.extend_from_slice(&peer.frame_id.to_le_bytes());
        msg.extend_from_slice(&(index as u16).to_le_bytes());
        msg.extend_from_slice(&count.to_le_bytes());
        msg.extend_from_slice(chunk);
        let Some(mut channel) = peer.rtc.channel(cid) else { return };
        if !matches!(channel.write(true, &msg), Ok(true)) {
            return;
        }
    }
}
