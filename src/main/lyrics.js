'use strict';

/**
 * Synced lyrics from LRCLib — the same source YouTube Music's own
 * `synced-lyrics` plugin uses. The api-server exposes no lyrics route, so the
 * widget has to ask for them itself.
 *
 * This is the only place the app talks to anything other than localhost.
 */

const ENDPOINT = 'https://lrclib.net/api';
const USER_AGENT = 'pear-music-widget (https://github.com/TianCal/pear-music-widget)';
const TIMEOUT_MS = 9000;
const CACHE_MAX = 60;

const cache = new Map();

const request = async (path) => {
  const res = await fetch(`${ENDPOINT}${path}`, {
    headers: { 'User-Agent': USER_AGENT },
    signal: AbortSignal.timeout(TIMEOUT_MS),
  });
  if (!res.ok) return null;
  return res.json();
};

/**
 * YouTube Music titles carry a lot that LRCLib will not match on — bracketed
 * soundtrack credits, 《…》 wrappers, "Official Music Video". Strip them for the
 * fallback search, but try the untouched title first: for plenty of tracks the
 * full string is the exact match.
 */
const cleanTitle = (title) => {
  const stripped = title
    .replace(/\([^)]*\)/g, ' ')
    .replace(/\[[^\]]*\]/g, ' ')
    .replace(/《[^》]*》/g, ' ')
    .replace(/【[^】]*】/g, ' ')
    .replace(/\b(official|music\s+video|lyric[s]?|audio|m\/?v|hd|4k)\b/gi, ' ')
    .replace(/\s+/g, ' ')
    .trim();
  return stripped || title;
};

/** Collaborations are listed several ways; LRCLib matches on the lead artist. */
const leadArtist = (artist) =>
  (artist || '')
    .split(/、|,|&|\band\b|feat\.?|ft\.?|with/i)[0]
    .replace(/\s+/g, ' ')
    .trim();

/**
 * `[mm:ss.xx]text`, possibly several stamps per line. Blank lines are kept —
 * they are the instrumental gaps, and the roll needs them to breathe.
 */
const parseLrc = (lrc) => {
  const lines = [];
  for (const raw of String(lrc).split('\n')) {
    const stamps = [...raw.matchAll(/\[(\d+):(\d{1,2}(?:[.:]\d{1,3})?)\]/g)];
    if (!stamps.length) continue;

    const text = raw.replace(/\[[^\]]*\]/g, '').trim();
    for (const stamp of stamps) {
      const seconds = Number(stamp[1]) * 60 + parseFloat(stamp[2].replace(':', '.'));
      if (Number.isFinite(seconds)) lines.push({ time: seconds, text });
    }
  }
  lines.sort((a, b) => a.time - b.time);
  return lines;
};

const shape = (record, how) => {
  if (!record) return null;
  if (record.syncedLyrics) {
    const lines = parseLrc(record.syncedLyrics);
    if (lines.length) return { synced: true, lines, how };
  }
  if (record.plainLyrics) {
    const lines = record.plainLyrics
      .split('\n')
      .map((text) => ({ time: null, text: text.trim() }));
    if (lines.some((line) => line.text)) return { synced: false, lines, how };
  }
  return null;
};

/** Prefer a search hit whose duration is closest to the track we are playing. */
const bestSearchHit = (hits, duration) => {
  if (!Array.isArray(hits)) return null;
  const synced = hits.filter((hit) => hit?.syncedLyrics);
  const pool = synced.length ? synced : hits.filter((hit) => hit?.plainLyrics);
  if (!pool.length) return null;
  if (!duration) return pool[0];

  return pool.reduce((best, hit) => {
    const delta = Math.abs((hit.duration || 0) - duration);
    const bestDelta = Math.abs((best.duration || 0) - duration);
    return delta < bestDelta ? hit : best;
  });
};

/**
 * @returns {Promise<{synced: boolean, lines: {time: number|null, text: string}[], how: string} | null>}
 */
const fetchLyrics = async (song) => {
  if (!song?.videoId) return null;
  if (cache.has(song.videoId)) return cache.get(song.videoId);

  const title = song.title || '';
  const artist = leadArtist(song.artist);
  const duration = Math.round(song.songDuration || 0);
  const query = (params) => `/get?${new URLSearchParams(params)}`;

  let result = null;
  try {
    // 1. Exact, with everything we know.
    if (title && artist) {
      const params = { track_name: title, artist_name: artist };
      if (song.album) params.album_name = song.album;
      if (duration) params.duration = String(duration);
      result = shape(await request(query(params)), 'exact');
    }

    // 2. Exact again on a cleaned title, without album or duration to pin it.
    const simple = cleanTitle(title);
    if (!result && simple && simple !== title && artist) {
      result = shape(await request(query({ track_name: simple, artist_name: artist })), 'cleaned');
    }

    // 3. Free-text search — this is what rescues soundtrack and 《…》 titles.
    if (!result && simple) {
      const hits = await request(`/search?${new URLSearchParams({ q: `${simple} ${artist}`.trim() })}`);
      result = shape(bestSearchHit(hits, duration), 'search');
    }
  } catch {
    return null; // offline or LRCLib down: the panel just says so
  }

  cache.set(song.videoId, result);
  if (cache.size > CACHE_MAX) cache.delete(cache.keys().next().value);
  return result;
};

module.exports = { fetchLyrics, parseLrc, cleanTitle, leadArtist };
