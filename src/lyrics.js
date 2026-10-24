'use strict';

/* The lyrics panel: the roll, manual scrolling and click-to-seek. */

// ------------------------------------------------------------------ lyrics

let lyricLines = [];       // [{ time, text }]
let lyricSynced = false;
let lyricActive = -1;
let lyricManualUntil = 0;
let lyricManualTimer = null;

// How long a hand scroll holds the roll still before playback takes it back.
const LYRIC_MANUAL_MS = 5000;

/* What made the roll stutter was never the transition — it was that the target
   moved underneath it. The active line used to grow from 12px to 13.5px, which
   reflowed every line below it *while* the glide to it was still running, and
   `offsetTop` was read back inside the animation frame, forcing a layout flush
   on every line change.

   So: the emphasis is a transform now (nothing below an active line moves), the
   geometry is measured once per render, and the glide itself stays a CSS
   transition — the compositor runs it with no per-frame JavaScript at all. All
   this code does is write a destination and a duration, twice a verse. */
let lyricCentres = [];     // the offset that puts each line in the middle
let lyricRange = [0, 0];   // how far the roll may travel, first line to last
let lyricMeasured = false;
/** False until the roll has been put somewhere. The first placement after
 *  opening the panel or changing track snaps: gliding there would mean watching
 *  the words swoop up from the top every time you open the panel. */
let lyricSettled = false;
let lyricOffset = 0;

const LYRIC_GLIDE_MIN_MS = 260;
const LYRIC_GLIDE_MAX_MS = 620;
/** Milliseconds per pixel travelled, between those two bounds. */
const LYRIC_GLIDE_PACE = 5;

/** The line being sung sits this far above the true middle. Dead centre reads
 *  as slightly low, because what you want to see next is underneath it — the
 *  lines already sung need less room than the ones coming. */
const LYRIC_RAISE = 14;

/** Where every line would have to sit to be centred. Costs one layout, taken
 *  when the panel is open and the lines have just changed shape. */
const measureLyrics = () => {
  const rows = el.lyricsLines.children;
  const height = el.lyricsScroll.clientHeight;
  // Nothing laid out yet — the panel is mid-open. Stay unmeasured and try again
  // on the next frame rather than caching a viewport of zero.
  if (!height) return false;
  const middle = height / 2;

  lyricCentres = [];
  for (let i = 0; i < rows.length; i += 1) {
    lyricCentres.push(rows[i].offsetTop + rows[i].offsetHeight / 2 - middle + LYRIC_RAISE);
  }

  // The roll travels between "first line centred" and "last line centred", and
  // no further. Clamping the low end at 0 instead is what put the opening lines
  // up in the top fade and let the closing ones drift down into the bottom one:
  // a line was being sung while it sat outside the readable band. Both ends are
  // allowed to be negative — a short lyric simply sits in the middle.
  const first = lyricCentres[0] ?? 0;
  const last = lyricCentres[lyricCentres.length - 1] ?? 0;
  lyricRange = [Math.min(first, last), Math.max(first, last)];
  lyricMeasured = true;
  return true;
};

/** Send the roll to `target`, over a distance-appropriate glide. Two style
 *  writes, and the compositor does the rest. */
const glideLyricsTo = (target, { instant = false } = {}) => {
  const to = clamp(target, lyricRange[0], lyricRange[1]);
  // A line-to-line step should feel immediate; a jump across a seek should not
  // race. Distance picks where between those the glide lands.
  const ms =
    instant ? 0
    : clamp(Math.abs(to - lyricOffset) * LYRIC_GLIDE_PACE, LYRIC_GLIDE_MIN_MS, LYRIC_GLIDE_MAX_MS);

  lyricOffset = to;
  el.lyricsLines.style.setProperty('--roll-ms', `${ms}ms`);
  el.lyricsLines.style.transform = `translateY(${(-to).toFixed(2)}px)`;
};

/** Scrolling over the lyrics moves the lyrics, not the volume. */
const scrollLyrics = (deltaY) => {
  if (!lyricSynced) return; // unsynced is a plain block that scrolls natively
  if (!lyricMeasured) measureLyrics();

  lyricManualUntil = performance.now() + LYRIC_MANUAL_MS;
  // Follows the wheel exactly: an eased glide would lag behind the fingers.
  glideLyricsTo(lyricOffset + deltaY, { instant: true });

  clearTimeout(lyricManualTimer);
  lyricManualTimer = setTimeout(() => {
    lyricActive = -1; // force the next tick to re-centre on the playing line
    bumpProgress();
  }, LYRIC_MANUAL_MS);
};

const LYRIC_NOTE = {
  idle: '',
  loading: 'Looking for lyrics…',
  none: 'No lyrics found for this track.',
};

/* No dirty check here any more: the main process pushes the `lyrics` event only
   when the words or the state actually change, so being called at all is the
   signal that this has to be rebuilt. */
const renderLyrics = () => {
  const data = state.lyrics;
  lyricLines = data?.lines || [];
  lyricSynced = !!data?.synced;
  lyricActive = -1;
  lyricManualUntil = 0;
  clearTimeout(lyricManualTimer);

  lyricOffset = 0;
  lyricMeasured = false;
  lyricSettled = false;
  lyricRange = [0, 0];

  el.lyrics.classList.toggle('plain', !!data && !lyricSynced);
  el.lyricsLines.replaceChildren();
  el.lyricsLines.style.removeProperty('--roll-ms');
  el.lyricsLines.style.transform = '';

  if (!lyricLines.length) {
    el.lyricsNote.textContent = LYRIC_NOTE[state.lyricsState] || LYRIC_NOTE.none;
    el.lyricsNote.hidden = !el.lyricsNote.textContent;
    return;
  }

  el.lyricsNote.textContent = lyricSynced ? '' : 'Unsynced lyrics — these do not follow along.';
  el.lyricsNote.hidden = lyricSynced;

  lyricLines.forEach((line, index) => {
    const row = document.createElement(lyricSynced ? 'button' : 'p');
    row.className = line.text ? 'lyric' : 'lyric gap';
    if (lyricSynced) {
      row.type = 'button';
      row.dataset.index = String(index);
    }
    const text = document.createElement('span');
    text.className = 'lyric-text';
    text.textContent = line.text;
    row.append(text);
    if (line.jyutping) {
      const jyutping = document.createElement('span');
      jyutping.className = 'lyric-jyutping';
      jyutping.textContent = line.jyutping;
      row.append(jyutping);
    }
    el.lyricsLines.append(row);
  });
};

/** Effective lyric timing from Rust: Song Time overrides the global Timing.
 *  Positive turns the lines over earlier; seeking subtracts the same shift. */
const lyricShift = () => state.lyricsOffset || 0;

/** Index of the last line whose timestamp has passed. */
const lyricIndexAt = (position) => {
  let lo = 0;
  let hi = lyricLines.length - 1;
  let found = -1;
  while (lo <= hi) {
    const mid = (lo + hi) >> 1;
    if (lyricLines[mid].time <= position) {
      found = mid;
      lo = mid + 1;
    } else {
      hi = mid - 1;
    }
  }
  return found;
};

/** Called from the rAF tick with the interpolated playhead. */
const rollLyrics = (position) => {
  if (openPanel !== 'lyrics' || !lyricSynced || !lyricLines.length) return;
  // Deferred to here rather than done at render: the panel may well have been
  // closed then, and a hidden panel has no height to measure against.
  if (!lyricMeasured && !measureLyrics()) return;

  const index = lyricIndexAt(position + lyricShift());
  if (index === lyricActive) return;
  const previous = lyricActive;
  lyricActive = index;

  // Only the rows whose state actually changed. Repainting the class on all of
  // them meant a hundred style invalidations per line on a long lyric, to move
  // one word's worth of emphasis.
  const rows = el.lyricsLines.children;
  rows[previous]?.classList.remove('active');
  rows[index]?.classList.add('active');
  for (let i = Math.min(previous, index); i < Math.max(previous, index); i += 1) {
    rows[i]?.classList.toggle('past', i < index);
  }

  // Leave the roll where the user parked it until their scroll times out.
  if (performance.now() < lyricManualUntil) return;

  // Centre the active line. Before the first cue there is no active line, so
  // the roll waits on the opening one rather than pinning the list to the top
  // and leaving it half inside the fade.
  glideLyricsTo(lyricCentres[index] ?? lyricRange[0], { instant: !lyricSettled });
  lyricSettled = true;
};

const toggleLyrics = () => {
  if (openPanel === 'lyrics') {
    setPanel(null);
    return;
  }
  if (searching) resetSearch(false);
  // Opening lands on the line being sung rather than gliding to it from
  // wherever the roll happened to be left.
  lyricActive = -1;
  lyricMeasured = false;
  lyricSettled = false;
  setPanel('lyrics');
};

el.cornerLyrics.addEventListener('click', (event) => {
  event.stopPropagation();
  toggleLyrics();
});

// Clicking a line seeks to it — the timestamps are right there. `dataset.index`
// is not tested for truthiness: the first line's is the string "0", which is
// falsy, and guarding on it is what used to make the opening line unclickable.
// A `<p>` from an unsynced block has no index at all, so the lookup misses.
el.lyricsLines.addEventListener('click', (event) => {
  const row = event.target.closest('.lyric');
  if (!row || !lyricSynced) return;
  const line = lyricLines[Number(row.dataset.index)];
  if (!line) return;

  // Land the audio where this line *sounds*, which is the same correction the
  // roll is already applying — otherwise a nudged roll would seek you to a
  // point that then lights up a different line.
  seekLocally(Math.max(0, line.time - lyricShift()));
});
