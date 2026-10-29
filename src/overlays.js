'use strict';

/* The two overlays that hang from the card: the playlist picker and the
   artist popup. */

/* ----------------------------------------------------------- playlist picker

   YouTube Music playlists and saved folders in one list. Picking a row plays
   it, switching source first when the row belongs to the other one — so both
   halves are listed whichever source is playing, and Rust does the switching.
   The list is fetched fresh unless the hover prefetch below is recent:
   playlists change in YouTube Music itself, and a drive can come or go. */

/** Bumped on every open and close, so a reply that lands after the picker
 *  was closed (or reopened) is dropped rather than drawn. */
let pickerSeq = 0;

const pickerNote = (text) => {
  const note = document.createElement('div');
  note.className = 'playlists-note';
  note.textContent = text;
  return note;
};

const pickerHead = (text) => {
  const head = document.createElement('div');
  head.className = 'playlists-head';
  head.textContent = text;
  return head;
};

/** A `.result` row: `lead` (a cover or a tile), then a title over a subtitle.
 *  The picker, the artist popup and search all draw these. It is a
 *  `.track-row` (styles.css) with the result kind's metrics. */
const resultRow = (lead, title, sub) => {
  const row = document.createElement('button');
  row.className = 'result track-row track-row--result';
  row.type = 'button';
  const text = document.createElement('span');
  text.className = 'result-text';
  const titleNode = document.createElement('span');
  titleNode.className = 'result-title';
  titleNode.textContent = title;
  const subNode = document.createElement('span');
  subNode.className = 'result-sub';
  subNode.textContent = sub || '';
  text.append(titleNode, subNode);
  row.append(lead, text);
  return row;
};

const pickerRow = ({ kind, id, title, sub, art, current, disabled }) => {
  let lead;
  if (kind === 'folder') {
    lead = document.createElement('span');
    lead.className = 'folder-tile';
    lead.innerHTML = '<svg><use href="#i-folder" /></svg>';
  } else {
    lead = document.createElement('img');
    lead.alt = '';
    if (art) lead.src = art;
  }
  const row = resultRow(lead, title, sub);
  row.setAttribute('role', 'menuitem');
  row.dataset.kind = kind;
  row.dataset.id = id;
  row.disabled = !!disabled;
  if (current) {
    row.classList.add('current');
    row.setAttribute('aria-current', 'true');
  }
  return row;
};

/** The list itself — its groups, notes and the covers of the rows in view —
 *  is shared/lists.js, drawn here as `.result` rows. */
const picker = window.shared.playlistList({
  scroll: el.playlistsScroll,
  row: '.result',
  ui: { head: pickerHead, note: pickerNote, row: pickerRow },
  fetchArt: (urls) => window.widget.playlistArt(urls),
});

// Scroll events belong to the rendering steps a hidden document may skip, so
// while the picker is open the rows in view are also rechecked on a timer. It
// measures a few dozen rows and asks for nothing that is already loading.
const PICKER_ART_CHECK_MS = 300;
let pickerArtTimer = null;

/** The list fetched when the pointer reached the button. Moving onto it is
 *  the cue that a click may follow, so by the time it does the reply is
 *  usually in. Reused only while fresh: playlists change in YouTube Music
 *  itself, and a drive can come or go. */
const PICKER_PREFETCH_MS = 15000;
let pickerPrefetch = null;

const fetchPicker = () => {
  if (pickerPrefetch && performance.now() - pickerPrefetch.at < PICKER_PREFETCH_MS) {
    return pickerPrefetch.reply;
  }
  const reply = window.widget.playlists().catch(() => null);
  pickerPrefetch = { at: performance.now(), reply };
  // A failed fetch is not worth keeping: the click should try again.
  reply.then((data) => {
    if (!data?.ok && pickerPrefetch?.reply === reply) pickerPrefetch = null;
  });
  return reply;
};

// Only once the pointer rests there: passing over the button on the way
// somewhere else should not cost a library fetch.
const PICKER_HOVER_MS = 150;
let pickerHoverTimer = null;
el.cornerPlaylists.addEventListener('pointerenter', () => {
  clearTimeout(pickerHoverTimer);
  pickerHoverTimer = setTimeout(() => {
    if (!pickerOpen) fetchPicker();
  }, PICKER_HOVER_MS);
});
el.cornerPlaylists.addEventListener('pointerleave', () => clearTimeout(pickerHoverTimer));

const openPicker = async () => {
  if (artistOpen) closeArtist();
  const seq = ++pickerSeq;
  pickerOpen = true;
  el.playlists.hidden = false;
  el.cornerPlaylists.classList.add('on');
  el.cornerPlaylists.setAttribute('aria-expanded', 'true');
  // The bar stays up while the picker hangs from it; see `armCornerFade`.
  setCornerIdle(false);
  clearTimeout(cornerIdleTimer);
  el.playlistsScroll.replaceChildren(pickerNote('Loading playlists…'));

  clearInterval(pickerArtTimer);
  pickerArtTimer = setInterval(picker.loadArt, PICKER_ART_CHECK_MS);

  const data = await fetchPicker();
  if (seq !== pickerSeq) return;
  picker.render(data);
};

const closePicker = () => {
  pickerSeq += 1;
  pickerOpen = false;
  el.playlists.hidden = true;
  el.cornerPlaylists.classList.remove('on');
  el.cornerPlaylists.setAttribute('aria-expanded', 'false');
  clearInterval(pickerArtTimer);
  armCornerFade();
};

el.cornerPlaylists.addEventListener('click', (event) => {
  event.stopPropagation();
  if (pickerOpen) closePicker();
  else openPicker();
});

/** One pick at a time: a second click while one is starting would race it. */
let pickerPlaying = false;

el.playlists.addEventListener('click', async (event) => {
  event.stopPropagation();
  const row = event.target.closest('.result');
  if (!row || row.disabled || pickerPlaying) return;

  const seq = pickerSeq;
  pickerPlaying = true;
  row.classList.add('busy');
  let reply;
  try {
    reply = await window.widget.playPlaylist(row.dataset.kind, row.dataset.id);
  } catch (err) {
    reply = { ok: false, error: String(err) };
  } finally {
    pickerPlaying = false;
  }
  if (seq !== pickerSeq) return;
  row.classList.remove('busy');
  if (reply?.ok) {
    // What is current just changed, so the kept list is out of date.
    pickerPrefetch = null;
    closePicker();
    return;
  }
  const sub = row.querySelector('.result-sub');
  if (sub) sub.textContent = reply?.error || 'Could not play that.';
});

// A click anywhere else on the card dismisses it, like a menu.
document.addEventListener('click', (event) => {
  if (pickerOpen && !event.target.closest('#playlists, #btn-playlists')) closePicker();
});

/* ------------------------------------------------------------ artist popup

   The Top songs of every artist on the playing track, one section each,
   dropped from the credit under the title. A page is ~0.4 s away through the
   API server, so it is warmed the way the picker is: once the pointer rests
   on the card, and again if the track changes while it is there. Nothing is
   fetched just because a track started. Rust keeps pages (see artist.rs) and
   so does this side, so reopening skips even the IPC. */

let artistSeq = 0;

/** The pages — kept replies, one section per artist filled in as each lands
 *  — are shared/lists.js; this side names the markup, `.result` rows under
 *  a head with the artist's name and Play all. */
const artistPages = window.shared.artistPages({
  fetch: (id, query) => window.widget.artistPage(id, query),
  ui: {
    section: {
      root: 'artist-section',
      head: 'artist-pop-head',
      name: 'artist-pop-name',
      playAll: 'artist-play-all',
    },
    note: pickerNote,
    row: (song) =>
      pickerRow({
        kind: 'song',
        id: song.videoId,
        title: song.title,
        sub: song.subtitle,
        art: song.thumbnail,
        current: song.videoId === state.song?.videoId,
      }),
  },
});

const songArtists = () => (state.song?.artists || []).slice(0, artistPages.limit);

/** Under the subtitle, starting at the credit, kept inside the card. Every
 *  measurement first, then the writes: the width does not depend on the
 *  height, and reading it after a write would lay the card out twice. */
const placeArtistPop = (link) => {
  const card = el.card.getBoundingClientRect();
  const line = el.subtitle.parentElement.getBoundingClientRect();
  const name = link.getBoundingClientRect();
  const pop = el.artistPop;
  const width = pop.offsetWidth;
  const top = Math.round(line.bottom - card.top + 4);
  const left = clamp(name.left - card.left - 6, 7, card.width - width - 7);
  pop.style.top = `${top}px`;
  pop.style.maxHeight = `${Math.max(80, card.height - top - 7)}px`;
  pop.style.left = `${Math.round(left)}px`;
};

const openArtist = (link) => {
  if (pickerOpen) closePicker();
  const seq = ++artistSeq;
  artistOpen = true;
  el.artistPop.hidden = false;
  artistPages.show(songArtists(), {
    live: () => seq === artistSeq,
    draw: (sections) => {
      el.artistPopScroll.replaceChildren(...sections);
      placeArtistPop(link);
    },
  });
};

const closeArtist = () => {
  artistSeq += 1;
  artistOpen = false;
  el.artistPop.hidden = true;
};

el.subtitle.addEventListener('click', (event) => {
  const link = event.target.closest('.artist-link');
  if (!link) return;
  event.stopPropagation();
  if (artistOpen) closeArtist();
  else openArtist(link);
});

const ARTIST_CARD_HOVER_MS = 150;
let pointerOnCard = false;
let artistWarmTimer = null;

const warmArtists = () => {
  clearTimeout(artistWarmTimer);
  artistWarmTimer = setTimeout(() => {
    if (!pointerOnCard || state.status !== 'connected') return;
    songArtists().forEach(artistPages.page);
  }, ARTIST_CARD_HOVER_MS);
};

el.card.addEventListener('pointerenter', () => {
  pointerOnCard = true;
  warmArtists();
});
el.card.addEventListener('pointerleave', () => {
  pointerOnCard = false;
  clearTimeout(artistWarmTimer);
});

/** One play at a time, as in the picker. */
let artistPlaying = false;

el.artistPop.addEventListener('click', async (event) => {
  event.stopPropagation();
  const target = event.target.closest('.result, .artist-play-all');
  if (!target || artistPlaying) return;

  const seq = artistSeq;
  const all = target.classList.contains('artist-play-all');
  artistPlaying = true;
  target.classList.add('busy');
  let reply;
  try {
    reply = all
      ? await window.widget.playPlaylist('youtube', target.dataset.id)
      : await window.widget.playResult(target.dataset.id);
  } catch (err) {
    reply = { ok: false, error: String(err) };
  } finally {
    artistPlaying = false;
  }
  if (seq !== artistSeq) return;
  target.classList.remove('busy');
  if (reply?.ok) {
    closeArtist();
    return;
  }
  const sub = all ? null : target.querySelector('.result-sub');
  if (sub) sub.textContent = reply?.error || 'Could not play that.';
});

document.addEventListener('click', (event) => {
  if (artistOpen && !event.target.closest('#artist-pop, .artist-link')) closeArtist();
});
