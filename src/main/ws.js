'use strict';

const EventEmitter = require('node:events');
const WebSocket = require('ws');

const api = require('./api');

const RECONNECT_MIN = 1000;
const RECONNECT_MAX = 15000;

/**
 * Realtime link to the API server's /api/v1/ws endpoint.
 *
 * Emits:
 *   'status'  ({ state, message })  state: connecting|connected|offline|unauthorized|denied
 *   'message' (parsed payload from the server)
 */
class RealtimeClient extends EventEmitter {
  constructor() {
    super();
    this.socket = null;
    this.timer = null;
    this.attempt = 0;
    this.stopped = false;
    this.state = 'connecting';
  }

  setStatus(state, message) {
    // Re-emit even when unchanged for terminal states, so a manual retry
    // always produces visible feedback in the renderer.
    this.state = state;
    this.emit('status', { state, message });
  }

  start() {
    this.stopped = false;
    this.connect();
  }

  /** Manual retry from the tray or the renderer's setup screen. */
  retry() {
    this.attempt = 0;
    this.stopped = false;
    this.teardown();
    clearTimeout(this.timer);
    this.connect();
  }

  stop() {
    this.stopped = true;
    clearTimeout(this.timer);
    this.teardown();
  }

  teardown() {
    if (!this.socket) return;
    const socket = this.socket;
    this.socket = null;
    socket.removeAllListeners();
    try {
      socket.terminate();
    } catch {
      /* already gone */
    }
  }

  scheduleReconnect() {
    if (this.stopped) return;
    clearTimeout(this.timer);
    const delay = Math.min(RECONNECT_MIN * 2 ** this.attempt, RECONNECT_MAX);
    this.attempt += 1;
    this.timer = setTimeout(() => this.connect(), delay);
  }

  async connect() {
    if (this.stopped) return;
    this.teardown();

    // Only advertise "connecting" on the first attempt or after a manual retry.
    // Once we know we are offline, staying offline while retrying in the
    // background beats flickering between the two on every backoff tick.
    if (this.attempt === 0 && this.state !== 'connected') this.setStatus('connecting');

    let token;
    try {
      token = await api.ensureToken();
    } catch (err) {
      if (err.code === 'DENIED') {
        // Do not re-open the authorisation dialog on a loop; wait for a manual retry.
        this.stopped = true;
        this.setStatus('denied', err.message);
        return;
      }
      this.setStatus(err.code === 'OFFLINE' ? 'offline' : 'unauthorized', err.message);
      this.scheduleReconnect();
      return;
    }

    const url = `${api.wsBase()}${api.API}/ws?token=${encodeURIComponent(token)}`;
    let socket;
    try {
      socket = new WebSocket(url, { handshakeTimeout: 5000 });
    } catch (err) {
      this.setStatus('offline', err.message);
      this.scheduleReconnect();
      return;
    }
    this.socket = socket;

    socket.on('open', () => {
      this.attempt = 0;
      this.setStatus('connected');
    });

    socket.on('message', (raw) => {
      let payload;
      try {
        payload = JSON.parse(raw.toString());
      } catch {
        return;
      }
      this.emit('message', payload);
    });

    socket.on('close', (code) => {
      if (this.socket !== socket) return;
      this.socket = null;
      if (code === 1008) {
        // Server rejected our token — drop it so the next attempt re-authorises.
        api.clearToken();
        this.setStatus('unauthorized', 'The API server rejected the saved token');
      } else {
        // A cached token makes ensureToken() skip the network entirely, so for
        // an unreachable server this close is the *only* signal we get. Report
        // offline whether or not we ever managed to connect.
        this.setStatus(
          'offline',
          this.state === 'connected' ? 'Connection closed' : 'Could not reach the API server',
        );
      }
      this.scheduleReconnect();
    });

    socket.on('error', () => {
      // 'close' always follows and handles both the status and the reconnect.
    });
  }
}

module.exports = { RealtimeClient };
