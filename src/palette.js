'use strict';

/**
 * Pulls a usable accent colour out of the cover art.
 *
 * Naive "most common colour" picks the muddy background of most album art, so
 * this buckets pixels by hue and weights each by how colourful it is. The
 * winning hue is then re-lit to a fixed saturation/lightness band so the accent
 * stays legible on both the dark and light glass backgrounds.
 */
(() => {
  const HUE_BUCKETS = 24;
  const SAMPLE = 42;
  const FALLBACK = { h: 352, s: 90, l: 67 };

  const canvas = document.createElement('canvas');
  canvas.width = SAMPLE;
  canvas.height = SAMPLE;
  const ctx = canvas.getContext('2d', { willReadFrequently: true });

  const rgbToHsl = (r, g, b) => {
    r /= 255;
    g /= 255;
    b /= 255;
    const max = Math.max(r, g, b);
    const min = Math.min(r, g, b);
    const l = (max + min) / 2;
    const d = max - min;
    if (d === 0) return [0, 0, l];

    const s = l > 0.5 ? d / (2 - max - min) : d / (max + min);
    let h;
    if (max === r) h = ((g - b) / d + (g < b ? 6 : 0)) / 6;
    else if (max === g) h = ((b - r) / d + 2) / 6;
    else h = ((r - g) / d + 4) / 6;
    return [h * 360, s, l];
  };

  const hslToRgb = (h, s, l) => {
    h = ((h % 360) + 360) % 360;
    const c = (1 - Math.abs(2 * l - 1)) * s;
    const x = c * (1 - Math.abs(((h / 60) % 2) - 1));
    const m = l - c / 2;
    const [r, g, b] =
      h < 60 ? [c, x, 0]
      : h < 120 ? [x, c, 0]
      : h < 180 ? [0, c, x]
      : h < 240 ? [0, x, c]
      : h < 300 ? [x, 0, c]
      : [c, 0, x];
    return [r + m, g + m, b + m].map((v) => Math.round(v * 255));
  };

  const toHex = (h, s, l) =>
    `#${hslToRgb(h, s / 100, l / 100)
      .map((v) => v.toString(16).padStart(2, '0'))
      .join('')}`;

  /** @returns {Promise<{hex: string, soft: string}>} */
  const extract = (dataUrl) =>
    new Promise((resolve) => {
      const done = ({ h, s, l }) => {
        const [r, g, b] = hslToRgb(h, s / 100, l / 100);
        resolve({ hex: toHex(h, s, l), soft: `rgba(${r}, ${g}, ${b}, 0.34)` });
      };

      if (!dataUrl) return done(FALLBACK);

      const img = new Image();

      const measure = () => {
        let data;
        try {
          ctx.clearRect(0, 0, SAMPLE, SAMPLE);
          ctx.drawImage(img, 0, 0, SAMPLE, SAMPLE);
          data = ctx.getImageData(0, 0, SAMPLE, SAMPLE).data;
        } catch {
          return done(FALLBACK);
        }

        const weight = new Float64Array(HUE_BUCKETS);
        const satSum = new Float64Array(HUE_BUCKETS);
        let vividTotal = 0;

        for (let i = 0; i < data.length; i += 4) {
          if (data[i + 3] < 200) continue;
          const [h, s, l] = rgbToHsl(data[i], data[i + 1], data[i + 2]);
          // Ignore near-black, near-white and washed-out pixels: they carry no hue.
          if (l < 0.14 || l > 0.93 || s < 0.16) continue;

          // Mid-lightness, high-saturation pixels are what the eye reads as "the colour".
          const w = s * s * (1 - Math.abs(l - 0.5) * 1.2);
          const bucket = Math.floor((h / 360) * HUE_BUCKETS) % HUE_BUCKETS;
          weight[bucket] += w;
          satSum[bucket] += s * w;
          vividTotal += w;
        }

        if (vividTotal < 2) return done(FALLBACK); // greyscale artwork

        // Fold in neighbouring buckets so a hue split across a boundary still wins.
        let best = 0;
        let bestScore = -1;
        for (let i = 0; i < HUE_BUCKETS; i += 1) {
          const score =
            weight[i] +
            0.5 * weight[(i + 1) % HUE_BUCKETS] +
            0.5 * weight[(i - 1 + HUE_BUCKETS) % HUE_BUCKETS];
          if (score > bestScore) {
            bestScore = score;
            best = i;
          }
        }

        const hue = (best + 0.5) * (360 / HUE_BUCKETS);
        const avgSat = weight[best] > 0 ? satSum[best] / weight[best] : 0.6;

        done({
          h: hue,
          s: Math.round(Math.min(96, Math.max(62, avgSat * 118))),
          l: 66,
        });
      };

      img.src = dataUrl;

      // `decode()` rather than `onload`, because the two mean different things:
      // onload fires once the bytes are in, decode() only once there is a
      // bitmap ready to draw. In a webview that is hidden — the dropdown, most
      // of the time — an image can be loaded but not yet rasterised, and
      // `drawImage` then paints nothing at all. That read as greyscale artwork
      // and fell back to the default accent, so a track whose cover was
      // resolved while the dropdown was closed came up pink in the dropdown and
      // its real colour in the widget.
      if (typeof img.decode === 'function') {
        img.decode().then(measure, () => done(FALLBACK));
      } else {
        img.onload = measure;
        img.onerror = () => done(FALLBACK);
      }
    });

  window.palette = { extract, FALLBACK_HEX: toHex(FALLBACK.h, FALLBACK.s, FALLBACK.l) };
})();
