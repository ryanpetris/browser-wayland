// Called by the Docker isolation rig while both private desktops and the outside graph are live.
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';

const waitFor = async predicate => {
  for (let n = 0; n < 200; n++) {
    if (predicate()) return;
    await new Promise(resolve => setTimeout(resolve, 50));
  }
  throw new Error('mixer isolation condition timed out');
};
const viewers = [];
try {
  for (const [index, home] of process.argv.slice(2).entries()) {
    const token = (await readFile(home + '/config/browser-wayland/token', 'utf8')).trim();
    const socket = new WebSocket(`ws://127.0.0.1:${8090 + index}/ws`);
    socket.binaryType = 'arraybuffer';
    const viewer = { socket, state: null, error: '' };
    viewers.push(viewer);
    socket.onmessage = ({ data }) => {
      const bytes = new Uint8Array(data), text = new TextDecoder().decode(bytes.subarray(1));
      if (bytes[0] === 0x0f) viewer.state = JSON.parse(text);
      if (bytes[0] === 0x11) viewer.error = text;
    };
    await waitFor(() => socket.readyState === WebSocket.OPEN);
    const auth = new TextEncoder().encode(token), packet = new Uint8Array(auth.length + 1);
    packet[0] = 0x80; packet.set(auth, 1); socket.send(packet);
    socket.send(new Uint8Array([0x81, 0, 16, 5, 0])); // Software VP8 hello.
    await waitFor(() => viewer.state?.available && viewer.state.nodes.some(n => n.kind === 'playback'));
  }
  assert.equal(viewers.length, 2);
  const command = (viewer, value) => {
    const json = new TextEncoder().encode(JSON.stringify(value)), packet = new Uint8Array(json.length + 1);
    packet[0] = 0x97; packet.set(json, 1); viewer.socket.send(packet);
  };
  for (const viewer of viewers) {
    assert(viewer.state.nodes.every(n => !n.name.includes('outside-') && n.application !== 'OutsideHostTest'));
    assert.equal(viewer.state.nodes.filter(n => n.kind === 'playback').length, 1);
  }
  const first = viewers[0], second = viewers[1];
  assert.notEqual(first.state.generation, second.state.generation);
  const firstId = first.state.nodes.find(n => n.kind === 'playback').id;
  const secondId = second.state.nodes.find(n => n.kind === 'playback').id;
  command(first, { op: 'mute', id: secondId, value: true });
  await waitFor(() => first.error.includes('earlier connection'));
  assert.equal(second.state.nodes.find(n => n.id === secondId).mute, false);
  command(first, { op: 'mute', id: firstId, value: true });
  await waitFor(() => first.state.nodes.find(n => n.id === firstId).mute);
  assert.equal(second.state.nodes.find(n => n.id === secondId).mute, false);
  command(first, { op: 'mute', id: firstId, value: false });
  await waitFor(() => first.state.nodes.find(n => n.id === firstId).mute === false);
  console.log('mixer snapshots exclude outside applications; foreign IDs rejected; private controls stay isolated');
} finally {
  for (const { socket } of viewers) socket.close();
}
