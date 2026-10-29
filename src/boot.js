'use strict';

/* Loaded last. Everything driven from Rust — events, the first state, the
   observers whose callbacks reach across files — is wired here, once every
   script has declared what it needs. */

// ------------------------------------------------------------------- boot

// The rail's width, cached for `renderProgress` (see `railWidth` in app.js).
new ResizeObserver((entries) => {
  railWidth = entries[entries.length - 1].contentRect.width;
  seekX = -1; // the cached pixel is against the old width, so force a rewrite
  renderProgress();
}).observe(el.rail);

// A resize or a skin change rewraps the lines, so every measurement taken
// against the old shape is stale.
new ResizeObserver(() => {
  lyricMeasured = false;
  lyricSettled = false;
  if (lyricsShowing()) {
    lyricActive = -1;
    bumpProgress();
  }
}).observe(el.lyricsScroll);

// The dropdown collapses itself when it loses focus; mirror that here.
window.widget.onPanelCollapsed(() => {
  // Overlays go with it: the picker's art timer would otherwise keep measuring
  // a page nobody can see.
  if (pickerOpen) closePicker();
  if (artistOpen) closeArtist();
  if (searching) resetSearch(true);
  else if (openPanel) {
    openPanel = null;
    paintPanel(null);
  }
});

// And reopens the queue or lyrics it was last left showing. Main has already
// sized the window for it; this draws it and tells main someone is watching.
window.widget.onPanelRestore((which) => {
  if (openPanel === which || searching) return;
  if (which === 'lyrics') toggleLyrics();
  else if (which === 'queue') toggleQueue();
});

const showConnecting = () => {
  showStatusCopy('connecting');
  el.setupRetry.hidden = true;
  el.setupOpen.hidden = true;
};

el.setupRetry.addEventListener('click', () => {
  showConnecting();
  window.widget.retry();
});

el.setupOpen.addEventListener('click', () => {
  el.setupTitle.textContent = 'Starting YouTube Music…';
  el.setupBody.textContent = 'The widget will connect as soon as the app is up.';
  el.setupRetry.hidden = true;
  el.setupOpen.hidden = true;
  window.widget.openApp();
});

// The widget's `document.hidden` is always true, so its `visibilitychange`
// never fires: this is where it catches up on coming back — the playhead, and
// the colours of a cover that could not be sampled while it was away. Going
// away closes the picker, whose art timer would otherwise keep running.
window.widget.onOnScreen(({ label, shown }) => {
  if (IS_PANEL || label !== 'widget') return;
  widgetOnScreen = shown;
  renderTurning();
  if (shown) {
    if (state.cover) applyPalette(state.cover);
    bumpProgress();
  } else if (pickerOpen) {
    closePicker();
  }
});

// The page is authored at a fixed base size and scaled by the main process, so
// the CSS corner radius has to shrink as the zoom grows to stay flush with the
// window's own native rounding. Rust sends the radius with the zoom.
window.widget.onZoom(({ zoom, radius }) => {
  document.documentElement.style.setProperty('--radius', `${(radius / zoom).toFixed(2)}px`);
});

/* Channels that have heard a live event. Each event is a whole snapshot of its
   channel, so once one has landed the cold-load reply below is older than what
   is on screen — applying it anyway put a previous source's song on a card
   already showing the new source's artwork and queue. */
const heardLive = new Set();
const live = (channel, apply) => (payload) => {
  heardLive.add(channel);
  apply(payload);
};

const listening = [
  window.widget.onState(live('state', applyState)),
  window.widget.onCover(live('cover', applyCover)),
  window.widget.onQueue(live('queue', applyQueue)),
  window.widget.onQueueArtReady(queueArtReady),
  window.widget.onLyrics(live('lyrics', applyLyrics)),
].map((off) => off.ready);

// One reply carries all four channels: a window loading from cold cannot wait a
// track for the artwork to change. Listen first, and only then ask: the
// listeners above register asynchronously, and a cover or queue event emitted
// after the reply was built but before they were in place would be lost —
// those change once a track, so the card would show the old art until the
// next one. (The strip boots the same way.)
Promise.all(listening)
  .then(() => window.widget.getState())
  .then((boot) => {
    if (!heardLive.has('cover')) applyCover(boot.cover);
    if (!heardLive.has('queue')) applyQueue(boot.queue);
    if (!heardLive.has('lyrics')) applyLyrics(boot.lyrics);
    if (!heardLive.has('state')) applyState(boot.state);

    // The widget reopens the panel it was left showing. The main process has
    // already sized the window for it; this is what tells the page to draw it
    // and the main process that somebody is watching, so the lyrics get
    // fetched.
    if (!IS_PANEL && boot.panel === 'lyrics') toggleLyrics();
    else if (!IS_PANEL && boot.panel === 'queue') toggleQueue();
  });

window.addEventListener('resize', refreshMarquees);

bumpProgress();
