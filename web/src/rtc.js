// The video over a WebRTC data channel (unordered, no retransmits): a lost packet costs one frame instead
// of stalling everything behind it, and UDP through ICE reaches a server across NAT. The page offers with
// one channel, the server (ICE lite) answers with its addresses; frames above 16 kB come as numbered
// fragments, reassembled here and handed to the same message handler as the socket's.
export function openRtc({ iceServers, g, signal, onMessage, onOpen, onClose }) {
  const pc = new RTCPeerConnection({ iceServers });
  const ch = pc.createDataChannel('video', { ordered: false, maxRetransmits: 0 });
  ch.binaryType = 'arraybuffer';
  const parts = new Map(); // frame id -> { got, chunks } while its fragments come in
  ch.onmessage = e => {
    const dv = new DataView(e.data); // every message is a fragment, a whole frame a fragment of one
    const id = dv.getUint32(1, true), index = dv.getUint16(5, true), count = dv.getUint16(7, true);
    let p = parts.get(id);
    if (!p) {
      p = { got: 0, chunks: new Array(count) };
      parts.set(id, p);
      if (parts.size > 8) parts.delete(parts.keys().next().value); // a frame that lost a fragment is forgotten
    }
    p.chunks[index] = new Uint8Array(e.data, 9);
    if (++p.got < count) return;
    parts.delete(id);
    const out = new Uint8Array(p.chunks.reduce((n, c) => n + c.length, 0));
    let o = 0;
    for (const c of p.chunks) { out.set(c, o); o += c.length; }
    onMessage(out.buffer);
  };
  ch.onopen = onOpen;
  ch.onclose = onClose;
  // the offer goes out with the candidates in it, once gathering is done (host ones come at once; a STUN
  // or TURN one takes a round trip or an allocation, so the wait is longer with servers configured): the
  // server answers with its own and does no trickle; `g` comes back with the answer
  let offered = false, closed = false;
  const offer = () => { if (!offered && !closed && pc.localDescription) { offered = true; signal({ offer: pc.localDescription.sdp, g }); } };
  pc.onicegatheringstatechange = () => { if (pc.iceGatheringState === 'complete') offer(); };
  pc.createOffer().then(o => pc.setLocalDescription(o)).then(() => setTimeout(offer, iceServers.length ? 5000 : 1500)).catch(onClose);
  return {
    answer: sdp => pc.setRemoteDescription({ type: 'answer', sdp }).catch(e => console.warn('WebRTC answer:', e)),
    close: () => { closed = true; pc.close(); },
    stats: () => pc.getStats(),
  };
}
