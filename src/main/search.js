'use strict';

/**
 * `POST /api/v1/search` returns YouTube Music's raw innertube payload — around
 * 270KB of deeply nested renderers with no stable path to the results. Rather
 * than walk a fixed path that a client update would break, collect every
 * `musicResponsiveListItemRenderer` in the tree and keep the ones that carry a
 * videoId. Songs, albums and videos all use that renderer, so one pass gets the
 * lot in roughly the order the app displays them.
 */

const MAX_RESULTS = 10;
const MAX_DEPTH = 40;

const collectItems = (node, out, depth = 0) => {
  if (!node || depth > MAX_DEPTH || out.length > 200) return;

  if (Array.isArray(node)) {
    for (const child of node) collectItems(child, out, depth + 1);
    return;
  }
  if (typeof node !== 'object') return;

  if (node.musicResponsiveListItemRenderer) out.push(node.musicResponsiveListItemRenderer);
  for (const value of Object.values(node)) collectItems(value, out, depth + 1);
};

/** Albums and videos hang their id off the play overlay rather than playlistItemData. */
const videoIdOf = (item) =>
  item?.playlistItemData?.videoId ||
  item?.overlay?.musicItemThumbnailOverlayRenderer?.content?.musicPlayButtonRenderer
    ?.playNavigationEndpoint?.watchEndpoint?.videoId ||
  null;

const columnText = (item, index) => {
  const runs =
    item?.flexColumns?.[index]?.musicResponsiveListItemFlexColumnRenderer?.text?.runs;
  if (!Array.isArray(runs)) return '';
  return runs.map((run) => run?.text ?? '').join('');
};

/** Smallest thumbnail is plenty for a 30px row and keeps the fetch cheap. */
const thumbnailOf = (item) => {
  const list = item?.thumbnail?.musicThumbnailRenderer?.thumbnail?.thumbnails;
  return Array.isArray(list) && list.length ? list[0].url : null;
};

const parseSearchResults = (payload) => {
  const items = [];
  collectItems(payload, items);

  const seen = new Set();
  const results = [];

  for (const item of items) {
    const videoId = videoIdOf(item);
    if (!videoId || seen.has(videoId)) continue;

    const title = columnText(item, 0).trim();
    if (!title) continue;

    seen.add(videoId);
    results.push({
      videoId,
      title,
      subtitle: columnText(item, 1).trim(),
      thumbnail: thumbnailOf(item),
    });

    if (results.length >= MAX_RESULTS) break;
  }

  return results;
};

/**
 * Index of a videoId in `GET /queue`. Items are not uniformly shaped — some are
 * `playlistPanelVideoRenderer`, others are wrapped — so search each item's
 * subtree rather than assuming a path.
 */
const findQueueIndex = (queue, videoId) => {
  const items = queue?.items;
  if (!Array.isArray(items) || !videoId) return -1;

  const contains = (node, depth = 0) => {
    if (!node || depth > 12) return false;
    if (Array.isArray(node)) return node.some((child) => contains(child, depth + 1));
    if (typeof node !== 'object') return false;
    if (node.videoId === videoId) return true;
    return Object.values(node).some((value) => contains(value, depth + 1));
  };

  return items.findIndex((item) => contains(item));
};

/** Queue items come either bare or wrapped; unwrap to the video renderer. */
const videoRenderer = (item) =>
  item?.playlistPanelVideoRenderer ||
  item?.playlistPanelVideoWrapperRenderer?.primaryRenderer?.playlistPanelVideoRenderer ||
  null;

const runsText = (node) =>
  Array.isArray(node?.runs) ? node.runs.map((run) => run?.text ?? '').join('') : '';

/**
 * The tracks queued after the one playing, for the stack skin's "Next tracks".
 * `selected` marks the current item; anything before it has already played.
 */
const parseQueueUpcoming = (queue, limit = 4) => {
  const items = Array.isArray(queue?.items) ? queue.items : [];
  const renderers = items.map(videoRenderer);

  const current = renderers.findIndex((r) => r?.selected === true);
  const start = current >= 0 ? current + 1 : 0;

  const out = [];
  for (let i = start; i < renderers.length && out.length < limit; i += 1) {
    const r = renderers[i];
    if (!r?.videoId) continue;
    out.push({
      videoId: r.videoId,
      title: runsText(r.title).trim(),
      artist: runsText(r.longBylineText || r.shortBylineText).split('•')[0].trim(),
      thumbnail: r.thumbnail?.thumbnails?.[0]?.url || null,
    });
  }
  return out;
};

module.exports = { parseSearchResults, findQueueIndex, parseQueueUpcoming, MAX_RESULTS };
