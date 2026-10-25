// Replays Ethan's real browser requests (MSG-90417/90429) against the LIVE
// profile chooser and the route ladder, read-only: nothing is opened or signed
// in. Each case is a message he sent, the site it needs, and the identity or
// role a worker should pass (`amux browser for <url> --identity/--role`).
//
//   node e2e/browser-command-replay.mjs            # against $(amux url)
//   AMUX_URL=https://127.0.0.1:8824 node e2e/browser-command-replay.mjs
//
// Prints one line per case and exits with the number of failures. An
// `unsynced` outcome is a pass only when the refusal names the profile the
// owner must sync, because a worker cannot fix that and must not substitute.
import {execFileSync} from 'node:child_process';
import https from 'node:https';

const base = process.env.AMUX_URL || execFileSync('amux', ['url'], {encoding: 'utf8'}).trim();
const session = process.env.AMUX_SESSION || 'browser-replay';

const CASES = [
  {msg: 'MSG-89745', said: 'use my esteininger21@gmail.com google account create a notebookLM ...',
   url: 'https://notebooklm.google.com', identity: 'esteininger21@gmail.com', want: 'ethan-personal'},
  {msg: 'MSG-89670', said: 'terminate my digital ocean account it should be esteininger21@gmail.com',
   url: 'https://cloud.digitalocean.com', identity: 'esteininger21@gmail.com', want: 'ethan-personal'},
  {msg: 'MSG-89482', said: 'fix rb2b if you need to you have full access to browser + inbox',
   url: 'https://app.rb2b.com', want: 'ethan'},
  {msg: 'MSG-90364', said: 'you create the accounts and submit (ethan@mixpeek.com ...)',
   url: 'https://studio.mixpeek.com', identity: 'ethan@mixpeek.com', want: 'ethan'},
  {msg: 'MSG-89939', said: 'open platform.openai.com > Plugins > Mixpeek > Review information',
   url: 'https://platform.openai.com', want: 'ethan'},
  {msg: 'MSG-90711', said: 'compare against the tubescience flawless search app',
   url: 'https://ts.app/flawless', want: 'ethan-tubescience'},
  {msg: 'MSG-90659', said: 'order the RoboForce mailer (FedEx Office ...)',
   url: 'https://www.fedex.com', identity: 'ethan@mixpeek.com', role: 'restricted', want: 'ethan-money'},
  {msg: 'MSG-90443', said: 'most effective linkedin growth magnets for our ICP',
   url: 'https://www.linkedin.com', want: 'ethan'},
  {msg: 'MSG-90536', said: 'take whatever is valuable from here https://x.com/alexgroberman/...',
   url: 'https://x.com/alexgroberman/status/2111484443726852385', want: 'ethan'},
];

function get(path) {
  return new Promise((resolve, reject) => {
    const req = https.request(new URL(path, base), {rejectUnauthorized: false, headers: {'X-Amux-Session': session}}, res => {
      let t = ''; res.on('data', d => { t += d; }); res.on('end', () => { try { resolve({status: res.statusCode, body: JSON.parse(t)}); } catch (e) { reject(e); } });
    });
    req.setTimeout(20000, () => req.destroy(new Error('timeout'))); req.on('error', reject); req.end();
  });
}

let failures = 0;
const ladder = (await get('/api/browser/routing/config')).body;
const order = (ladder.ladder || []).join(' > ');
const ladderOk = ladder.measured === true && ladder.ladder?.[0] === 'amux';
console.log(`${ladderOk ? 'PASS' : 'FAIL'} ladder: ${order} (MSG-90394: the amux profile browser comes before a CDP copy of Chrome)`);
if (!ladderOk) failures++;

for (const c of CASES) {
  const q = new URLSearchParams({url: c.url, ...(c.identity && {identity: c.identity}), ...(c.role && {role: c.role})});
  const {status, body} = await get(`/api/browser/profile-for?${q}`);
  let verdict, detail;
  if (body.profile) {
    verdict = body.profile === c.want ? 'PASS' : 'FAIL';
    detail = `${body.profile} signed_in=${body.signed_in}`;
  } else if ((body.unsynced || []).includes(c.want)) {
    verdict = 'PASS';
    detail = `unsynced: ${c.want} has no browser data here; owner must sync it (refusal names it)`;
  } else {
    verdict = 'FAIL';
    detail = `${status} ${body.error || ''}`;
  }
  if (verdict === 'FAIL') failures++;
  console.log(`${verdict} ${c.msg} ${c.url}${c.identity ? ' --identity ' + c.identity : ''}${c.role ? ' --role ' + c.role : ''} -> ${detail} (want ${c.want})`);
}
console.log(failures ? `${failures} FAILED of ${CASES.length + 1}` : `all ${CASES.length + 1} passed`);
process.exitCode = failures;
