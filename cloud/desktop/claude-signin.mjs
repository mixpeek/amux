// node claude-signin.mjs <cdp-port> <authorize-url> <email>
// Drive a HEADED browser (claude.ai turns headless Chrome away) through one Claude sign-in for a
// remote `claude auth login`, and write the OAuth code it ends on to the file in env CODE_FILE.
// The code is never printed.
//
// The emailed sign-in link is read through this machine's amux email API when the account's
// inbox is connected (GET /api/email/search). Otherwise the script waits while a person finishes
// the sign-in in the visible window, and picks the code up from the callback page.
// Exit codes: 1 login page never loaded, 4 never reached the callback, 5 no code on it.
const [port, url, email] = process.argv.slice(2);
const fs = await import('fs'); const cp = await import('child_process');
const wait = ms => new Promise(r => setTimeout(r, ms));
const until = async (f, ms) => { for (let t = 0; t < ms; t += 1000) { const v = await f(); if (v) return v; await wait(1000); } return null; };
const pages = async () => (await (await fetch(`http://127.0.0.1:${port}/json`)).json()).filter(t => t.type === 'page');
const connect = async t => {
  const ws = new WebSocket(t.webSocketDebuggerUrl); await new Promise(r => ws.onopen = r);
  let id = 0; const pend = {};
  ws.onmessage = e => { const m = JSON.parse(e.data); if (m.id && pend[m.id]) { pend[m.id](m); delete pend[m.id]; } };
  const send = (method, params = {}) => new Promise(r => { const i = ++id; pend[i] = r; ws.send(JSON.stringify({ id: i, method, params })); });
  const ev = async x => (await send('Runtime.evaluate', { expression: x, returnByValue: true, awaitPromise: true })).result?.result?.value;
  const click = async expr => {
    const xy = await ev(`(()=>{const e=(${expr}); if(!e) return null; e.scrollIntoView({block:'center'}); const b=e.getBoundingClientRect(); return [b.x+b.width/2,b.y+b.height/2];})()`);
    if (!xy) return false;
    for (const type of ['mousePressed', 'mouseReleased']) await send('Input.dispatchMouseEvent', { type, x: xy[0], y: xy[1], button: 'left', clickCount: 1 });
    return true;
  };
  return { send, ev, click, type: text => send('Input.insertText', { text }) };
};
let amuxBase = '';
try { amuxBase = cp.execFileSync('amux', ['url'], { stdio: ['ignore', 'pipe', 'ignore'] }).toString().trim(); } catch {}
const amux = path => {
  if (!amuxBase) return null;
  try { return JSON.parse(cp.execFileSync('curl', ['-sk', '--max-time', '20', '-H', `X-Amux-Session: ${process.env.AMUX_SESSION || ''}`, amuxBase + path]).toString()); } catch { return null; }
};

// 1. A fresh claude.ai session (another account's cookies would sign in as that account), then the URL.
const L = await connect((await pages()).find(t => /claude|platform/.test(t.url)) || (await pages())[0]);
const { result } = await L.send('Network.getAllCookies');
for (const c of result.cookies || []) if (/claude\.(ai|com)$/.test(c.domain.replace(/^\./, ''))) await L.send('Network.deleteCookies', { name: c.name, domain: c.domain, path: c.path });
await L.send('Page.navigate', { url });
if (!(await until(() => L.ev(`location.host==='claude.ai' && !!document.querySelector('input[type=email]')`), 60000))) {
  console.log('login page did not load'); process.exit(1);
}

// 2. Ask for the sign-in email. A synthetic click on the button does not submit this form.
const started = Date.now() / 1000;
await L.click(`document.querySelector('input[type=email]')`);
await L.ev(`(()=>{document.querySelector('input[type=email]').select(); document.execCommand('delete'); return 1})()`);
await L.type(email);
await L.ev(`(()=>{const i=document.querySelector('input[type=email]'); const b=[...document.querySelectorAll('button')].find(b=>/^Continue with email$/.test(b.innerText.trim())); i.form.requestSubmit(b); return 1})()`);
console.log('requested sign-in email for', email);

// 3. Read the link from the inbox, open it to get the 6-digit code, and enter the code.
const link = await until(() => {
  for (const m of amux('/api/email/search?q=' + encodeURIComponent(`in:anywhere to:${email} secure link to Claude`) + '&days=1&limit=10') || []) {
    const sent = Date.parse(m.date_iso || m.received || m.date || '') / 1000;
    if ((m.to || '').toLowerCase() !== email.toLowerCase() || !(sent >= started - 30)) continue;
    const found = JSON.stringify(amux('/api/email/message/' + encodeURIComponent(m.message_id)) || {}).match(/https:\/\/claude\.ai\/magic-link#[A-Za-z0-9_\-:.]+/);
    if (found) return found[0];
  }
  return null;
}, amuxBase ? 120000 : 0);
if (link) {
  const nt = await (await fetch(`http://127.0.0.1:${port}/json/new?${encodeURIComponent(link)}`, { method: 'PUT' })).json();
  const H = await connect(nt);
  const code6 = await until(async () => { const v = await H.ev('(document.body.innerText.match(/\\b\\d{6}\\b/) || [""])[0]'); return /^\d{6}$/.test(v || '') ? v : null; }, 30000);
  await fetch(`http://127.0.0.1:${port}/json/close/${nt.id}`);
  const field = `document.querySelector('input[autocomplete=one-time-code]')`;
  if (code6) {
    if (!(await L.ev(`!!${field}`))) await L.click(`[...document.querySelectorAll('button, a')].find(b=>/Enter verification code/i.test(b.innerText))`);
    if (await until(() => L.ev(`!!${field}`), 15000)) {
      await L.click(field); await L.type(code6); await wait(500);
      await L.ev(`(()=>{const i=${field}; const b=[...document.querySelectorAll('button')].find(b=>/^(Verify|Continue)/i.test(b.innerText.trim())); if (i.form && b) i.form.requestSubmit(b); else if (b) b.click(); return 1})()`);
      console.log('entered the emailed code');
      await wait(4000);   // Authorize is not clickable until the session settles
    }
  }
} else {
  console.log(`no sign-in email readable here: finish signing in as ${email} in the browser window (10 min)`);
}

// 4. Authorize, then read the code off the callback page.
const cb = await until(async () => {
  if (/oauth\/code/.test(await L.ev('location.href') || '')) return true;
  await L.click(`[...document.querySelectorAll('button')].find(b=>/^(Authorize|Allow|Approve)$/i.test(b.innerText.trim()))`);
  return null;
}, link ? 60000 : 600000);
if (!cb) { console.log('stuck at', await L.ev('location.host+location.pathname')); process.exit(4); }
const code = await until(async () => { const t = await L.ev(`(document.querySelector('code, pre')||{}).innerText || (document.querySelector('input[readonly]')||{}).value || ''`); return t && t.length > 20 ? t.trim() : null; }, 20000)
  || (await L.ev('document.body.innerText')).match(/[A-Za-z0-9_\-]{30,}#[A-Za-z0-9_\-]{10,}/)?.[0];
if (!code) { console.log('no code on the callback page'); process.exit(5); }
fs.writeFileSync(process.env.CODE_FILE, code, { mode: 0o600 });
console.log('got the OAuth code');
process.exit(0);
