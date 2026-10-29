'use strict';

/**
 * The two lists the card's overlays and the strip's sheet both draw: the
 * playlist list (YouTube Music playlists and saved folders, covers loaded
 * for the rows in view) and the artist pages (one Top songs section per
 * artist on the track, filled in as each page lands).
 *
 * Each page brings its own markup through `ui` and its own IPC through the
 * fetch arguments; nothing here reads a page's globals.
 */
(() => {
  // -------------------------------------------------------------- playlists

  /* Covers come from Rust (the CSP allows data: URLs only), and only for the
     rows in view: a library of a hundred playlists should not wait on, or
     fill the image cache with, covers nobody sees. Measured by hand rather
     than with an IntersectionObserver or animation frames, which a hidden
     WebKit document — the widget always reports hidden — may never deliver. */
  const ART_MARGIN = 60;

  /**
   * `scroll` is the list's scrolling box and `row` the selector of a row in
   * it. `ui.head(text)`, `ui.note(text)` and `ui.row(fields)` build the
   * page's own markup; a row's `<img>` is given its cover when one is already
   * held, else `data-art` with its URL. `fetchArt(urls)` resolves to
   * `{ art: { url: data } }`. A cover that came back empty is asked for again
   * when it scrolls back into view, or not before `retryMs` when that is set.
   */
  const playlistList = ({ scroll, row: rowSelector, ui, fetchArt, retryMs = 0 }) => {
    /** Covers already resolved, by source URL: reopening draws them at once. */
    const art = new Map();
    const pending = new Set();
    /** URLs that came back empty, by when they may be asked again. */
    const failed = new Map();

    const render = (data) => {
      if (!data?.ok) {
        scroll.replaceChildren(ui.note('Could not load playlists.'));
        return;
      }
      const youtube = [ui.head('YouTube Music')];
      if (!data.youtube?.ok) {
        youtube.push(ui.note(data.youtube?.error || 'YouTube Music did not answer.'));
      } else if (!data.youtube.playlists.length) {
        youtube.push(ui.note('No playlists in your library.'));
      } else {
        for (const item of data.youtube.playlists) {
          const row = ui.row({
            kind: 'youtube',
            id: item.id,
            title: item.title,
            sub: item.subtitle,
            art: art.get(item.art),
            current: item.id === data.currentPlaylist,
          });
          if (item.art && !art.has(item.art)) row.querySelector('img').dataset.art = item.art;
          youtube.push(row);
        }
      }

      const folders = [ui.head('Folders')];
      if (!data.folders.length) {
        folders.push(ui.note('Add a folder from Source > Choose Folder… in the menu.'));
      }
      for (const folder of data.folders) {
        folders.push(
          ui.row({
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
      scroll.replaceChildren(...(data.local ? [...folders, ...youtube] : [...youtube, ...folders]));
      scroll.querySelector('.current')?.scrollIntoView({ block: 'nearest' });
      loadArt();
    };

    /** Ask for the covers of the rows in view that have none in flight. */
    const loadArt = () => {
      const top = scroll.scrollTop - ART_MARGIN;
      const bottom = scroll.scrollTop + scroll.clientHeight + ART_MARGIN;
      const urls = [];
      for (const img of scroll.querySelectorAll('img[data-art]')) {
        const row = img.closest(rowSelector);
        if (row.offsetTop + row.offsetHeight < top || row.offsetTop > bottom) continue;
        const url = img.dataset.art;
        if ((failed.get(url) ?? 0) > performance.now()) continue;
        if (!pending.has(url)) {
          pending.add(url);
          urls.push(url);
        }
      }
      if (!urls.length) return;
      fetchArt(urls)
        .then((reply) => {
          for (const [url, data] of Object.entries(reply?.art || {})) {
            if (data) art.set(url, data);
          }
          if (retryMs) {
            for (const url of urls) {
              if (!reply?.art?.[url]) failed.set(url, performance.now() + retryMs);
            }
          }
          for (const img of scroll.querySelectorAll('img[data-art]')) {
            const data = art.get(img.dataset.art);
            if (data) {
              img.src = data;
              delete img.dataset.art;
            }
          }
        })
        .catch(() => {})
        // A failed cover can be asked for again when it scrolls back into view.
        .finally(() => urls.forEach((url) => pending.delete(url)));
    };

    return { render, loadArt };
  };

  // ---------------------------------------------------------------- artists

  /** Artists warmed and shown for one track: a credit can list more names
   *  than a popup has room for. */
  const ARTISTS_MAX = 4;
  const ARTIST_REUSE_MS = 10 * 60 * 1000;
  const ARTIST_PAGES_MAX = 24;
  /** A section still loading only says so if its reply is not in quickly, so
   *  warm pages never flash a note. */
  const ARTIST_LOADING_NOTE_MS = 150;

  /**
   * `fetch(id, query)` asks Rust for a page (`artist_page`). `ui.section`
   * names the classes of a section (`root`, optional), its `head`, the
   * artist's `name` and `playAll`; `ui.note(text)` and `ui.row(song)` build
   * the page's own markup.
   */
  const artistPages = ({ fetch, ui }) => {
    /** Replies by artist id, or by name for an artist without one:
     *  `{ at, reply, data }`, where `data` is set once a good reply lands so
     *  an open can draw it in the same frame. Rust keeps pages too
     *  (artist.rs); this side saves the IPC on a reopen. */
    const pages = new Map();

    /** `query` is YouTube Music's own name, which an artist without an id is
     *  found by; `name` may be an `artistNames` rename. */
    const page = ({ id, query }) => {
      const key = id || `~${query}`;
      const held = pages.get(key);
      if (held && performance.now() - held.at < ARTIST_REUSE_MS) return held;
      const entry = { at: performance.now(), data: null };
      entry.reply = fetch(id, query).catch((err) => ({ ok: false, error: String(err) }));
      entry.reply.then((data) => {
        // A failure is not kept here: Rust decides what is worth asking again.
        if (data?.ok) entry.data = data;
        else if (pages.get(key) === entry) pages.delete(key);
      });
      pages.delete(key);
      pages.set(key, entry);
      if (pages.size > ARTIST_PAGES_MAX) pages.delete(pages.keys().next().value);
      return entry;
    };

    /** One artist's section: their name and Play all, then their songs. */
    const section = (artist, data) => {
      const head = document.createElement('div');
      head.className = ui.section.head;
      const name = document.createElement('span');
      name.className = ui.section.name;
      name.textContent = artist.name;
      head.append(name);

      const box = document.createElement('div');
      if (ui.section.root) box.className = ui.section.root;
      box.append(head);
      if (!data) {
        box.append(ui.note('Loading top songs…'));
        return box;
      }
      if (!data.ok) {
        box.append(ui.note(data.error || 'Could not load this artist.'));
        return box;
      }
      const { songs, allSongs } = data.page;
      if (allSongs) {
        const all = document.createElement('button');
        all.className = ui.section.playAll;
        all.type = 'button';
        all.dataset.id = allSongs;
        all.textContent = 'Play all';
        head.append(all);
      }
      if (!songs.length) box.append(ui.note('No top songs on YouTube Music.'));
      for (const song of songs) box.append(ui.row(song));
      return box;
    };

    /**
     * Draw the sections for `artists` through `draw(nodes)`: at once with
     * what is held, then again as each page lands, in credit order, for as
     * long as `live()` says this is still the view that was opened.
     */
    const show = (artists, { live, draw }) => {
      const entries = artists.map(page);
      let late = false;
      const redraw = () => {
        if (!live()) return;
        const shown = artists
          .map((artist, i) => {
            const data = entries[i].data || entries[i].settled || null;
            return data || late ? section(artist, data) : null;
          })
          .filter(Boolean);
        draw(shown);
      };
      redraw();
      const notes = setTimeout(() => {
        late = true;
        redraw();
      }, ARTIST_LOADING_NOTE_MS);
      // Each section fills in as its page lands, in credit order.
      for (const entry of entries) {
        entry.reply.then((data) => {
          entry.settled = data;
          redraw();
        });
      }
      Promise.all(entries.map((entry) => entry.reply)).then(() => clearTimeout(notes));
    };

    return { page, show, limit: ARTISTS_MAX };
  };

  window.shared = Object.assign(window.shared || {}, { playlistList, artistPages });
})();
