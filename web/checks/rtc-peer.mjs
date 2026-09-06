// Run inside the Docker rig: node checks/rtc-peer.mjs.
import assert from 'node:assert/strict';
import { openRtc, pageEndpoint } from '../src/rtc.js';

for (const [url, address, port] of [
  ['https://desktop.example:9443/', 'desktop.example', '9443'],
  ['https://192.0.2.1/', '192.0.2.1', '443'],
  ['http://localhost/', 'localhost', '80'],
  ['https://[2001:db8::1]:9443/', '2001:db8::1', '9443'],
]) {
  assert.deepEqual(pageEndpoint(new URL(url)), { host: address, port: Number(port) });
}

let pc;
const timers = new Map();
let timerId = 0;
globalThis.setTimeout = (fn, ms) => { timers.set(++timerId, { fn, ms }); return timerId; };
globalThis.clearTimeout = id => timers.delete(id);
class Peer {
  constructor() { pc = this; this.channel = {}; this.localCalls = 0; this.closed = false; }
  createDataChannel(name, options) { assert.equal(name, 'video'); assert.equal(options.ordered, true); return this.channel; }
  createOffer() { return new Promise(resolve => { this.offerReady = resolve; }); }
  async setLocalDescription(o) { this.localCalls++; this.localDescription = o; if (!this.gatherPending) { this.iceGatheringState = 'complete'; this.onicegatheringstatechange?.(); } }
  async setRemoteDescription(answer) { this.remoteDescription = answer; throw new Error('invalid answer'); }
  close() { this.closed = true; this.channel.onclose?.(); }
}
globalThis.RTCPeerConnection = Peer;
const settle = async () => { for (let i = 0; i < 6; i++) await Promise.resolve(); };
let messages = 0, opens = 0, failures = [], offers = [];
const start = () => openRtc({ iceServers: [], endpoint: { host: 'localhost', port: 8080 }, g: 7, signal: o => offers.push(o), onMessage: () => messages++, onOpen: () => opens++, onClose: reason => failures.push(reason) });
let peer = start();
const oldOpen = pc.channel.onopen, oldMessage = pc.channel.onmessage, oldClose = pc.channel.onclose;
peer.close();
pc.offerReady({ sdp: 'late' });
oldOpen(); oldMessage({}); oldClose();
await settle();
assert.equal(pc.localCalls, 0);
assert.deepEqual([messages, opens, failures.length, offers.length], [0, 0, 0, 0]);

peer = start();
pc.offerReady({ sdp: 'current' });
await settle();
assert.deepEqual(offers, [{ offer: 'current', g: 7, endpoint: { host: 'localhost', port: 8080 } }]);
pc.channel.onopen();
assert.equal(opens, 1);
peer.answer('bad');
assert.equal(pc.remoteDescription.sdp, 'bad');
await settle();
assert.deepEqual(failures, ['Answer rejected']);
pc.connectionState = 'failed'; pc.onconnectionstatechange();
assert.equal(failures.at(-1), 'Peer connection failed');
const before = failures.length;
peer.answer('late bad answer');
peer.close();
await settle();
assert.equal(failures.length, before);
assert(pc.closed);
assert.equal(pc.onconnectionstatechange, null);
assert.equal(pc.channel.onmessage, null);
peer = start();
pc.gatherPending = true;
pc.offerReady({ sdp: 'gathering' });
await settle();
assert.equal(timers.size, 1);
const delayedOffer = [...timers.values()][0].fn;
peer.close();
assert.equal(timers.size, 0);
delayedOffer();
assert.equal(offers.length, 1);
peer = start();
pc.gatherPending = true; pc.offerReady({ sdp: 'gathered later' });
await settle();
assert.equal(timers.size, 1);
pc.iceGatheringState = 'complete'; pc.onicegatheringstatechange();
assert.equal(timers.size, 0);
assert.equal(offers.length, 2);
peer.close();
console.log('RTC peer callback disposal, late gathering, rejected answers and connection failures passed');
