const { test } = require('node:test');
const assert = require('node:assert/strict');
const vm = require('node:vm');
const fs = require('node:fs');

function clock() {
  const context = vm.createContext({ performance: {}, Date });
  context.window = context;
  vm.runInContext(fs.readFileSync(`${__dirname}/../src/clock.js`, 'utf8').replace('__EPOCH__', '1000'), context);
  return context;
}

test('wall time cannot advance the replay clock', async () => {
  const c = clock();
  await new Promise(resolve => setTimeout(resolve, 20));
  assert.equal(c.Date.now(), 1000);
  assert.equal(c.performance.now(), 0);
  c.__advance(25);
  assert.equal(c.Date.now(), 1025);
  assert.equal(new c.Date().getTime(), 1025);
  assert.equal(new c.Date(42).getTime(), 42);
});

test('timers preserve order, cancellation, and interval catch-up', () => {
  const c = clock(), seen = [];
  const id = c.setInterval(() => { seen.push(c.performance.now()); }, 10);
  c.setTimeout(() => c.clearInterval(id), 25);
  const cancelled = c.setTimeout(() => seen.push('cancelled'), 5);
  c.clearTimeout(cancelled);
  c.__advance(30);
  assert.deepEqual(seen, [10, 20]);
  assert.throws(() => c.__advance(29), /backwards/);
});

test('RAF scheduled inside RAF waits for the next explicit tick', () => {
  const c = clock(), seen = [];
  c.requestAnimationFrame(t => {
    seen.push(t);
    c.requestAnimationFrame(t => seen.push(t));
  });
  c.__advance(40);
  assert.deepEqual(seen, [40]);
  c.__advance(80);
  assert.deepEqual(seen, [40, 80]);
});
