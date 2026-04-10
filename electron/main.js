const { app, BrowserWindow, shell, session } = require('electron');
const path = require('path');

const AMUX_URL = process.env.AMUX_URL || 'https://localhost:8822';

function createWindow() {
  const win = new BrowserWindow({
    width: 1440,
    height: 900,
    minWidth: 800,
    minHeight: 500,
    titleBarStyle: 'hiddenInset',
    backgroundColor: '#0d1117',
    webPreferences: {
      nodeIntegration: false,
      contextIsolation: true,
    },
    show: false,
  });

  // Open external links in default browser
  win.webContents.setWindowOpenHandler(({ url }) => {
    if (!url.startsWith(AMUX_URL)) {
      shell.openExternal(url);
      return { action: 'deny' };
    }
    return { action: 'allow' };
  });

  win.webContents.on('will-navigate', (event, url) => {
    const parsed = new URL(url);
    if (parsed.hostname !== 'localhost' && parsed.hostname !== '127.0.0.1') {
      event.preventDefault();
      shell.openExternal(url);
    }
  });

  // Show window once content is ready (avoids white flash)
  win.once('ready-to-show', () => win.show());

  loadWithRetry(win, 0);
  return win;
}

function loadWithRetry(win, attempts) {
  win.loadURL(AMUX_URL).catch(() => {
    if (attempts < 30) {
      setTimeout(() => loadWithRetry(win, attempts + 1), 1000);
    }
  });
}

app.whenReady().then(() => {
  // Trust self-signed certificates for localhost
  session.defaultSession.setCertificateVerifyProc((request, callback) => {
    if (request.hostname === 'localhost' || request.hostname === '127.0.0.1') {
      callback(0); // trust
    } else {
      callback(-3); // use default verification
    }
  });

  createWindow();

  app.on('activate', () => {
    if (BrowserWindow.getAllWindows().length === 0) createWindow();
  });
});

app.on('window-all-closed', () => {
  if (process.platform !== 'darwin') app.quit();
});
