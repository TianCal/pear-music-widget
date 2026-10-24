'use strict';

/* The one panel slot (queue, lyrics, search), search itself, and the card's
   whole-surface gestures. */

// ------------------------------------------------------------------ search


/** Write the open panel to the DOM. Shared by `setPanel` and the teardown the
 *  dropdown's blur pushes at us, so the two can never drift apart. */
const paintPanel = (which) => {
  document.body.classList.toggle('panel-open', !!which);
  for (const panel of PANELS) {
    el[panel.section].hidden = which !== panel.key;
    el[panel.corner].classList.toggle('on', which === panel.key);
  }
};

const setPanel = (which) => {
  // A panel opening below would sit under the picker.
  if (which && pickerOpen) closePicker();
  if (which && artistOpen) closeArtist();
  openPanel = which;
  paintPanel(which);
  // Opening or closing one is an interaction, so it restarts the countdown
  // rather than pausing it.
  el.cornerBar.classList.remove('idle');
  armCornerFade();
  return window.widget.setPanel(which);
};

let searchSeq = 0;
let searchTimer = null;
let results = [];
let activeResult = -1;

const SEARCH_DEBOUNCE_MS = 400;

const setNote = (text) => {
  el.searchNote.textContent = text || '';
  el.searchNote.hidden = !text;
};

const renderResults = () => {
  el.results.replaceChildren();

  results.forEach((item, index) => {
    const img = document.createElement('img');
    img.alt = '';
    if (item.thumbnail) img.src = item.thumbnail;
    const row = resultRow(img, item.title, item.subtitle);
    row.dataset.index = String(index);
    if (index === activeResult) row.classList.add('active');
    el.results.append(row);
  });
};

const highlight = (index) => {
  if (!results.length) return;
  activeResult = Math.min(results.length - 1, Math.max(0, index));
  [...el.results.children].forEach((row, i) => row.classList.toggle('active', i === activeResult));
  el.results.children[activeResult]?.scrollIntoView({ block: 'nearest' });
};

const runSearch = async (query) => {
  const seq = ++searchSeq;
  const trimmed = query.trim();

  if (!trimmed) {
    results = [];
    activeResult = -1;
    renderResults();
    setNote('Type to search.');
    return;
  }

  setNote('Searching…');
  const res = await window.widget.search(trimmed);
  if (seq !== searchSeq || !searching) return; // a newer query won, or we closed
  // Rust saw an older query start after this one; this is still the newest.
  if (res?.superseded) return runSearch(query);

  if (!res?.ok) {
    results = [];
    renderResults();
    setNote(res?.error || 'Search failed.');
    return;
  }

  results = res.results || [];
  activeResult = results.length ? 0 : -1;
  renderResults();
  setNote(results.length ? '' : 'No results.');
};

const openSearch = async () => {
  if (searching || state.status !== 'connected') return;
  if (pickerOpen) closePicker();
  if (artistOpen) closeArtist();
  searching = true;
  results = [];
  activeResult = -1;
  renderResults();
  setNote('Type to search.');
  await setPanel('search');
  el.searchInput.focus();
  el.searchInput.select();
};

/** Local teardown; `pushed` is true when main already shrank the window for us. */
const resetSearch = (pushed) => {
  searching = false;
  searchSeq += 1;
  clearTimeout(searchTimer);
  el.searchInput.value = '';
  results = [];
  activeResult = -1;
  el.results.replaceChildren();
  if (pushed) {
    openPanel = null;
    paintPanel(null);
  } else {
    setPanel(null);
  }
};

const closeSearch = () => {
  if (!searching) return;
  resetSearch(false);
};

el.cornerSearch.addEventListener('click', (event) => {
  event.stopPropagation();
  if (searching) closeSearch();
  else openSearch();
});

el.searchClose.addEventListener('click', (event) => {
  event.stopPropagation();
  closeSearch();
});

el.searchInput.addEventListener('input', () => {
  clearTimeout(searchTimer);
  const query = el.searchInput.value;
  searchTimer = setTimeout(() => runSearch(query), SEARCH_DEBOUNCE_MS);
});

el.results.addEventListener('click', (event) => {
  const row = event.target.closest('.result');
  if (!row) return;
  const item = results[Number(row.dataset.index)];
  if (!item) return;
  window.widget.playResult(item.videoId);
  closeSearch();
});

// Double-clicking brings YouTube Music forward — while it is the source; a
// local folder has no app to bring forward. The artwork is the guaranteed
// target because it opts out of the drag region; the document-level listener
// covers the rest of the card for the case where the events do get through.
// Controls are excluded — a double click on one would fire it twice as well.
const INTERACTIVE =
  'button, input, .seek, .vol-hit, .result, .upnext-item, .queue-item, .playlists, .artist-link';

const openMusicApp = () => window.widget.openApp();

document.addEventListener('dblclick', (event) => {
  if (event.target.closest(INTERACTIVE)) return;
  // The dropdown gets the gesture on the artwork alone. It is a menu, and a
  // double click on any old part of a menu launching an app would be a
  // surprise — but the cover carries the tooltip that promises it on both
  // surfaces, so on the dropdown it was promising nothing.
  if (IS_PANEL && !event.target.closest('.art')) return;
  if (isLocal(state)) return;
  openMusicApp();
});

// Right-click either surface for a subset of the menu-bar menu — the widget
// gets its own layout and chrome, the dropdown gets its layout. Inputs keep
// their own menu, and the heart already uses right-click for dislike.
document.addEventListener('contextmenu', (event) => {
  if (event.target.closest('input')) return;
  event.preventDefault();
  window.widget.contextMenu();
});
