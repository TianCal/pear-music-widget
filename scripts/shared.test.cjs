const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const { test } = require('node:test');

// The shared/ helpers are classic scripts attaching to window.shared, loaded
// here the way a page loads them. Time is a hand-turned clock so the volume
// rules can be checked to the millisecond.
function load(names) {
  let now = 0;
  const timers = new Map();
  let nextId = 1;
  const context = {
    window: {},
    performance: { now: () => now },
    setTimeout(fn, ms) {
      const id = nextId++;
      timers.set(id, { at: now + ms, fn });
      return id;
    },
    clearTimeout(id) {
      timers.delete(id);
    },
  };
  for (const name of names) {
    vm.runInNewContext(fs.readFileSync(path.join(__dirname, `../src/shared/${name}.js`), 'utf8'), context);
  }
  /** Move the clock on, running each timer as its moment comes. */
  const advance = (ms) => {
    const until = now + ms;
    for (;;) {
      const due = [...timers.entries()].filter(([, t]) => t.at <= until).sort((a, b) => a[1].at - b[1].at);
      if (!due.length) break;
      const [id, timer] = due[0];
      timers.delete(id);
      now = timer.at;
      timer.fn();
    }
    now = until;
  };
  return { shared: context.window.shared, advance };
}

// ------------------------------------------------------------------ format

test('formatTime reads m:ss, and h:mm:ss once there is an hour', () => {
  const { formatTime } = load(['format']).shared;
  assert.equal(formatTime(0), '0:00');
  assert.equal(formatTime(5), '0:05');
  assert.equal(formatTime(61), '1:01');
  assert.equal(formatTime(59.9), '0:59', 'whole seconds, rounded down');
  assert.equal(formatTime(3600), '1:00:00');
  assert.equal(formatTime(3700), '1:01:40', 'an hour-long file never reads 61:40');
  assert.equal(formatTime(36_000 + 7 * 60 + 9), '10:07:09');
  assert.equal(formatTime(-3), '0:00', 'nothing before the start');
  assert.equal(formatTime(undefined), '0:00');
  assert.equal(formatTime(NaN), '0:00');
});

test('clamp keeps a value inside its bounds', () => {
  const { clamp } = load(['format']).shared;
  assert.equal(clamp(5, 0, 10), 5);
  assert.equal(clamp(-1, 0, 10), 0);
  assert.equal(clamp(11, 0, 10), 10);
});

// ---------------------------------------------------------------- artcache

test('artCache evicts the oldest entry past its entry ceiling', () => {
  const cache = load(['artcache']).shared.artCache({ max: 2 });
  cache.set('a', 'A');
  cache.set('b', 'B');
  cache.set('c', 'C');
  assert.equal(cache.size, 2);
  assert.deepEqual([...cache.keys()], ['b', 'c']);
  assert.equal(cache.has('a'), false);
});

test('artCache evicts by characters, counting the id with the art', () => {
  // Each entry is 1 + 4 characters; three fit, a fourth pushes the oldest out.
  const cache = load(['artcache']).shared.artCache({ chars: 15 });
  cache.set('a', 'AAAA');
  cache.set('b', 'BBBB');
  cache.set('c', 'CCCC');
  assert.deepEqual([...cache.keys()], ['a', 'b', 'c']);
  cache.set('d', 'DDDD');
  assert.deepEqual([...cache.keys()], ['b', 'c', 'd']);
  // An entry over the whole budget on its own is not held either, as the
  // card's and the strip's caches always had it: nothing stays over budget.
  cache.set('e', 'E'.repeat(20));
  assert.deepEqual([...cache.keys()], []);
  assert.equal(cache.size, 0);
  cache.set('f', 'FFFF');
  assert.deepEqual([...cache.keys()], ['f'], 'and the budget is whole again after it');
});

test('artCache replaces an id without counting it twice', () => {
  const cache = load(['artcache']).shared.artCache({ chars: 15 });
  cache.set('a', 'AAAA');
  cache.set('a', 'AAAA');
  cache.set('a', 'AAAA');
  cache.set('b', 'BBBB');
  cache.set('c', 'CCCC');
  assert.deepEqual([...cache.keys()], ['a', 'b', 'c'], 'three entries fit, so the budget was not spent thrice on one');
  assert.equal(cache.get('a'), 'AAAA');
});

test('artCache: get promotes a hit, peek does not', () => {
  const cache = load(['artcache']).shared.artCache({ max: 3 });
  cache.set('a', 'A');
  cache.set('b', 'B');
  cache.set('c', 'C');
  assert.equal(cache.get('a'), 'A');
  assert.deepEqual([...cache.keys()], ['b', 'c', 'a'], 'the hit moved to the newest end');
  assert.equal(cache.peek('b'), 'B');
  assert.deepEqual([...cache.keys()], ['b', 'c', 'a'], 'a peek left the order alone');
  cache.set('d', 'D');
  assert.deepEqual([...cache.keys()], ['c', 'a', 'd'], 'the peeked entry, still oldest, went first');
  assert.equal(cache.get('zzz'), undefined);
});

test('artCache drop frees the budget', () => {
  const cache = load(['artcache']).shared.artCache({ chars: 10 });
  cache.set('a', 'AAAA');
  cache.set('b', 'BBBB');
  cache.drop('a');
  cache.drop('a'); // dropping twice is harmless
  cache.set('c', 'CCCC');
  assert.deepEqual([...cache.keys()], ['b', 'c'], 'the dropped entry made room, so nothing else went');
});

// ------------------------------------------------------------------ volume

/** A volume control over a page-like state, with every send recorded. */
function volume() {
  const { shared, advance } = load(['volume']);
  const state = { volume: 50 };
  const applied = [];
  const sent = [];
  const control = shared.volumeControl({
    apply: (v) => {
      state.volume = v;
      applied.push(v);
    },
    volume: () => state.volume,
    send: (v) => sent.push(v),
  });
  return { control, state, applied, sent, advance };
}

test('volume applies at once and sends once per 70 ms, the latest value', () => {
  const { control, applied, sent, advance } = volume();
  control.set(30);
  assert.deepEqual(applied, [30], 'the slider moves before anything is sent');
  assert.deepEqual(sent, []);
  advance(20);
  control.set(40);
  advance(20);
  control.set(45.4);
  assert.deepEqual(sent, [], 'nothing goes out inside the throttle window');
  advance(30);
  assert.deepEqual(sent, [45], 'one send, carrying the value at the time it fires, rounded');
  advance(1000);
  assert.deepEqual(sent, [45], 'and no more for a value already sent');
});

test('volume: a second burst after the window sends again', () => {
  const { control, sent, advance } = volume();
  control.set(10);
  advance(70);
  control.set(20);
  advance(69);
  assert.deepEqual(sent, [10]);
  advance(1);
  assert.deepEqual(sent, [10, 20]);
});

test('volume sends a released value immediately, once', () => {
  const { control, sent, advance } = volume();
  control.set(30);
  advance(10);
  control.set(33, { immediate: true });
  assert.deepEqual(sent, [33], 'the release flushes now');
  advance(200);
  assert.deepEqual(sent, [33], 'the pending throttled send was taken over, not added to');
});

test('volume clamps what it applies', () => {
  const { control, applied } = volume();
  control.set(150);
  control.set(-5);
  assert.deepEqual(applied, [100, 0]);
});

test('volume holds its own value against echoes for 900 ms', () => {
  const { control, advance } = volume();
  assert.equal(control.holding(), false);
  control.set(30);
  assert.equal(control.holding(), true);
  advance(899);
  assert.equal(control.holding(), true);
  advance(1);
  assert.equal(control.holding(), false);
  control.set(31, { immediate: true });
  assert.equal(control.holding(), true, 'an immediate send holds too');
});

test('volume wheel steps by 0.12 per delta and carries a gesture for 400 ms', () => {
  const { control, applied, sent, advance } = volume();
  // Floats carried across steps, so to a hair.
  const near = (expected, message) => assert.ok(Math.abs(applied.at(-1) - expected) < 1e-9, message);
  control.wheel(10, 50);
  near(51.2, 'a fresh gesture starts from the level given');
  advance(100);
  control.wheel(10, 0);
  near(52.4, 'inside a gesture the carry wins over the level given');
  advance(400);
  control.wheel(10, 20);
  near(21.2, 'after 400 ms a new gesture starts from the level given');
  control.dropCarry();
  control.wheel(-10, 80);
  near(78.8, 'a dropped carry starts over too');
  control.wheel(-10_000, 0);
  assert.equal(applied.at(-1), 0, 'and the carry is clamped');
  advance(70);
  assert.deepEqual(sent, [51, 52, 0], 'wheel steps went out by the same throttle, rounded');
});
