#!/usr/bin/env node
'use strict';

/**
 * Generates the app icon and the menu-bar icons from scratch — no image
 * libraries, no binary assets checked in.
 *
 *   build/icon.icns        Big Sur style rounded square with a play glyph
 *   src/main/tray-icons.js Template images for the menu bar, solid + hollow
 *
 *   node scripts/make-icon.js
 */

const { execFileSync } = require('node:child_process');
const fs = require('node:fs');
const path = require('node:path');
const zlib = require('node:zlib');

const ROOT = path.join(__dirname, '..');
const BUILD = path.join(ROOT, 'build');
const ICONSET = path.join(BUILD, 'icon.iconset');

const SIZE = 1024;
const MARGIN = 100; // Big Sur icons float inside a transparent margin
const RADIUS = 185; // ~22.4% of the visible square
const SS = 3; // supersampling factor for anti-aliasing

// Warm red -> orange, matching the widget's default accent.
const TOP = [255, 90, 110];
const BOTTOM = [255, 138, 74];

const writePng = (file, width, height, pixels) => {
  const raw = Buffer.alloc((width * 4 + 1) * height);
  for (let y = 0; y < height; y += 1) {
    raw[y * (width * 4 + 1)] = 0; // filter: none
    pixels.copy(raw, y * (width * 4 + 1) + 1, y * width * 4, (y + 1) * width * 4);
  }
  const chunk = (type, data) => {
    const body = Buffer.concat([Buffer.from(type), data]);
    const len = Buffer.alloc(4);
    len.writeUInt32BE(data.length);
    const crc = Buffer.alloc(4);
    crc.writeUInt32BE(zlib.crc32 ? zlib.crc32(body) : crc32(body));
    return Buffer.concat([len, body, crc]);
  };
  const ihdr = Buffer.alloc(13);
  ihdr.writeUInt32BE(width, 0);
  ihdr.writeUInt32BE(height, 4);
  ihdr[8] = 8; // bit depth
  ihdr[9] = 6; // RGBA
  fs.writeFileSync(
    file,
    Buffer.concat([
      Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]),
      chunk('IHDR', ihdr),
      chunk('IDAT', zlib.deflateSync(raw, { level: 9 })),
      chunk('IEND', Buffer.alloc(0)),
    ]),
  );
};

// Node only exposes zlib.crc32 from v20.15; keep a fallback.
let CRC_TABLE = null;
const crc32 = (buf) => {
  if (!CRC_TABLE) {
    CRC_TABLE = new Int32Array(256);
    for (let n = 0; n < 256; n += 1) {
      let c = n;
      for (let k = 0; k < 8; k += 1) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
      CRC_TABLE[n] = c;
    }
  }
  let crc = -1;
  for (let i = 0; i < buf.length; i += 1) crc = CRC_TABLE[(crc ^ buf[i]) & 0xff] ^ (crc >>> 8);
  return (crc ^ -1) >>> 0;
};

/** Signed distance to a rounded rectangle; negative inside. */
const roundedRectSdf = (px, py, cx, cy, halfW, halfH, r) => {
  const qx = Math.abs(px - cx) - (halfW - r);
  const qy = Math.abs(py - cy) - (halfH - r);
  const outside = Math.hypot(Math.max(qx, 0), Math.max(qy, 0));
  return outside + Math.min(Math.max(qx, qy), 0) - r;
};

const render = () => {
  const pixels = Buffer.alloc(SIZE * SIZE * 4);
  const c = SIZE / 2;
  const half = (SIZE - MARGIN * 2) / 2;

  // Play triangle: equilateral-ish, optically centred (nudged right of centre).
  const triH = 300;
  const triW = 260;
  const triCx = c + 26;
  const inTriangle = (x, y) => {
    const dy = y - c;
    if (Math.abs(dy) > triH / 2) return false;
    const edge = triCx - triW / 2 + (1 - Math.abs(dy) / (triH / 2)) * triW;
    return x >= triCx - triW / 2 && x <= edge;
  };

  for (let y = 0; y < SIZE; y += 1) {
    for (let x = 0; x < SIZE; x += 1) {
      let bodyCoverage = 0;
      let glyphCoverage = 0;

      for (let sy = 0; sy < SS; sy += 1) {
        for (let sx = 0; sx < SS; sx += 1) {
          const fx = x + (sx + 0.5) / SS;
          const fy = y + (sy + 0.5) / SS;
          if (roundedRectSdf(fx, fy, c, c, half, half, RADIUS) <= 0) bodyCoverage += 1;
          if (inTriangle(fx, fy)) glyphCoverage += 1;
        }
      }

      const samples = SS * SS;
      const body = bodyCoverage / samples;
      if (body === 0) continue;

      const t = y / SIZE;
      const base = [0, 1, 2].map((i) => Math.round(TOP[i] + (BOTTOM[i] - TOP[i]) * t));
      const glyph = Math.min(glyphCoverage / samples, body);

      const i = (y * SIZE + x) * 4;
      pixels[i] = Math.round(base[0] * (1 - glyph) + 255 * glyph);
      pixels[i + 1] = Math.round(base[1] * (1 - glyph) + 255 * glyph);
      pixels[i + 2] = Math.round(base[2] * (1 - glyph) + 255 * glyph);
      pixels[i + 3] = Math.round(body * 255);
    }
  }
  return pixels;
};

// ------------------------------------------------------------ menu-bar icons

// Template images: black with an alpha mask, which macOS re-tints for light and
// dark menu bars. Drawn at 18pt (and @2x) with the glyph occupying ~41% of the
// canvas radius, a touch smaller than the system Now Playing icon.
const TRAY_RADIUS_RATIO = 0.414;
const TRAY_STROKE_RATIO = 0.108;

/** Play triangle inscribed in a circle of radius r centred on the canvas. */
const inPlayGlyph = (dx, dy, r) => {
  const tx = dx / r;
  const ty = dy / r;
  return tx >= -0.34 && tx <= 0.46 && Math.abs(ty) <= (0.46 - tx) * 0.55;
};

const renderTray = (S, hollow) => {
  const pixels = Buffer.alloc(S * S * 4);
  const c = (S - 1) / 2;
  const outer = S * TRAY_RADIUS_RATIO;
  const inner = outer - S * TRAY_STROKE_RATIO;
  const ss = 4;

  for (let y = 0; y < S; y += 1) {
    for (let x = 0; x < S; x += 1) {
      let covered = 0;
      for (let sy = 0; sy < ss; sy += 1) {
        for (let sx = 0; sx < ss; sx += 1) {
          const dx = x + (sx + 0.5) / ss - 0.5 - c;
          const dy = y + (sy + 0.5) / ss - 0.5 - c;
          const dist = Math.hypot(dx, dy);
          if (dist > outer) continue;

          if (hollow) {
            // Ring plus a solid play triangle sitting inside it.
            if (dist >= inner && !inPlayGlyph(dx, dy, inner)) covered += 1;
            else if (dist < inner && inPlayGlyph(dx, dy, inner * 0.92)) covered += 1;
          } else if (!inPlayGlyph(dx, dy, outer)) {
            // Solid disc with the triangle knocked out.
            covered += 1;
          }
        }
      }
      if (!covered) continue;
      const i = (y * S + x) * 4;
      pixels[i] = pixels[i + 1] = pixels[i + 2] = 0;
      pixels[i + 3] = Math.round((covered / (ss * ss)) * 255);
    }
  }
  return pixels;
};

const trayBase64 = (S, hollow) => {
  const tmp = path.join(BUILD, `tray-${S}-${hollow ? 'hollow' : 'solid'}.png`);
  writePng(tmp, S, S, renderTray(S, hollow));
  const b64 = fs.readFileSync(tmp).toString('base64');
  fs.rmSync(tmp);
  return b64;
};

fs.mkdirSync(BUILD, { recursive: true });
fs.writeFileSync(
  path.join(ROOT, 'src', 'main', 'tray-icons.js'),
  `'use strict';\n\n` +
    `// GENERATED by scripts/make-icon.js — do not edit by hand.\n` +
    `// Menu-bar template images: 'solid' while connected, 'hollow' when the\n` +
    `// YouTube Music API server cannot be reached.\n\n` +
    `module.exports = {\n` +
    `  solid: {\n    x1: '${trayBase64(18, false)}',\n    x2: '${trayBase64(36, false)}',\n  },\n` +
    `  hollow: {\n    x1: '${trayBase64(18, true)}',\n    x2: '${trayBase64(36, true)}',\n  },\n` +
    `};\n`,
);
console.log('wrote src/main/tray-icons.js');

// ---------------------------------------------------------------- app icon

fs.rmSync(ICONSET, { recursive: true, force: true });
fs.mkdirSync(ICONSET, { recursive: true });

const master = path.join(BUILD, 'icon.png');
writePng(master, SIZE, SIZE, render());

for (const size of [16, 32, 128, 256, 512]) {
  for (const scale of [1, 2]) {
    const px = size * scale;
    const name = scale === 1 ? `icon_${size}x${size}.png` : `icon_${size}x${size}@2x.png`;
    execFileSync('sips', ['-z', String(px), String(px), master, '--out', path.join(ICONSET, name)], {
      stdio: 'ignore',
    });
  }
}

execFileSync('iconutil', ['-c', 'icns', ICONSET, '-o', path.join(BUILD, 'icon.icns')]);
fs.rmSync(ICONSET, { recursive: true, force: true });

console.log('wrote build/icon.icns and build/icon.png');
