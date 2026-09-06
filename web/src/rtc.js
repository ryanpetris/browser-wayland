// The video over a WebRTC data channel, ordered and reliable like the socket: a lost packet is
// retransmitted and the frames behind it wait a round trip. (Unordered, the frame behind arrived first,
// the page took the wait for a gap and asked for a keyframe, and the keyframes were what the link then
// spent itself on.) Measured against the socket it is even on a clean link and behind it under loss
// (see the README), so it is a choice, not the default. The page offers one channel, the server (ICE
// lite) answers with its addresses;
// frames above 16 kB come as numbered fragments, reassembled here and handed to the same message handler
// as the socket's.
export const RTC_TIMING = { gather: 1500, gatherWithServers: 5000, attempt: 10000, retry: 1000, retryMax: 30000, healthy: 10000 };

export function openRtc({ iceServers, g, signal, onMessage, onOpen, onClose }) {
  const pc = new RTCPeerConnection({ iceServers });
  let ch;
  try { ch = pc.createDataChannel('video', { ordered: true }); } catch (e) { pc.close(); throw e; }
  let offered = false, closed = false, gatherTimer;
  const fail = reason => { if (!closed) onClose(reason); };
  ch.binaryType = 'arraybuffer';
  const parts = new Map(); // frame id -> { got, chunks } while its fragments come in
  let incomplete = 0; // frames given up on: the server abandoned one half-sent
  ch.onmessage = e => {
    if (closed) return;
    const dv = new DataView(e.data); // every message is a fragment, a whole frame a fragment of one
    const id = dv.getUint32(1, true), index = dv.getUint16(5, true), count = dv.getUint16(7, true);
    let p = parts.get(id);
    if (!p) {
      p = { got: 0, chunks: new Array(count) };
      parts.set(id, p);
      if (parts.size > 8) { parts.delete(parts.keys().next().value); incomplete++; } // a frame that lost a fragment is forgotten
    }
    p.chunks[index] = new Uint8Array(e.data, 9);
    if (++p.got < count) return;
    parts.delete(id);
    const out = new Uint8Array(p.chunks.reduce((n, c) => n + c.length, 0));
    let o = 0;
    for (const c of p.chunks) { out.set(c, o); o += c.length; }
    onMessage(out.buffer);
  };
  ch.onopen = () => { if (!closed) onOpen(); };
  ch.onclose = () => fail('Data channel closed');
  ch.onerror = () => fail('Data channel failed');
  pc.onconnectionstatechange = () => {
    if (pc.connectionState === 'failed' || pc.connectionState === 'disconnected') fail('Peer connection ' + pc.connectionState);
  };
  // the offer goes out with the candidates in it, once gathering is done (host ones come at once; a STUN
  // or TURN one takes a round trip or an allocation, so the wait is longer with servers configured): the
  // server answers with its own and does no trickle; `g` comes back with the answer
  const offer = () => { if (!offered && !closed && pc.localDescription) { offered = true; clearTimeout(gatherTimer); signal({ offer: pc.localDescription.sdp, g }); } };
  pc.onicegatheringstatechange = () => { if (pc.iceGatheringState === 'complete') offer(); };
  pc.createOffer().then(o => { if (!closed) return pc.setLocalDescription(o); }).then(() => {
    if (!closed && !offered) gatherTimer = setTimeout(offer, iceServers.length ? RTC_TIMING.gatherWithServers : RTC_TIMING.gather);
  }).catch(() => fail('Offer failed'));
  return {
    answer: sdp => { if (!closed) pc.setRemoteDescription({ type: 'answer', sdp }).catch(() => fail('Answer rejected')); },
    close: () => {
      closed = true;
      clearTimeout(gatherTimer);
      ch.onopen = ch.onclose = ch.onerror = ch.onmessage = null;
      pc.onicegatheringstatechange = pc.onconnectionstatechange = null;
      parts.clear();
      pc.close();
    },
    incomplete: () => incomplete,
    // the numbers the Statistics tab shows: the path's round trip, what the channel carried, frames lost to it
    stats: async () => {
      const out = { incomplete, rttMs: null, bytes: 0, messages: 0 };
      (await pc.getStats()).forEach(r => {
        if (r.type === 'candidate-pair' && r.nominated && r.currentRoundTripTime !== undefined) out.rttMs = r.currentRoundTripTime * 1000; // the pair in use, not any that worked
        if (r.type === 'data-channel') { out.bytes = r.bytesReceived; out.messages = r.messagesReceived; }
      });
      return out;
    },
  };
}
