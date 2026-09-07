import { startMic, stopMic } from '/mic.js';

// Runs against real capture, AudioWorklet and WebCodecs; injected failures exercise cleanup.
export async function checkMicrophone() {
  const assert = (ok, message) => { if (!ok) throw new Error(message); };
  const delay = ms => new Promise(r => setTimeout(r, ms));
  const wait = async fn => {
    for (let i = 0; i < 200; i++) { if (fn()) return; await delay(25); }
    throw new Error('microphone check timed out');
  };
  const Context = window.AudioContext, Encoder = window.AudioEncoder, Worklet = window.AudioWorkletNode;
  const gum = navigator.mediaDevices.getUserMedia.bind(navigator.mediaDevices);
  const contexts = [], encoders = [], tracks = [], nodes = [], blocks = [];
  window.micCheckState = () => ({ contexts: contexts.map(c => c.state), encoders: encoders.map(e => e.state), tracks: tracks.map(t => t.readyState), nodes: nodes.length, blocks: blocks.length });
  let constraints, encoderCallbacks, throwConfigure = false, throwEncode = false, throwProcessor = false, failModule = false, holdModule, legacy = false;
  const legacyNodes = [];
  window.AudioContext = class extends Context {
    constructor(options) {
      super(options);
      contexts.push(this);
      const addModule = this.audioWorklet.addModule.bind(this.audioWorklet);
      this.audioWorklet.addModule = async url => {
        if (failModule) throw new Error('injected module failure');
        if (holdModule) await holdModule;
        if (throwProcessor) {
          const source = (await (await fetch(url)).text()).replace('process(inputs) {', 'process(inputs) { throw new Error("injected processor failure");');
          const broken = URL.createObjectURL(new Blob([source], { type: 'text/javascript' }));
          try { return await addModule(broken); }
          finally { URL.revokeObjectURL(broken); }
        }
        return addModule(url);
      };
    }
    createScriptProcessor(...args) {
      assert(legacy, 'unexpected legacy capture');
      const node = super.createScriptProcessor(...args);
      legacyNodes.push(node);
      node.meter = this.createAnalyser();
      node.connect(node.meter);
      node.meter.connect(this.destination);
      return node;
    }
  };
  window.AudioEncoder = class extends Encoder {
    constructor(callbacks) { super(callbacks); encoders.push(this); encoderCallbacks = callbacks; }
    configure(config) { if (throwConfigure) throw new Error('injected configure failure'); super.configure(config); }
    encode(data) {
      if (throwEncode) throw new Error('injected encode failure');
      blocks.push({ timestamp: data.timestamp, frames: data.numberOfFrames, rate: data.sampleRate });
      super.encode(data);
    }
  };
  window.AudioWorkletNode = class extends Worklet {
    constructor(...args) {
      super(...args);
      nodes.push(this);
      this.meter = args[0].createAnalyser();
      super.connect(this.meter);
      this.meter.connect(args[0].destination);
    }
  };
  const realCapture = async options => {
    constraints = options;
    const stream = await gum(options);
    tracks.push(...stream.getTracks());
    return stream;
  };
  navigator.mediaDevices.getUserMedia = realCapture;
  const stopped = async () => {
    await wait(() => contexts.every(c => c.state === 'closed'));
    assert(encoders.every(e => e.state === 'closed'), 'all encoders closed');
    assert(tracks.every(t => t.readyState === 'ended'), 'all tracks ended');
    assert(nodes.every(n => n.port.onmessage === null && n.onprocessorerror === null), 'callbacks detached');
    assert(legacyNodes.every(n => n.onaudioprocess === null), 'legacy callbacks detached');
  };
  let packets = 0, ends = 0, decodedPeak = 0;
  const decoder = new AudioDecoder({
    output: data => {
      const pcm = new Float32Array(data.numberOfFrames);
      data.copyTo(pcm, { planeIndex: 0, format: 'f32-planar' });
      for (const sample of pcm) decodedPeak = Math.max(decodedPeak, Math.abs(sample));
      data.close();
    },
    error: e => { throw e; },
  });
  decoder.configure({ codec: 'opus', sampleRate: 48000, numberOfChannels: 1 });
  const send = data => {
    packets++;
    decoder.decode(new EncodedAudioChunk({ type: 'key', timestamp: packets * 20000, data }));
  };
  const end = () => ends++;
  try {
    for (let cycle = 0; cycle < 3; cycle++) {
      await decoder.flush();
      decodedPeak = 0;
      blocks.length = 0;
      const before = packets;
      assert(await startMic(send, end), 'start succeeds');
      assert(!(await startMic(send, end)), 'duplicate start ignored');
      await wait(() => packets > before + 15);
      await wait(() => decodedPeak > .001);
      let frames = 0;
      for (const block of blocks) {
        assert(block.rate === contexts.at(-1).sampleRate, 'actual sample rate');
        assert(block.timestamp === Math.round(frames * 1e6 / block.rate), 'continuous timestamps');
        frames += block.frames;
      }
      const samples = new Float32Array(nodes.at(-1).meter.fftSize);
      nodes.at(-1).meter.getFloatTimeDomainData(samples);
      assert(samples.every(s => s === 0), 'silent local output');
      const stale = encoderCallbacks;
      stopMic();
      const after = packets;
      stale.output({ byteLength: 0, copyTo() {} });
      stale.error(new Error('stale encoder error'));
      await delay(100);
      assert(packets === after && ends === 0, 'no stale packets or errors');
      await stopped();
    }
    assert(constraints.audio.echoCancellation && constraints.audio.noiseSuppression && constraints.audio.autoGainControl, 'capture processing requested');

    // Old requests may resolve or reject after a new capture owns the microphone.
    for (const rejectOld of [false, true]) {
      let resolve, reject;
      navigator.mediaDevices.getUserMedia = () => new Promise((a, b) => { resolve = a; reject = b; });
      const pending = startMic(send, end);
      stopMic();
      navigator.mediaDevices.getUserMedia = realCapture;
      assert(await startMic(send, end), 'restart during old permission request');
      const before = packets;
      if (rejectOld) reject(new DOMException('Denied', 'NotAllowedError'));
      else resolve(await realCapture({ audio: true }));
      assert(!(await pending), 'cancelled start reports false');
      await wait(() => packets > before + 3);
      stopMic();
      await stopped();
    }
    navigator.mediaDevices.getUserMedia = () => Promise.reject(new DOMException('Denied', 'NotAllowedError'));
    await startMic(send, end).then(() => { throw new Error('denial should reject'); }, e => assert(e.name === 'NotAllowedError', 'permission denial'));
    await stopped();
    navigator.mediaDevices.getUserMedia = realCapture;

    for (const failure of ['configure', 'module', 'encode', 'processor', 'track', 'send']) {
      throwConfigure = failure === 'configure'; failModule = failure === 'module'; throwEncode = failure === 'encode';
      throwProcessor = failure === 'processor';
      const previousEnds = ends;
      try { await startMic(failure === 'send' ? () => { throw new Error('send failed'); } : send, end); }
      catch (e) { assert(['configure', 'module'].includes(failure), e.message); }
      if (failure === 'track') tracks.at(-1).dispatchEvent(new Event('ended'));
      if (!['configure', 'module'].includes(failure)) {
        try { await wait(() => ends === previousEnds + 1); }
        catch { throw new Error(`${failure}: expected end callback, got ${ends - previousEnds}; handler=${typeof nodes.at(-1).onprocessorerror}; context=${contexts.at(-1).state}; encoder=${encoders.at(-1).state}`); }
      }
      await stopped();
      throwConfigure = failModule = throwEncode = throwProcessor = false;
    }
    let release;
    holdModule = new Promise(r => { release = r; });
    const pending = startMic(send, end);
    await wait(() => tracks.at(-1).readyState === 'live');
    stopMic();
    holdModule = null;
    assert(await startMic(send, end), 'restart during module load');
    release();
    assert(!(await pending), 'old module load cancelled');
    stopMic();
    await stopped();
    legacy = true;
    window.AudioWorkletNode = undefined;
    await decoder.flush();
    decodedPeak = 0;
    const before = packets;
    assert(await startMic(send, end), 'legacy start succeeds');
    await wait(() => packets > before + 15 && decodedPeak > .001);
    const samples = new Float32Array(legacyNodes.at(-1).meter.fftSize);
    legacyNodes.at(-1).meter.getFloatTimeDomainData(samples);
    assert(samples.every(s => s === 0), 'legacy local output is silent');
    stopMic();
    await stopped();
    return { packets, decodedPeak, contexts: contexts.length, worklets: nodes.length, ends };
  } finally { stopMic(); decoder.close(); }
}
