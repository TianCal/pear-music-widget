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
  setSkin: (skin) => ipcRenderer.invoke('widget:skin', skin),
  setPanelSkin: (skin) => ipcRenderer.invoke('widget:panel-skin', skin),
  openApp: () => ipcRenderer.invoke('widget:open-app'),
  search: (query) => ipcRenderer.invoke('widget:search', query),
  playResult: (videoId) => ipcRenderer.invoke('widget:play-result', videoId),
  setPanel: (which) => ipcRenderer.invoke('widget:panel', which),
  contextMenu: () => ipcRenderer.send('widget:context-menu'),
  onOpenSearch: (handler) => {
    const listener = () => handler();
    ipcRenderer.on('open-search', listener);
    return () => ipcRenderer.off('open-search', listener);
  },
  onPanelCollapsed: (handler) => {
    const listener = () => handler();
    ipcRenderer.on('panel-collapsed', listener);
    return () => ipcRenderer.off('panel-collapsed', listener);
  },
  onZoom: (handler) => {
    const listener = (_event, zoom) => handler(zoom);
    ipcRenderer.on('zoom', listener);
    return () => ipcRenderer.off('zoom', listener);
  },
  retry: () => ipcRenderer.invoke('widget:retry'),
  quit: () => ipcRenderer.invoke('widget:quit'),
});
