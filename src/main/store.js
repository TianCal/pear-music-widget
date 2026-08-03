'use strict';

const fs = require('node:fs');
const path = require('node:path');
const { app } = require('electron');

const DEFAULTS = {
  host: '127.0.0.1',
  port: 26538,
  clientId: 'PearMusicWidget',
  token: null,
  bounds: null, // last window position
  sizes: {}, // last width the user chose, per skin
  skin: 'classic', // floating widget layout: 'classic' | 'stack'
  panelSkin: 'classic', // menu-bar dropdown layout, chosen independently
  alwaysOnTop: true,
  opacity: 1,
};

let cache = null;
let file = null;

const filePath = () => {
  if (!file) file = path.join(app.getPath('userData'), 'settings.json');
  return file;
};

const load = () => {
  if (cache) return cache;
  try {
    cache = { ...DEFAULTS, ...JSON.parse(fs.readFileSync(filePath(), 'utf8')) };
  } catch {
    cache = { ...DEFAULTS };
  }
  return cache;
};

const get = (key) => load()[key];

const set = (patch) => {
  cache = { ...load(), ...patch };
  try {
    fs.mkdirSync(path.dirname(filePath()), { recursive: true });
    fs.writeFileSync(filePath(), JSON.stringify(cache, null, 2));
  } catch (err) {
    console.error('[store] failed to persist settings:', err.message);
  }
  return cache;
};

module.exports = { get, set, load, filePath, DEFAULTS };
