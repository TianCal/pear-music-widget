#!/usr/bin/env node
'use strict';

/**
 * Connectivity check for the YouTube Music / pear-desktop API server.
 * Run with `npm run doctor` when the widget shows a setup screen.
 */

const WebSocket = require('ws');

const HOST = process.env.PMW_HOST || '127.0.0.1';
const PORT = Number(process.env.PMW_PORT || 26538);
const CLIENT_ID = 'PearMusicWidget-doctor';

const base = `http://${HOST}:${PORT}`;
const ok = (msg) => console.log(`  \x1b[32m✓\x1b[0m ${msg}`);
const bad = (msg) => console.log(`  \x1b[31m✗\x1b[0m ${msg}`);
const info = (msg) => console.log(`    \x1b[2m${msg}\x1b[0m`);

const main = async () => {
  console.log(`\nChecking ${base}\n`);

  // 1. Is anything listening?
  let doc;
  try {
    const res = await fetch(`${base}/doc`, { signal: AbortSignal.timeout(3000) });
    doc = await res.json();
    ok(`API server reachable — ${doc.info?.title || 'unknown server'}`);
  } catch (err) {
    bad('API server not reachable');
    info(err.cause?.code === 'ECONNREFUSED' ? 'Nothing is listening on that port.' : err.message);
    info('Open YouTube Music → menu → Plugins → enable "API Server", and check its port.');
    process.exitCode = 1;
    return;
  }

  // 2. Auth. With authStrategy AUTH_AT_FIRST this pops a dialog in the app.
  let token;
  try {
    const res = await fetch(`${base}/auth/${CLIENT_ID}`, { method: 'POST' });
    if (res.status === 403) {
      bad('Authorisation denied in YouTube Music');
      process.exitCode = 1;
      return;
    }
    token = (await res.json()).accessToken;
    ok('Access token issued');
  } catch (err) {
    bad(`Auth request failed: ${err.message}`);
    process.exitCode = 1;
    return;
  }

  // 3. Authenticated read.
  try {
    const res = await fetch(`${base}/api/v1/song`, {
      headers: { Authorization: `Bearer ${token}` },
      signal: AbortSignal.timeout(3000),
    });
    if (res.status === 200) {
      const song = await res.json();
      ok(`Song endpoint OK — ${song.title ? `"${song.title}"` : 'nothing playing'}`);
    } else if (res.status === 204) {
      ok('Song endpoint OK — nothing playing');
    } else {
      bad(`Song endpoint returned HTTP ${res.status}`);
      process.exitCode = 1;
    }
  } catch (err) {
    bad(`Song endpoint failed: ${err.message}`);
    process.exitCode = 1;
  }

  // 4. Realtime channel.
  await new Promise((resolve) => {
    const ws = new WebSocket(`ws://${HOST}:${PORT}/api/v1/ws?token=${encodeURIComponent(token)}`);
    const timer = setTimeout(() => {
      bad('WebSocket connected but sent no initial state within 5s');
      process.exitCode = 1;
      ws.terminate();
      resolve();
    }, 5000);

    ws.on('message', (raw) => {
      clearTimeout(timer);
      let type = 'unknown';
      try {
        type = JSON.parse(raw.toString()).type;
      } catch {
        /* ignore */
      }
      ok(`WebSocket streaming — first frame: ${type}`);
      ws.close();
      resolve();
    });

    ws.on('error', (err) => {
      clearTimeout(timer);
      bad(`WebSocket failed: ${err.message}`);
      process.exitCode = 1;
      resolve();
    });
  });

  console.log('');
};

main();
