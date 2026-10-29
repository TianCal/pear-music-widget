'use strict';

/**
 * Paints a cover's palette onto a style declaration: the accent for the
 * controls and the washes behind them. Covers can land faster than they are
 * sampled, so each painter keeps a token and the newest cover wins.
 */
(() => {
  /** `extract(cover, tint)` is `palette.js`'s; `style` is where the custom
   *  properties go (the card's root, the strip's own element). */
  const palettePainter = ({ extract, style }) => {
    let paletteToken = 0;
    return async (cover, tint) => {
      const token = ++paletteToken;
      const palette = await extract(cover, tint);
      if (token !== paletteToken) return; // a newer cover or tint won the race
      style.setProperty('--accent', palette.accent);
      style.setProperty('--accent-soft', palette.accentSoft);
      style.setProperty('--wash-1', palette.wash1);
      style.setProperty('--wash-2', palette.wash2);
      style.setProperty('--wash-3', palette.wash3);
      style.setProperty('--wash-base', palette.washBase);
    };
  };

  window.shared = Object.assign(window.shared || {}, { palettePainter });
})();
