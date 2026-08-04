'use strict';

/**
 * Pulls a small palette out of the cover art: an accent for the controls, and
 * three washes that tint the whole card.
 *
 * Naive "most common colour" picks the muddy background of most album art, so
 * this buckets pixels by hue and weights each by how colourful it is. The
 * winning hues are then re-lit to fixed saturation/lightness bands so they stay
 * legible on both the dark and light glass.
 *
 * The washes are generated rather than being the cover itself. Blurring the
 * artwork drags every dark and desaturated region into the mix, which is what
 * made a bright cover come out brown.
 */
(() => {
  const HUE_BUCKETS = 24;
  const BUCKET_DEG = 360 / HUE_BUCKETS;
  const SAMPLE = 42;

  /** Used when the artwork has no hue to give — see `spread`. */
  const FALLBACK = { h: 352, s: 90, l: 67 };

  /** How far a runner-up must sit from a hue already taken, in buckets. */
  const HUE_SPACING = 2;
  /** A runner-up scoring below this share of the winner is not really present. */
  const SECOND_SHARE = 0.12;
  const THIRD_SHARE = 0.08;

  const canvas = document.createElement('canvas');
  canvas.width = SAMPLE;
  canvas.height = SAMPLE;
  const ctx = canvas.getContext('2d', { willReadFrequently: true });

  const clamp = (v, min, max) => Math.min(max, Math.max(min, v));

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

  const toRgba = (h, s, l, a) => {
    const [r, g, b] = hslToRgb(h, s / 100, l / 100);
    return `rgba(${r}, ${g}, ${b}, ${a})`;
  };

  // ------------------------------------------------------------- extraction

  /**
   * Hue histogram of the sampled pixels, weighted by how colourful each one is.
   */
  const histogram = (data) => {
    const buckets = new Float64Array(HUE_BUCKETS);
    const sat = new Float64Array(HUE_BUCKETS);
    let vivid = 0;
    let lightnessSum = 0;
    let opaque = 0;

    for (let i = 0; i < data.length; i += 4) {
      if (data[i + 3] < 200) continue;
      const [h, s, l] = rgbToHsl(data[i], data[i + 1], data[i + 2]);

      // Every opaque pixel counts towards how dark the artwork reads overall,
      // including the ones carrying no hue — a black-and-white cover is still a
      // dark cover.
      lightnessSum += l;
      opaque += 1;

      // Ignore near-black, near-white and washed-out pixels: they carry no hue.
      if (l < 0.14 || l > 0.93 || s < 0.16) continue;

      // Mid-lightness, high-saturation pixels are what the eye reads as "the
      // colour". Squaring saturation is the important part: it lets a small
      // patch of vivid red outvote a large wash of desaturated beige, which
      // counting pixels never would. The lightness term fades out pixels
      // drifting toward black or white, since those read as shading.
      const w = s * s * (1 - Math.abs(l - 0.5) * 1.2);
      const bucket = Math.floor((h / 360) * HUE_BUCKETS) % HUE_BUCKETS;
      buckets[bucket] += w;
      sat[bucket] += s * w;
      vivid += w;
    }

    return { buckets, sat, vivid, lightness: opaque ? lightnessSum / opaque : 0.5 };
  };

  /**
   * Score a bucket with its neighbours at half value. Without this, a hue
   * sitting right on a bucket boundary is split into two half-strength bins and
   * loses to a weaker but better-centred one.
   */
  const scoreOf = (buckets, i) =>
    buckets[i] +
    0.5 * buckets[(i + 1) % HUE_BUCKETS] +
    0.5 * buckets[(i - 1 + HUE_BUCKETS) % HUE_BUCKETS];

  /**
   * The `count` strongest hues, each far enough from the ones already taken to
   * be a different colour rather than the same one twice.
   */
  const dominantHues = (buckets, sat, count) => {
    const remaining = Float64Array.from(buckets);
    const picks = [];

    for (let n = 0; n < count; n += 1) {
      let best = -1;
      let bestScore = 0;
      for (let i = 0; i < HUE_BUCKETS; i += 1) {
        const score = scoreOf(remaining, i);
        if (score > bestScore) {
          bestScore = score;
          best = i;
        }
      }
      if (best < 0) break;

      picks.push({
        hue: (best + 0.5) * BUCKET_DEG,
        sat: buckets[best] > 0 ? sat[best] / buckets[best] : 0.6,
        score: bestScore,
      });

      // Take this hue and its neighbours out of the running so the next pick is
      // a genuinely different colour.
      for (let d = -HUE_SPACING; d <= HUE_SPACING; d += 1) {
        remaining[(best + d + HUE_BUCKETS * 2) % HUE_BUCKETS] = 0;
      }
    }

    return picks;
  };

  /**
   * Three hues to build the card from. A cover that really is one colour gets
   * an analogous spread off the winner rather than an invented second hue,
   * which is what keeps a monochrome sleeve from sprouting a stripe of
   * something unrelated.
   */
  const spread = (data) => {
    const { buckets, sat, vivid, lightness } = histogram(data);

    if (vivid < 2) {
      // Genuinely greyscale artwork. Keep the app's accent so the transport
      // still reads, but let the card stay as colourless as the cover.
      return {
        greyscale: true,
        lightness,
        hues: [
          { hue: FALLBACK.h, sat: 0.08 },
          { hue: FALLBACK.h, sat: 0.05 },
          { hue: FALLBACK.h, sat: 0.03 },
        ],
        accent: FALLBACK,
      };
    }

    const picks = dominantHues(buckets, sat, 3);
    const primary = picks[0];

    const second =
      picks[1] && picks[1].score >= primary.score * SECOND_SHARE
        ? picks[1]
        : { hue: primary.hue + 26, sat: primary.sat * 0.82 };
    const third =
      picks[2] && picks[2].score >= primary.score * THIRD_SHARE
        ? picks[2]
        : { hue: primary.hue - 22, sat: primary.sat * 0.7 };

    return {
      greyscale: false,
      lightness,
      hues: [primary, second, third],
      accent: {
        h: primary.hue,
        // Only the hue survives from the artwork. Saturation is the bucket's
        // weighted average pushed up, and lightness is pinned flat: the accent
        // has to stay legible on both the dark and light glass, and taking the
        // artwork's real lightness would give a near-black accent on a dark
        // cover.
        s: Math.round(clamp(primary.sat * 118, 62, 96)),
        l: 66,
      },
    };
  };

  // ------------------------------------------------------------------ washes

  /**
   * Turn the hues into the layers the card paints.
   *
   * Dark and light appearance need genuinely different values, not the same
   * colours at a different opacity: on dark glass the wash has to carry its own
   * light, and on light glass it has to stay out of the text's way.
   */
  const washes = (palette, dark) => {
    const [one, two, three] = palette.hues;

    // A dark cover gets a slightly deeper wash and a light one a slightly
    // airier one, so the card still feels like it came from that artwork.
    const lean = palette.lightness < 0.42 ? -4 : 3;

    const band = dark
      ? { s: [82, 72, 64], l: [43 + lean, 36 + lean, 30 + lean], a: [0.86, 0.74, 0.64], base: [48, 19, 0.74] }
      : { s: [94, 86, 76], l: [70 + lean, 76 + lean, 80 + lean], a: [0.84, 0.72, 0.62], base: [66, 87, 0.66] };

    // Pushed well past the artwork's own saturation, with a high floor: the
    // card is small and sits on translucent glass, so a wash that merely
    // matches the cover's chroma reads as grey by the time it is composited.
    const sat = (i, source) =>
      palette.greyscale ? band.s[i] * 0.14 : clamp(source * 100 * 1.5, band.s[i] * 0.8, band.s[i]);

    return {
      wash1: toRgba(one.hue, sat(0, one.sat), band.l[0], band.a[0]),
      wash2: toRgba(two.hue, sat(1, two.sat), band.l[1], band.a[1]),
      wash3: toRgba(three.hue, sat(2, three.sat), band.l[2], band.a[2]),
      washBase: toRgba(one.hue, palette.greyscale ? 4 : band.base[0], band.base[1], band.base[2]),
    };
  };

  const prefersDark = () =>
    typeof matchMedia === 'function' ? matchMedia('(prefers-color-scheme: dark)').matches : true;

  const build = (palette) => {
    const { h, s, l } = palette.accent;
    return {
      accent: toHex(h, s, l),
      accentSoft: toRgba(h, s, l, 0.34),
      ...washes(palette, prefersDark()),
    };
  };

  const FALLBACK_PALETTE = {
    greyscale: true,
    lightness: 0.5,
    hues: [
      { hue: FALLBACK.h, sat: 0.08 },
      { hue: FALLBACK.h, sat: 0.05 },
      { hue: FALLBACK.h, sat: 0.03 },
    ],
    accent: FALLBACK,
  };

  /**
   * @returns {Promise<{accent: string, accentSoft: string, wash1: string,
   *                    wash2: string, wash3: string, washBase: string}>}
   */
  const extract = (dataUrl) =>
    new Promise((resolve) => {
      const done = (palette) => resolve(build(palette));
      if (!dataUrl) return done(FALLBACK_PALETTE);

      const img = new Image();

      const measure = () => {
        try {
          ctx.clearRect(0, 0, SAMPLE, SAMPLE);
          ctx.drawImage(img, 0, 0, SAMPLE, SAMPLE);
          done(spread(ctx.getImageData(0, 0, SAMPLE, SAMPLE).data));
        } catch {
          done(FALLBACK_PALETTE);
        }
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
        img.decode().then(measure, () => done(FALLBACK_PALETTE));
      } else {
        img.onload = measure;
        img.onerror = () => done(FALLBACK_PALETTE);
      }
    });

  window.palette = { extract, FALLBACK_HEX: toHex(FALLBACK.h, FALLBACK.s, FALLBACK.l) };
})();
