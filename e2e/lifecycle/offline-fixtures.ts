import { createServer } from 'node:http';
import { request as httpsRequest } from 'node:https';
import { test as base } from '../fixtures';
export { expect } from '../fixtures';

// Browsers reject service-worker scripts served with the scratch server's
// self-signed TLS certificate, even with ignoreHTTPSErrors. Loopback HTTP is a
// browser-defined secure context: proxy the *real* private server and its shell
// without changing production TLS, installing a trusted CA, or stubbing APIs.
type OfflineTransport = {origin:string; setConnected:(connected:boolean) => void};
export const test = base.extend<{}, { offlineTransport: OfflineTransport }>({
  offlineTransport: [async ({}, use, info) => {
    const upstream = new URL(String(info.project.use.baseURL));
    if (upstream.protocol !== 'https:' || upstream.hostname !== 'localhost') {
      throw new Error('Offline fixture only proxies the lifecycle HTTPS localhost server');
    }
    let connected = true;
    const active = new Set<() => void>();
    const server = createServer((req, res) => {
      if (!connected) { res.destroy(); return; }
      const target = new URL(req.url || '/', upstream);
      if (target.origin !== upstream.origin) { res.writeHead(400).end(); return; }
      const forward = httpsRequest(target, { method: req.method,
        headers: { ...req.headers, host: upstream.host }, rejectUnauthorized: false }, response => {
        res.writeHead(response.statusCode || 502, response.headers);
        response.pipe(res);
      });
      forward.on('error', error => {
        if (!res.headersSent) res.writeHead(502, { 'Content-Type': 'text/plain' });
        res.end('Scratch upstream unavailable: ' + error.message);
      });
      const stop = () => {forward.destroy(); res.destroy();};
      active.add(stop);
      res.on('close', () => {active.delete(stop); forward.destroy();});
      req.pipe(forward);
    });
    await new Promise<void>((resolve, reject) => {
      server.once('error', reject);
      server.listen(0, '127.0.0.1', resolve);
    });
    const address = server.address();
    if (!address || typeof address === 'string') throw new Error('No loopback listener');
    try { await use({origin:`http://127.0.0.1:${address.port}`, setConnected(value) {connected=value; if (!value) for (const stop of active) stop();}}); }
    finally {
      server.closeAllConnections();
      await new Promise<void>(resolve => server.close(() => resolve()));
    }
  }, { scope: 'worker' }],
  baseURL: async ({ offlineTransport }, use) => use(offlineTransport.origin),
});
