// Reuse pixels only after the Rust side has seen two identical captures. This
// observer also requires static, fully loaded resources and stops on any change.
(() => {
  let dirty = true, safePreviously = false, dynamicStyles = false;
  let sheets = new WeakMap();
  const documents = new WeakSet(), staticImages = new Map(), failedLinks = new WeakSet();
  const invalidate = event => {
    dirty = true;
    if (event?.target?.tagName === 'LINK') {
      if (event.type === 'error') failedLinks.add(event.target);
      sheets = new WeakMap();
    }
  };
  const staticImage = src => {
    if (!staticImages.has(src)) {
      let safe = /^data:image\/jpe?g[;,]/i.test(src);
      if (/^data:image\/png;base64,/i.test(src)) {
        // APNG declares animation using an acTL chunk. False positives merely
        // disable reuse; malformed or unfamiliar formats take the capture path.
        try { safe = !atob(src.slice(src.indexOf(',') + 1)).includes('acTL'); } catch {}
      }
      if (/^data:image\/svg\+xml[;,]/i.test(src)) {
        try {
          const comma = src.indexOf(',');
          const svg = src.slice(0, comma).includes(';base64') ? atob(src.slice(comma + 1)) : decodeURIComponent(src.slice(comma + 1));
          safe = !/<(?:animate\w*|set|image|use|foreignObject|script|style)\b|animation|transition/i.test(svg);
        } catch {}
      }
      staticImages.set(src, safe);
    }
    return staticImages.get(src);
  };
  const cssSafe = value => {
    if (/image-set\(|paint\(/i.test(value)) return false;
    let dataUrls = 0;
    for (const match of value.matchAll(/url\(\s*(?:"([^"]*)"|'([^']*)'|([^)]*))\s*\)/gi)) {
      const url = (match[1] ?? match[2] ?? match[3]).trim();
      if (url.includes('\\') || /^blob:/i.test(url)) return false;
      if (/^data:/i.test(url)) { dataUrls++; if (!staticImage(url)) return false; }
      // HTTP(S)/file URLs are blocked by CDP before replay starts.
    }
    const safe = !/data:/i.test(value) || dataUrls > 0;
    return safe;
  };
  const rulesSafe = rules => {
    for (const rule of rules) {
      if (rule.type === 3) return false; // Imports may finish asynchronously.
      if (rule.type === 5) continue; // FontFaceSet below tracks font loading.
      if (rule.style && !cssSafe(rule.style.cssText)) return false;
      if (rule.cssRules && !rulesSafe(rule.cssRules)) return false;
    }
    return true;
  };
  window.__replayer.on('event-cast', event => {
    dirty = true;
    // CSSOM changes can introduce resources without DOM mutations. Conservatively
    // retain every-frame capture after stylesheet/declaration/adoption events.
    if (event.type === 3 && [8, 13, 15].includes(event.data.source)) dynamicStyles = true;
  });
  const inspect = doc => {
    if (!doc) return false;
    const owner = doc.ownerDocument || doc;
    if (!documents.has(doc)) {
      documents.add(doc);
      dirty = true;
      new MutationObserver(records => {
        dirty = true;
        if (records.some(r => /^(STYLE|LINK)$/.test(r.target.tagName || r.target.parentElement?.tagName || ''))) sheets = new WeakMap();
      }).observe(doc, { subtree: true, childList: true, attributes: true, characterData: true });
      for (const event of ['load', 'error', 'scroll', 'resize', 'focusin', 'focusout']) doc.addEventListener(event, invalidate, true);
      for (const event of ['loading', 'loadingdone', 'loadingerror']) owner.fonts.addEventListener(event, invalidate);
    }
    let safe = !dynamicStyles && owner.fonts.status === 'loaded';
    if (doc.querySelector('video, canvas, animate, animateMotion, animateTransform, set, link[rel~="stylesheet"][href^="data:"]') ||
        (doc.getAnimations ? doc.getAnimations() : [...doc.querySelectorAll('*')].flatMap(e => e.getAnimations())).some(a => a.playState === 'running' || a.pending) ||
        doc.activeElement?.matches('input, textarea, [contenteditable]')) safe = false;
    for (const element of doc.querySelectorAll('*')) if (element.shadowRoot && !inspect(element.shadowRoot)) safe = false;
    for (const img of (doc.images || doc.querySelectorAll('img'))) {
      if (!img.complete || (img.naturalWidth && !staticImage(img.currentSrc || img.src))) safe = false;
    }
    for (const element of doc.querySelectorAll('[style]')) if (!cssSafe(element.style.cssText)) safe = false;
    for (const sheet of [...(doc.styleSheets || []), ...(doc.adoptedStyleSheets || [])]) {
      if (!sheets.has(sheet)) {
        try { sheets.set(sheet, rulesSafe(sheet.cssRules)); } catch {
          // These schemes are blocked before any replay DOM is built. An opaque
          // stylesheet object for such a URL cannot later deliver new pixels.
          sheets.set(sheet, /^(https?|file|ftp|wss?):/i.test(sheet.href || '') || (!!sheet.ownerNode && failedLinks.has(sheet.ownerNode)));
        }
      }
      if (!sheets.get(sheet)) safe = false;
    }
    for (const frame of doc.querySelectorAll('iframe')) {
      try { if (!inspect(frame.contentDocument)) safe = false; } catch { safe = false; }
    }
    return safe;
  };
  window.__captureNeeded = () => {
    const safe = dirty ? inspect(document) : safePreviously;
    safePreviously = safe;
    const result = dirty || !safe;
    dirty = false;
    return result;
  };
})();

// Preserve every rrweb RAF/timer tick. Cross CDP on visual changes or at most one
// output second apart. A microtask checkpoint delivers mutation observers.
window.__advanceUntilCapture = async (start, stop, fps, speed, duration) => {
  for (let index = start; index < stop; index++) {
    window.__advance(Math.min(index * 1000 * speed / fps, duration));
    await Promise.resolve();
    if (window.__captureNeeded()) return { index, dirty: true };
  }
  return { index: stop - 1, dirty: false };
};
