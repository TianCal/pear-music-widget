'use strict';

const store = require('./store');

const API = '/api/v1';

/**
 * Error codes surfaced to the renderer so it can render the right setup hint.
 *   OFFLINE      - nothing listening; YouTube Music closed or API Server plugin disabled
 *   UNAUTHORIZED - no token yet, or the stored token was rejected
 *   DENIED       - the user pressed "Deny" on the in-app authorisation dialog
 *   ERROR        - anything else
 */
class ApiError extends Error {
  constructor(code, message) {
    super(message);
    this.code = code;
  }
}

const httpBase = () => `http://${store.get('host')}:${store.get('port')}`;
const wsBase = () => `ws://${store.get('host')}:${store.get('port')}`;

const classify = (err) => {
  const cause = err?.cause?.code || err?.code;
  if (cause === 'ECONNREFUSED' || cause === 'EHOSTUNREACH' || cause === 'ENOTFOUND') {
    return new ApiError('OFFLINE', 'API server is not reachable');
  }
  if (err?.name === 'TimeoutError' || cause === 'ETIMEDOUT' || cause === 'UND_ERR_CONNECT_TIMEOUT') {
    return new ApiError('OFFLINE', 'API server timed out');
  }
  return new ApiError('ERROR', err?.message || String(err));
};

/** Ask the API server for a JWT. Pops a dialog inside YouTube Music the first time. */
const authorize = async () => {
  const id = store.get('clientId');
  let res;
  try {
    // No timeout: this blocks until the user answers the dialog in YouTube Music.
    res = await fetch(`${httpBase()}/auth/${encodeURIComponent(id)}`, { method: 'POST' });
  } catch (err) {
    throw classify(err);
  }

  if (res.status === 403) throw new ApiError('DENIED', 'Authorisation was denied in YouTube Music');
  if (!res.ok) throw new ApiError('ERROR', `Auth failed with HTTP ${res.status}`);

  const { accessToken } = await res.json();
  if (!accessToken) throw new ApiError('ERROR', 'Auth response contained no token');

  store.set({ token: accessToken });
  return accessToken;
};

/** Returns a usable token, requesting one if we do not have it cached. */
const ensureToken = async () => store.get('token') || (await authorize());

const clearToken = () => store.set({ token: null });

/**
 * Authenticated request against /api/v1. Retries once with a fresh token on 401/403,
 * which covers the case where the API server's secret was regenerated.
 */
const request = async (method, path, body, { retry = true } = {}) => {
  const token = await ensureToken();

  let res;
  try {
    res = await fetch(`${httpBase()}${API}${path}`, {
      method,
      headers: {
        Authorization: `Bearer ${token}`,
        ...(body === undefined ? {} : { 'Content-Type': 'application/json' }),
      },
      body: body === undefined ? undefined : JSON.stringify(body),
      signal: AbortSignal.timeout(4000),
    });
  } catch (err) {
    throw classify(err);
  }

  if ((res.status === 401 || res.status === 403) && retry) {
    clearToken();
    return request(method, path, body, { retry: false });
  }
  if (res.status === 401 || res.status === 403) {
    clearToken();
    throw new ApiError('UNAUTHORIZED', 'Token rejected by the API server');
  }
  if (res.status === 204) return null;
  if (!res.ok) throw new ApiError('ERROR', `HTTP ${res.status} on ${path}`);

  const text = await res.text();
  if (!text) return null;
  try {
    return JSON.parse(text);
  } catch {
    return null;
  }
};

const get = (path) => request('GET', path);
const post = (path, body) => request('POST', path, body);

// ---------------------------------------------------------------- cover art

// Cover URLs are stable per video, and the renderer needs them as data URLs so
// that <canvas> colour extraction is not blocked by cross-origin tainting.
const imageCache = new Map();
const IMAGE_CACHE_MAX = 40;

const fetchCover = async (src) => {
  if (!src) return null;
  if (imageCache.has(src)) return imageCache.get(src);

  let dataUrl = null;
  try {
    const res = await fetch(src, { signal: AbortSignal.timeout(8000) });
    if (res.ok) {
      const buf = Buffer.from(await res.arrayBuffer());
      const type = res.headers.get('content-type') || 'image/jpeg';
      dataUrl = `data:${type};base64,${buf.toString('base64')}`;
    }
  } catch {
    dataUrl = null;
  }

  if (dataUrl) {
    imageCache.set(src, dataUrl);
    if (imageCache.size > IMAGE_CACHE_MAX) {
      imageCache.delete(imageCache.keys().next().value);
    }
  }
  return dataUrl;
};

// ------------------------------------------------------------------ actions

const actions = {
  play: () => post('/play'),
  pause: () => post('/pause'),
  togglePlay: () => post('/toggle-play'),
  next: () => post('/next'),
  previous: () => post('/previous'),
  seekTo: (seconds) => post('/seek-to', { seconds }),
  setVolume: (volume) => post('/volume', { volume }),
  toggleMute: () => post('/toggle-mute'),
  like: () => post('/like'),
  dislike: () => post('/dislike'),
  shuffle: () => post('/shuffle'),
};

// /repeat-mode is intentionally not used: it answers null unconditionally on the
// shipped api-server plugin, so repeat state cannot be displayed truthfully.
const queries = {
  song: () => get('/song'),
  likeState: () => get('/like-state'),
  shuffleState: () => get('/shuffle'),
  volumeState: () => get('/volume'),
};

module.exports = {
  ApiError,
  actions,
  queries,
  authorize,
  ensureToken,
  clearToken,
  fetchCover,
  httpBase,
  wsBase,
  API,
};
