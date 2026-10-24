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

/** Covers already resolved, by source URL: reopening draws them at once. */
const pickerArt = new Map();

/** A `.result` row: `lead` (a cover or a tile), then a title over a subtitle.
 *  The picker, the artist popup and search all draw these. */
const resultRow = (lead, title, sub) => {
  const row = document.createElement('button');
  row.className = 'result';
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
    if (art && pickerArt.get(art)) lead.src = pickerArt.get(art);
    else if (art) lead.dataset.art = art;
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

const renderPicker = (data) => {
  const youtube = [pickerHead('YouTube Music')];
  if (!data.youtube?.ok) {
    youtube.push(pickerNote(data.youtube?.error || 'YouTube Music did not answer.'));
  } else if (!data.youtube.playlists.length) {
    youtube.push(pickerNote('No playlists in your library.'));
  } else {
    for (const item of data.youtube.playlists) {
      youtube.push(
        pickerRow({
          kind: 'youtube',
          id: item.id,
          title: item.title,
          sub: item.subtitle,
          art: item.art,
          current: item.id === data.currentPlaylist,
        }),
      );
    }
  }

  const folders = [pickerHead('Folders')];
  if (!data.folders.length) {
    folders.push(pickerNote('Add a folder from Source > Choose Folder… in the menu.'));
  }
  for (const folder of data.folders) {
    folders.push(
      pickerRow({
        kind: 'folder',
        id: folder.path,
        title: folder.name,
        sub: folder.present ? folder.parent : 'Not connected',
        current: folder.current,
        disabled: !folder.present,
      }),
    );
  }

  // The source that is playing leads.
  el.playlistsScroll.replaceChildren(
    ...(data.local ? [...folders, ...youtube] : [...youtube, ...folders]),
  );
  el.playlistsScroll.querySelector('.current')?.scrollIntoView({ block: 'nearest' });
  loadVisiblePickerArt();
};

/* Covers come from Rust (the CSP allows data: URLs only), and only for the rows
   in view: a library of a hundred playlists should not wait on, or fill the
   image cache with, covers nobody sees. Measured by hand rather than with an
   IntersectionObserver or animation frames, which a hidden WebKit document —
   the widget always reports hidden — may never deliver. */
const PICKER_ART_MARGIN = 60;
const pickerArtPending = new Set();

const loadVisiblePickerArt = () => {
  const scroll = el.playlistsScroll;
  const top = scroll.scrollTop - PICKER_ART_MARGIN;
  const bottom = scroll.scrollTop + scroll.clientHeight + PICKER_ART_MARGIN;
  const urls = [];
  for (const img of scroll.querySelectorAll('img[data-art]')) {
    const row = img.closest('.result');
    if (row.offsetTop + row.offsetHeight < top || row.offsetTop > bottom) continue;
    const url = img.dataset.art;
    if (!pickerArtPending.has(url)) {
      pickerArtPending.add(url);
      urls.push(url);
    }
  }
  if (!urls.length) return;
  window.widget
    .playlistArt(urls)
    .then((reply) => {
      for (const [url, data] of Object.entries(reply?.art || {})) {
        if (data) pickerArt.set(url, data);
      }
      for (const img of scroll.querySelectorAll('img[data-art]')) {
        const data = pickerArt.get(img.dataset.art);
        if (data) {
          img.src = data;
          delete img.dataset.art;
        }
      }
    })
    .catch(() => {})
    // A failed cover can be asked for again when it scrolls back into view.
    .finally(() => urls.forEach((url) => pickerArtPending.delete(url)));
};

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
  pickerArtTimer = setInterval(loadVisiblePickerArt, PICKER_ART_CHECK_MS);

  const data = await fetchPicker();
  if (seq !== pickerSeq) return;
  if (!data?.ok) {
    el.playlistsScroll.replaceChildren(pickerNote('Could not load playlists.'));
    return;
  }
  renderPicker(data);
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

/** Replies by artist id, or by name for an artist without one:
 *  `{ at, reply, data }`, where `data` is set once a good reply lands so an
 *  open can draw it in the same frame. */
const ARTIST_REUSE_MS = 10 * 60 * 1000;
const ARTIST_PAGES_MAX = 24;
const artistPages = new Map();

/** `query` is YouTube Music's own name, which an artist without an id is
 *  found by; `name` may be an `artistNames` rename. */
const fetchArtist = ({ id, query }) => {
  const key = id || `~${query}`;
  const held = artistPages.get(key);
  if (held && performance.now() - held.at < ARTIST_REUSE_MS) return held;
  const entry = { at: performance.now(), data: null };
  entry.reply = window.widget
    .artistPage(id, query)
    .catch((err) => ({ ok: false, error: String(err) }));
  entry.reply.then((data) => {
    // A failure is not kept here: Rust decides what is worth asking again.
    if (data?.ok) entry.data = data;
    else if (artistPages.get(key) === entry) artistPages.delete(key);
  });
  artistPages.delete(key);
  artistPages.set(key, entry);
  if (artistPages.size > ARTIST_PAGES_MAX) artistPages.delete(artistPages.keys().next().value);
  return entry;
};

/** Artists warmed and shown for one track: a credit can list more names
 *  than a popup has room for. */
const ARTISTS_MAX = 4;
const songArtists = () => (state.song?.artists || []).slice(0, ARTISTS_MAX);

/** One artist's section: their name and Play all, then their songs. */
const artistSection = (artist, data) => {
  const head = document.createElement('div');
  head.className = 'artist-pop-head';
  const name = document.createElement('span');
  name.className = 'artist-pop-name';
  name.textContent = artist.name;
  head.append(name);

  const section = document.createElement('div');
  section.className = 'artist-section';
  section.append(head);
  if (!data) {
    section.append(pickerNote('Loading top songs…'));
    return section;
  }
  if (!data.ok) {
    section.append(pickerNote(data.error || 'Could not load this artist.'));
    return section;
  }
  const { songs, allSongs } = data.page;
  if (allSongs) {
    const all = document.createElement('button');
    all.className = 'artist-play-all';
    all.type = 'button';
    all.dataset.id = allSongs;
    all.textContent = 'Play all';
    head.append(all);
  }
  if (!songs.length) section.append(pickerNote('No top songs on YouTube Music.'));
  for (const song of songs) {
    const row = pickerRow({
      kind: 'song',
      id: song.videoId,
      title: song.title,
      sub: song.subtitle,
      current: song.videoId === state.song?.videoId,
    });
    const img = row.querySelector('img');
    if (song.thumbnail) img.src = song.thumbnail;
    section.append(row);
  }
  return section;
};

/** Under the subtitle, starting at the credit, kept inside the card. */
const placeArtistPop = (link) => {
  const card = el.card.getBoundingClientRect();
  const line = el.subtitle.parentElement.getBoundingClientRect();
  const name = link.getBoundingClientRect();
  const pop = el.artistPop;
  const top = Math.round(line.bottom - card.top + 4);
  pop.style.top = `${top}px`;
  pop.style.maxHeight = `${Math.max(80, card.height - top - 7)}px`;
  const left = clamp(name.left - card.left - 6, 7, card.width - pop.offsetWidth - 7);
  pop.style.left = `${Math.round(left)}px`;
};

/** A section still loading only says so if its reply is not in quickly, so
 *  warm pages never flash a note. */
const ARTIST_LOADING_NOTE_MS = 150;

const openArtist = (link) => {
  if (pickerOpen) closePicker();
  const seq = ++artistSeq;
  artistOpen = true;
  const artists = songArtists();
  const entries = artists.map(fetchArtist);
  let late = false;
  const draw = () => {
    if (seq !== artistSeq) return;
    const shown = artists
      .map((artist, i) => {
        const data = entries[i].data || entries[i].settled || null;
        return data || late ? artistSection(artist, data) : null;
      })
      .filter(Boolean);
    el.artistPopScroll.replaceChildren(...shown);
    placeArtistPop(link);
  };

  el.artistPop.hidden = false;
  draw();
  const notes = setTimeout(() => {
    late = true;
    draw();
  }, ARTIST_LOADING_NOTE_MS);
  // Each section fills in as its page lands, in credit order.
  for (const entry of entries) {
    entry.reply.then((data) => {
      entry.settled = data;
      draw();
    });
  }
  Promise.all(entries.map((entry) => entry.reply)).then(() => clearTimeout(notes));
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
    songArtists().forEach(fetchArtist);
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
