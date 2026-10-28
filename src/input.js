'use strict';

/* Pointer, wheel and keyboard: seeking, volume and shortcuts. */

// -------------------------------------------------------------- seek drag

const ratioFromEvent = (event, node) => {
  const rect = node.getBoundingClientRect();
  return clamp((event.clientX - rect.left) / rect.width, 0, 1);
};

/** Pointer capture throws if the id is no longer active; a failed capture just
 *  means the drag ends when the pointer leaves the element. */
const capturePointer = (node, pointerId) => {
  try {
    node.setPointerCapture(pointerId);
  } catch {
    /* not capturable */
  }
};

el.seek.addEventListener('pointerdown', (event) => {
  const duration = state.song?.songDuration || 0;
  if (!duration) return;

  seeking = true;
  el.seek.classList.add('dragging');
  capturePointer(el.seek, event.pointerId);
  seekPreview = ratioFromEvent(event, el.seek) * duration;
  bumpProgress();
});

el.seek.addEventListener('pointermove', (event) => {
  if (!seeking) return;
  seekPreview = ratioFromEvent(event, el.seek) * (state.song?.songDuration || 0);
  renderProgress();
});

const endSeek = (event) => {
  if (!seeking) return;
  seeking = false;
  el.seek.classList.remove('dragging');
  try {
    el.seek.releasePointerCapture(event.pointerId);
  } catch {
    /* pointer already released */
  }

  seekLocally(seekPreview);
};

el.seek.addEventListener('pointerup', endSeek);
el.seek.addEventListener('pointercancel', endSeek);

// ------------------------------------------------------------ volume drag

let volumeDragging = false;
let volumeSendTimer = null;
let volumePeekTimer = null;

// Every volume we POST comes back as a VOLUME_CHANGED echo. While the user is
// dragging there are several in flight at once, and the last ones to land are
// not necessarily the newest — that is what made the slider jump on release.
// So: throttle what we send, always send the final value, and ignore echoes
// until the server has had time to settle on it.
let ignoreVolumeUntil = 0;
const VOLUME_SETTLE_MS = 900;
const VOLUME_THROTTLE_MS = 70;

const holdingVolume = () => volumeDragging || performance.now() < ignoreVolumeUntil;

/** Briefly show a wheel or keyboard change, both ways the card has of showing
 *  one: the slider peeks out from under the speaker, and the progress bar turns
 *  into the volume. Which of the two you see is the stylesheet's business — the
 *  speaker is hidden on Stack. */
const peekVolume = () => {
  el.volPop.classList.add('peek');

  const level = volumeLevel();
  el.seekVolFill.style.width = `${level}%`;
  el.seekVolLevel.textContent = String(Math.round(level));
  el.seekVolIcon.firstElementChild.setAttribute('href', volumeGlyph());
  el.seek.classList.add('showing-volume');

  clearTimeout(volumePeekTimer);
  volumePeekTimer = setTimeout(() => {
    el.volPop.classList.remove('peek');
    el.seek.classList.remove('showing-volume');
  }, 1100);
};

const flushVolume = () => {
  clearTimeout(volumeSendTimer);
  volumeSendTimer = null;
  send('volume', { volume: Math.round(state.volume) });
};

const setVolume = (value, { immediate = false } = {}) => {
  const volume = clamp(value, 0, 100);
  if (state.muted && volume > 0) state.muted = false;
  state.volume = volume;
  ignoreVolumeUntil = performance.now() + VOLUME_SETTLE_MS;
  renderControls();

  if (immediate) {
    flushVolume();
    return;
  }
  if (volumeSendTimer) return;
  volumeSendTimer = setTimeout(() => {
    volumeSendTimer = null;
    send('volume', { volume: Math.round(state.volume) });
  }, VOLUME_THROTTLE_MS);
};

// Events land on the padded hit area, but the ratio is measured against the
// visible rail so the ends of the slider map to 0 and 100.
const volumeFromEvent = (event) => ratioFromEvent(event, el.volRail) * 100;

el.volHit.addEventListener('pointerdown', (event) => {
  volumeDragging = true;
  el.volPop.classList.add('dragging');
  capturePointer(el.volHit, event.pointerId);
  setVolume(volumeFromEvent(event));
});

el.volHit.addEventListener('pointermove', (event) => {
  if (volumeDragging) setVolume(volumeFromEvent(event));
});

const endVolume = (event) => {
  if (!volumeDragging) return;
  volumeDragging = false;
  el.volPop.classList.remove('dragging');
  try {
    el.volHit.releasePointerCapture(event.pointerId);
  } catch {
    /* pointer already released */
  }
  // Whatever the throttle last sent, the value under the cursor wins.
  setVolume(state.volume, { immediate: true });
};

el.volHit.addEventListener('pointerup', endVolume);
el.volHit.addEventListener('pointercancel', endVolume);

// ------------------------------------------------- wheel / trackpad / keys

// A trackpad emits many small deltas where a mouse wheel emits few large ones.
// Carrying a float between events lets both feel proportional instead of the
// trackpad being ignored to rounding.
let wheelVolume = null;
let wheelResetTimer = null;
const WHEEL_SENSITIVITY = 0.12;

el.card.addEventListener(
  'wheel',
  (event) => {
    if (searching) return; // the wheel belongs to the result list
    if (lyricsShowing() && event.target.closest('#lyrics')) {
      scrollLyrics(event.deltaY);
      return;
    }
    // These scroll on their own; the wheel belongs to them, not to volume.
    if (event.target.closest('#queue, #upnext, #playlists, #artist-pop')) return;
    if (state.status !== 'connected') return;
    // With macOS natural scrolling, swiping up reports a positive deltaY, so
    // adding it is what makes "scroll up" raise the volume.
    const from = wheelVolume ?? (state.muted ? 0 : state.volume);
    wheelVolume = clamp(from + event.deltaY * WHEEL_SENSITIVITY, 0, 100);

    clearTimeout(wheelResetTimer);
    wheelResetTimer = setTimeout(() => {
      wheelVolume = null;
    }, 400);

    setVolume(wheelVolume);
    peekVolume();
  },
  { passive: true },
);

const VOLUME_STEP = 5;
const VOLUME_STEP_FINE = 1;

// While the search panel is open the arrows belong to the result list, and
// Escape closes it. Volume only gets the keys once search is out of the way.
document.addEventListener('keydown', (event) => {
  if (event.key === 'Escape' && pickerOpen) {
    event.preventDefault();
    closePicker();
    return;
  }
  if (event.key === 'Escape' && artistOpen) {
    event.preventDefault();
    closeArtist();
    return;
  }
  if (event.key === 'Escape' && !searching && openPanel) {
    event.preventDefault();
    setPanel(null);
    return;
  }
  if (!searching) return;

  if (event.key === 'Escape') {
    event.preventDefault();
    closeSearch();
    return;
  }
  if (event.key === 'ArrowDown' || event.key === 'ArrowUp') {
    event.preventDefault();
    highlight(activeResult + (event.key === 'ArrowDown' ? 1 : -1));
    return;
  }
  if (event.key === 'Enter') {
    event.preventDefault();
    clearTimeout(searchTimer);
    const item = results[activeResult];
    if (item) {
      window.widget.playResult(item.videoId);
      closeSearch();
    } else {
      runSearch(el.searchInput.value);
    }
  }
});

// Playback and volume shortcuts belong to the focused surface, outside editors.
document.addEventListener('keydown', (event) => {
  if (searching || pickerOpen || artistOpen || event.defaultPrevented || event.isComposing) return;
  if (event.metaKey || event.ctrlKey || event.altKey) return;
  if (event.target.closest('input, textarea, select, [contenteditable]:not([contenteditable="false"])')) return;
  if (state.status !== 'connected') return;

  if (event.key === ' ') {
    event.preventDefault();
    if (!event.repeat) el.play.click();
    return;
  }
  if (event.key === 'ArrowLeft' || event.key === 'ArrowRight') {
    const duration = state.song?.songDuration || 0;
    if (!duration || seeking) return;
    event.preventDefault();
    seekLocally(clamp(playhead() + (event.key === 'ArrowRight' ? 5 : -5), 0, duration));
    return;
  }
  if (event.key !== 'ArrowUp' && event.key !== 'ArrowDown') return;

  event.preventDefault();
  const step = event.shiftKey ? VOLUME_STEP_FINE : VOLUME_STEP;
  const from = state.muted ? 0 : state.volume;
  wheelVolume = null;
  setVolume(from + (event.key === 'ArrowUp' ? step : -step), { immediate: true });
  peekVolume();
});
