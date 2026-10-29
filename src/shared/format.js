'use strict';

/**
 * The small helpers every page wants: a clock reading and a clamp.
 *
 * The card, the strip and Sort Songs are separate pages that share no
 * script scope, so what they share is reached through `window.shared`
 * rather than declared at the top level, where a second declaration of
 * the same name would stop a script from loading at all.
 */
(() => {
  /** `m:ss`, or `h:mm:ss` once there is an hour: an hour-long local file
   *  reads 1:01:40, never 61:40. */
  const formatTime = (seconds) => {
    const s = Math.max(0, Math.floor(seconds || 0));
    const h = Math.floor(s / 3600);
    const m = Math.floor((s % 3600) / 60);
    const sec = String(s % 60).padStart(2, '0');
    return h > 0 ? `${h}:${String(m).padStart(2, '0')}:${sec}` : `${m}:${sec}`;
  };

  const clamp = (v, min, max) => Math.min(max, Math.max(min, v));

  window.shared = Object.assign(window.shared || {}, { formatTime, clamp });
})();
