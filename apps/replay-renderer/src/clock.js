// Installed before rrweb. Playback scheduling advances only when Rust asks.
(() => {
  let now = 0, nextId = 1;
  const rafs = new Map(), timers = new Map();
  const NativeDate = Date;
  const epoch = __EPOCH__;
  class VirtualDate extends NativeDate {
    constructor(...args) { super(...(args.length ? args : [epoch + now])); }
    static now() { return epoch + now; }
  }
  window.Date = VirtualDate;
  Object.defineProperty(performance, 'now', { value: () => now });
  window.requestAnimationFrame = fn => { const id = nextId++; rafs.set(id, fn); return id; };
  window.cancelAnimationFrame = id => rafs.delete(id);
  const schedule = (fn, delay, repeat, args) => {
    if (typeof fn !== 'function') throw new Error('String timers are unsupported');
    const id = nextId++, interval = Math.max(1, Number(delay) || 0);
    timers.set(id, { fn, at: now + interval, interval, repeat, args });
    return id;
  };
  window.setTimeout = (fn, delay, ...args) => schedule(fn, delay, false, args);
  window.setInterval = (fn, delay, ...args) => schedule(fn, delay, true, args);
  window.clearTimeout = window.clearInterval = id => timers.delete(id);
  window.__advance = target => {
    if (target < now) throw new Error('Clock cannot run backwards');
    let count = 0;
    while (true) {
      let selected;
      for (const item of timers) {
        if (item[1].at <= target && (!selected || item[1].at < selected[1].at)) selected = item;
      }
      if (!selected) break;
      if (++count > 100000) throw new Error('Virtual timer runaway');
      const [id, timer] = selected;
      now = timer.at;
      if (timer.repeat) timer.at += timer.interval; else timers.delete(id);
      timer.fn(...timer.args);
    }
    now = target;
    const pending = [...rafs];
    for (const [id, fn] of pending) { if (rafs.delete(id)) fn(now); }
  };
})();
