window.__replayer = new rrweb.Replayer(window.__replayEvents, {
  root: document.body,
  speed: 1,
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
