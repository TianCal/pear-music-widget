'use strict';

const { contextBridge, ipcRenderer } = require('electron');

contextBridge.exposeInMainWorld('widget', {
  getState: () => ipcRenderer.invoke('widget:state'),
  onState: (handler) => {
    const listener = (_event, state) => handler(state);
    ipcRenderer.on('state', listener);
    return () => ipcRenderer.off('state', listener);
  },
  command: (name, payload) => ipcRenderer.invoke('widget:command', name, payload),
  setAppearance: (appearance) => ipcRenderer.invoke('widget:appearance', appearance),
  setSkin: (skin) => ipcRenderer.invoke('widget:skin', skin),
  openApp: () => ipcRenderer.invoke('widget:open-app'),
  search: (query) => ipcRenderer.invoke('widget:search', query),
  playResult: (videoId) => ipcRenderer.invoke('widget:play-result', videoId),
  setSearchOpen: (open) => ipcRenderer.invoke('widget:search-open', open),
  onSearchCollapsed: (handler) => {
    const listener = () => handler();
    ipcRenderer.on('search-collapsed', listener);
    return () => ipcRenderer.off('search-collapsed', listener);
  },
  onZoom: (handler) => {
    const listener = (_event, zoom) => handler(zoom);
    ipcRenderer.on('zoom', listener);
    return () => ipcRenderer.off('zoom', listener);
  },
  retry: () => ipcRenderer.invoke('widget:retry'),
  quit: () => ipcRenderer.invoke('widget:quit'),
});
