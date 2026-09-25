window.__replayer = new rrweb.Replayer(window.__replayEvents, {
  root: document.body,
  speed: 1,
  // Rust removes safe idle frames after stepping the original clock. rrweb's
  // own speed changes would break the original-time footer and evidence mapping.
  skipInactive: false,
  showWarning: false,
  showDebug: false,
  mouseTail: false,
  insertStyleRules: [
    '*,*::before,*::after {transition:none!important;animation:none!important;caret-color:transparent!important;scroll-behavior:auto!important}'
  ],
  loadTimeout: 0,
  UNSAFE_replayCanvas: false,
});
delete window.__replayEvents;
window.__replayer.play(0);
window.__advance(0);
