'use strict';

/**
 * The volume rules the card and the strip share.
 *
 * Every volume we POST comes back as a VOLUME_CHANGED echo. While the user is
 * turning it there are several in flight at once, and the last ones to land
 * are not necessarily the newest — that is what made the slider jump on
 * release. So: throttle what we send, always send the final value, and hold
 * our own value against echoes until the server has had time to settle on it.
 *
 * A trackpad emits many small wheel deltas where a mouse wheel emits few
 * large ones. Carrying a float between events lets both feel proportional
 * instead of the trackpad being ignored to rounding.
 */
(() => {
  const VOLUME_THROTTLE_MS = 70;
  const VOLUME_SETTLE_MS = 900;
  const WHEEL_SENSITIVITY = 0.12;
  const WHEEL_CARRY_MS = 400;

  const clamp = (v, min, max) => Math.min(max, Math.max(min, v));

  /**
   * `apply(volume)` writes the clamped value into the page's own state and
   * draws it; `volume()` reads it back when a send is due, so what goes out
   * is the latest value, not the one that started the timer; `send(volume)`
   * posts the rounded value.
   */
  const volumeControl = ({ apply, volume, send }) => {
    let holdUntil = 0;
    let sendTimer = null;
    let carry = null;
    let carryTimer = null;

    const flush = () => {
      clearTimeout(sendTimer);
      sendTimer = null;
      send(Math.round(volume()));
    };

    /** Throttle sends to one per 70 ms; `immediate` sends now, for the value
     *  under a released pointer or a key press. */
    const set = (value, { immediate = false } = {}) => {
      apply(clamp(value, 0, 100));
      holdUntil = performance.now() + VOLUME_SETTLE_MS;
      if (immediate) {
        flush();
        return;
      }
      if (sendTimer) return;
      sendTimer = setTimeout(() => {
        sendTimer = null;
        send(Math.round(volume()));
      }, VOLUME_THROTTLE_MS);
    };

    /** While this holds, our own value beats the echoes still in flight. */
    const holding = () => performance.now() < holdUntil;

    /** One wheel step. `from` is the level a fresh gesture starts at; a
     *  gesture still carrying (within 400 ms of its last step) ignores it.
     *  With macOS natural scrolling, swiping up reports a positive deltaY,
     *  so adding it is what makes "scroll up" raise the volume. */
    const wheel = (deltaY, from) => {
      carry = clamp((carry ?? from) + deltaY * WHEEL_SENSITIVITY, 0, 100);
      clearTimeout(carryTimer);
      carryTimer = setTimeout(() => {
        carry = null;
      }, WHEEL_CARRY_MS);
      set(carry);
    };

    /** A key step starts from the real level, not a wheel gesture's carry. */
    const dropCarry = () => {
      carry = null;
    };

    return { set, holding, wheel, dropCarry };
  };

  window.shared = Object.assign(window.shared || {}, { volumeControl });
})();
