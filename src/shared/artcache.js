'use strict';

/**
 * A bounded map of artwork data URLs by id, one per page that draws queue
 * rows. Artwork is asked for by id, for the rows a surface is actually
 * showing, and what has arrived survives a queue change: the same track
 * scrolled back to is already paid for.
 *
 * The Rust cache has the same byte and entry ceilings; bounding this copy
 * matters because every webview owns its own JavaScript heap. Data URLs are
 * ASCII, so a character budget mirrors the source-byte budget even if the
 * engine's storage differs. Map iteration order is the eviction order: `get`
 * reinserts a hit to promote it, `peek` leaves the order alone.
 */
(() => {
  const ART_CACHE_CHARS = 4 * 1024 * 1024;
  const ART_CACHE_MAX = 256;

  const artCache = ({ chars = ART_CACHE_CHARS, max = ART_CACHE_MAX } = {}) => {
    const held = new Map();
    let used = 0;

    const drop = (id) => {
      const art = held.get(id);
      if (art === undefined) return;
      held.delete(id);
      used -= id.length + art.length;
    };

    const set = (id, art) => {
      drop(id);
      held.set(id, art);
      used += id.length + art.length;
      while (held.size > max || used > chars) {
        const oldest = held.keys().next().value;
        if (oldest === undefined) break;
        drop(oldest);
      }
    };

    const get = (id) => {
      const art = held.get(id);
      if (art !== undefined) {
        held.delete(id);
        held.set(id, art);
      }
      return art;
    };

    return {
      has: (id) => held.has(id),
      peek: (id) => held.get(id),
      get,
      set,
      drop,
      keys: () => held.keys(),
      get size() {
        return held.size;
      },
    };
  };

  window.shared = Object.assign(window.shared || {}, { artCache });
})();
