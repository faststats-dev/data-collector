const { test } = require('node:test');
const assert = require('node:assert/strict');
const vm = require('node:vm');
const fs = require('node:fs');

function setup() {
  const listeners = new Map(), fontListeners = new Map(), observers = [], cast = {};
  const doc = {
    fonts: { status: 'loaded', addEventListener: (n, f) => fontListeners.set(n, f) },
    images: [], styleSheets: [], adoptedStyleSheets: [], frames: [], elements: [], inline: [],
    media: false, activeElement: null,
    querySelector() { return this.media ? {} : null; },
    querySelectorAll(selector) { return selector === 'iframe' ? this.frames : selector === '[style]' ? this.inline : this.elements; },
    getAnimations: () => [],
    addEventListener: (n, f) => listeners.set(n, f),
  };
  const c = vm.createContext({
    document: doc, atob: s => Buffer.from(s, 'base64').toString('binary'),
    MutationObserver: class { constructor(f) { observers.push(f); } observe() {} },
    __replayer: { on: (n, f) => { cast[n] = f; } },
  });
  c.window = c;
  vm.runInContext(fs.readFileSync(`${__dirname}/../src/visuals.js`, 'utf8'), c);
  const mutate = () => observers[0]([{ target: { tagName: 'DIV' } }]);
  return { c, doc, listeners, fontListeners, cast, mutate, observers };
}

test('static documents can be reused until mutation, scroll or resource completion', () => {
  const { c, mutate, listeners } = setup();
  assert.equal(c.__captureNeeded(), true);
  assert.equal(c.__captureNeeded(), false);
  mutate(); assert.equal(c.__captureNeeded(), true); assert.equal(c.__captureNeeded(), false);
  for (const event of ['scroll', 'load', 'error', 'focusin']) {
    listeners.get(event)(); assert.equal(c.__captureNeeded(), true); assert.equal(c.__captureNeeded(), false);
  }
});

test('loading fonts/images, animated images and media never reuse frames', () => {
  for (const alter of [
    d => { d.fonts.status = 'loading'; },
    d => { d.images = [{ complete: false }]; },
    d => { d.images = [{ complete: true, naturalWidth: 1, src: 'data:image/gif;base64,R0lG' }]; },
    d => { d.images = [{ complete: true, naturalWidth: 1, src: 'data:image/png;base64,' + Buffer.from('png-acTL-animation').toString('base64') }]; },
    d => { d.media = true; },
    d => { d.frames = [{ contentDocument: null }]; },
  ]) {
    const { c, doc } = setup(); alter(doc);
    assert.equal(c.__captureNeeded(), true); assert.equal(c.__captureNeeded(), true);
  }
});

test('static PNGs are reusable but animated CSS background images are not', () => {
  const { c, doc } = setup();
  doc.images = [{ complete: true, naturalWidth: 1, src: 'data:image/png;base64,' + Buffer.from('static-png').toString('base64') }];
  assert.equal(c.__captureNeeded(), true); assert.equal(c.__captureNeeded(), false);
  const other = setup();
  other.doc.styleSheets = [{ cssRules: [{ type: 1, style: { cssText: 'background-image: url("data:image/gif;base64,R0lG")' } }] }];
  assert.equal(other.c.__captureNeeded(), true); assert.equal(other.c.__captureNeeded(), true);
});

test('CSSOM mutations recheck resources without permanently disabling reuse', () => {
  const { c, cast } = setup();
  c.__captureNeeded(); assert.equal(c.__captureNeeded(), false);
  cast['event-cast']({ type: 3, data: { source: 13 } });
  assert.equal(c.__captureNeeded(), true); assert.equal(c.__captureNeeded(), false);
});

test('batched clock steps preserve every tick and stop at the first mutation', async () => {
  const { c, mutate } = setup();
  c.__captureNeeded();
  const times = [];
  c.__advance = time => { times.push(time); if (time === 2400) mutate(); };
  const step = await c.__advanceUntilCapture(0, 10, 10, 8, 99999);
  assert.equal(step.index, 3); assert.equal(step.dirty, true);
  assert.deepEqual(times, [0, 800, 1600, 2400]);
  const next = await c.__advanceUntilCapture(4, 7, 10, 8, 5000);
  assert.equal(next.index, 6); assert.equal(next.dirty, false);
  assert.deepEqual(times.slice(4), [3200, 4000, 4800]);
});


test('shadow-root mutations invalidate cached document pixels', () => {
  const { c, doc, observers } = setup();
  const shadow = setup().doc;
  shadow.ownerDocument = doc;
  doc.elements = [{ shadowRoot: shadow }];
  assert.equal(c.__captureNeeded(), true); assert.equal(c.__captureNeeded(), false);
  observers[1]([{ target: { tagName: 'DIV' } }]);
  assert.equal(c.__captureNeeded(), true); assert.equal(c.__captureNeeded(), false);
  shadow.media = true;
  observers[1]([{ target: { tagName: 'VIDEO' } }]);
  assert.equal(c.__captureNeeded(), true); assert.equal(c.__captureNeeded(), true);
});

test('unreadable blocked HTTP stylesheets are static; unknown blob sheets are not', () => {
  for (const [href, reusable] of [['https://example.com/a.css', true], ['blob:unknown', false]]) {
    const { c, doc } = setup();
    doc.styleSheets = [{ href, get cssRules() { throw new Error('cross origin'); } }];
    assert.equal(c.__captureNeeded(), true); assert.equal(c.__captureNeeded(), !reusable);
  }
});


test('continuous activity captures without rescanning the whole DOM', () => {
  const { c, doc, cast } = setup();
  let scans = 0;
  const query = doc.querySelectorAll;
  doc.querySelectorAll = function(selector) { if (selector === '*') scans++; return query.call(this, selector); };
  c.__captureNeeded();
  for (let i=0; i<100; i++) {
    cast['event-cast']({ type: 3, data: { source: 1 } });
    assert.equal(c.__captureNeeded(), true);
  }
  assert.equal(scans, 1);
  assert.equal(c.__captureNeeded(), false);
  assert.equal(scans, 2);
  cast['event-cast']({ type: 5, data: { tag: 'metadata' } });
  assert.equal(c.__captureNeeded(), false);
});

test('resources introduced by CSSOM stay unsafe after the deferred check', () => {
  const { c, doc, cast } = setup();
  const sheet = { cssRules: [] };
  doc.styleSheets = [sheet];
  c.__captureNeeded(); c.__captureNeeded();
  sheet.cssRules = [{ type: 1, style: { cssText: 'background:url(data:image/gif;base64,R0lG)' } }];
  cast['event-cast']({ type: 3, data: { source: 8 } });
  assert.equal(c.__captureNeeded(), true);
  assert.equal(c.__captureNeeded(), true);
  assert.equal(c.__captureNeeded(), true);
});
