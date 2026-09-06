// Run with Node in the Docker rig. A fake clock exercises request timing without wall-clock sleeps.
import assert from 'node:assert/strict';
import { thumbnailScheduler } from '../src/thumbnails.js';

let time = 0, serial = 0, tail = Promise.resolve(), allowed = true, fail = false, finish;
const timers = new Map(), starts = [], images = [];
const flush = async () => { for (let i = 0; i < 12; i++) await Promise.resolve(); };
const advance = async ms => {
  time += ms;
  for (const [id, timer] of timers) if (timer.at <= time) { timers.delete(id); timer.run(); }
  await flush();
};
let hold = false;
const scheduler = thumbnailScheduler({
  queue: run => (tail = tail.then(run, run)),
  now: () => time,
  delay: (run, ms) => { const id = ++serial; timers.set(id, { run, at: time + ms }); return id; },
  cancel: id => timers.delete(id),
  allowed: () => allowed,
  capture: async (size, signal) => {
    starts.push({ time, size, signal });
    if (hold) await new Promise(resolve => { finish = resolve; });
    if (fail) throw new Error('temporary capture failure');
    return starts.length;
  },
  publish: image => images.push(image),
});
const update = (revision, eligible = true, width = 64) => scheduler.update({ revision, eligible, sizing: { width } });
update(1); await flush();
assert.equal(starts.length, 1);
for (let revision = 2; revision <= 30; revision++) { await advance(100); update(revision); }
assert.equal(starts.length, 1, 'continuous activity coalesces during cooldown');
await advance(100);
assert.equal(starts.length, 2, 'continuous activity gets its trailing update');
await advance(3000);
assert.equal(starts.length, 2, 'idle does not poll');
update(30, false); await advance(5000); update(30); await flush();
assert.equal(starts.length, 2, 'unchanged visibility return reuses the image');
hold = true; update(31); await flush(); update(32); finish(); hold = false; await flush();
await advance(3000);
assert.equal(starts.length, 4, 'activity during capture remains pending');
assert.equal(images.length, 4);
hold = true; await advance(3000); update(33); await flush(); update(34, false);
assert(starts.at(-1).signal.aborted, 'hide aborts client capture');
finish(); hold = false; await flush();
assert.equal(images.length, 4, 'hidden capture cannot replace retained image');
await advance(4000); update(34); await flush();
assert.equal(images.length, 5, 'dirty visibility return refreshes');
fail = true; update(35); await advance(3000); await advance(3000); await advance(30000);
assert.equal(starts.length, 8, 'final failure has one retry, then stops');
assert.equal(images.length, 5, 'failure retains successful image');
fail = false; update(36); await flush();
assert.equal(images.length, 6, 'new activity permits recovery');
allowed = false; update(37); await advance(3000);
assert.equal(starts.length, 9, 'live eligibility is rechecked before starting');
allowed = true; update(37); await flush();
assert.equal(starts.length, 10);
update(38); scheduler.dispose(); await advance(10000);
assert.equal(starts.length, 10, 'disposal cancels trailing work');
assert.equal(timers.size, 0);
assert(starts.every((start, i) => !i || start.time - starts[i - 1].time >= 3000), 'all starts are at least three seconds apart');
console.log('thumbnail cooldown, coalescing, activity during capture, visibility, stale results, retries and disposal passed');
