#!/usr/bin/env node
// The helper model for the intent e2e server (AMUX_HELPER_CLI). A CLI not
// named `claude` is called as `<cli> --print <prompt> --model <m>`
// (crates/amux-server/src/api/lookup.rs). It answers the three prompts the
// intent features send (route, mail searches, ideas pass) from the seeded
// household, deterministically, so the specs test amux and not a model. The
// persona runs use the real model.
const args = process.argv.slice(2);
const i = args.indexOf('--print');
const prompt = i >= 0 ? args[i + 1] || '' : args.join(' ');
const out = (v) => { process.stdout.write(typeof v === 'string' ? v : JSON.stringify(v)); process.exit(0); };

const section = (name) => {
  const a = prompt.indexOf(name + ':');
  if (a < 0) return '';
  const rest = prompt.slice(a + name.length + 1);
  const b = rest.search(/\n[A-Z][A-Z ()]+:\n/);
  return b < 0 ? rest : rest.slice(0, b);
};

function lane(text) {
  const t = text.toLowerCase();
  if (/\bbus\b/.test(t)) return 'misroute';
  if (/dinner|meal|recipe|grocer|cook/.test(t)) return 'meals';
  if (/rental|tova|\$\d|payment|reconcil|enrollment|benefit|fsa/.test(t)) return 'money';
  if (/capital of|what is \d|how many days/.test(t)) return 'answer';
  return 'family';
}
const HELPERS = {
  family: { name: 'family-helper', desc: "Handles the family's school, camp, calendar and errands" },
  meals: { name: 'meal-planner', desc: 'Plans the week of dinners around the family' },
  money: { name: 'family-money', desc: 'Keeps track of family payments, benefits and paperwork' },
};
const titleOf = (t) => {
  const s = t.replace(/^keep this goal for me:\s*/i, '').split(/[.!?\n]/)[0].replace(/^(please|can you|could you)\s+/i, '');
  return s.split(/\s+/).slice(0, 7).join(' ');
};

if (prompt.startsWith('You route a person')) {
  const req = section('REQUEST').trim();
  const workers = section('WORKERS');
  const has = (n) => new RegExp('^- ' + n + ' \\[', 'm').test(workers);
  const l = lane(req);
  if (l === 'answer') out({ action: 'answer', answer: 'Paris is the capital of France.', title: titleOf(req), why: 'A quick question.', candidates: [] });
  const target = l === 'misroute' ? (has('meal-planner') ? 'meal-planner' : 'family-helper') : HELPERS[l].name;
  const spec = HELPERS[l === 'misroute' ? 'family' : l];
  if (has(target)) out({ action: 'send', worker: target, title: titleOf(req), why: `${target} already handles this.`, candidates: [{ worker: target, why: 'closest fit' }] });
  out({ action: 'create', title: titleOf(req), why: 'Nobody handles this yet, so a new helper takes it.',
    candidates: [], create: { name: spec.name, dir: '', groups: [], desc: spec.desc, worker_type: 'chat', model: 'sonnet' } });
}

if (prompt.includes('write one short email search')) {
  out(['camp kaylie rafi', 'hudson prep ezra', 'conference lincoln', 'hertz tova']);
}

if (prompt.startsWith('You help a busy person')) {
  const ids = [...prompt.matchAll(/^- \[([^\]]+)\] (.*)$/gm)].map((m) => ({ id: m[1], text: m[2] }));
  const asked = ids.filter((x) => x.id.startsWith('o:'));
  const find = (re, kind) => ids.find((x) => (!kind || x.id.startsWith(kind)) && re.test(x.text));
  const askedAbout = (re) => asked.find((x) => re.test(x.text));
  const ideas = [], goals = [];
  const camp = askedAbout(/camp/i), campMail = find(/camp kaylie/i, 'e:');
  if (camp && campMail) ideas.push({ title: 'Draft a second nudge to Camp Kaylie', body: 'I can pull the Camp Kaylie waitlist thread with Marcy Feld and draft a second, friendly nudge about Rafi, ready for your approval.',
    request: 'Draft a second nudge to Marcy Feld at Camp Kaylie about Rafi\'s Session 2 waitlist spot, from the earlier email thread. Keep it a draft for me to approve.', why: 'Your last note got no answer in 12 days.', sources: [camp.id, campMail.id] });
  const app = askedAbout(/application/i), appMail = find(/hudson prep/i, 'e:');
  if (app && appMail) ideas.push({ title: "Check whether Ezra's application is complete", body: 'I can check your email for the Hudson Prep confirmation and deadline messages and tell you if it is done or still missing something.',
    request: "Check my email for Hudson Prep's confirmation and deadline emails about Ezra's application and tell me if it is done or missing something.", why: 'The deadline is coming up.', sources: [app.id, appMail.id] });
  const school = askedAbout(/school|inbox/i), slip = find(/permission slip/i, 'e:');
  if (school && slip) goals.push({ title: 'Never miss a school form', tier: 'standing', due: null, request: 'Watch my email for school forms and deadlines and keep a list of what is due.',
    status_line: "3 forms open: Rafi's field trip slip is due Friday, picture day orders, and Ezra's emergency contact form.", why: 'School forms keep coming up.', sources: [school.id, slip.id] });
  const dinners = askedAbout(/dinner/i), csa = find(/csa/i, 'e:');
  if (dinners && csa) goals.push({ title: "Plan the week's dinners", tier: 'standing', due: null, request: 'Plan our dinners every week from the CSA box, with Noa vegetarian.',
    status_line: "This week's dinners are planned; next: Thursday's CSA pickup.", why: 'Dinners come up every week.', sources: [dinners.id, csa.id] });
  const cal = askedAbout(/calendar/i), nextEv = ids.find((x) => x.id.startsWith('c:') && !/vermont/i.test(x.text));
  if (cal && nextEv) goals.push({ title: 'Stay ahead of the family calendar', tier: 'standing', due: null, request: 'Keep me ahead of the family calendar: what is next and what it needs.',
    status_line: `Next: ${nextEv.text.split(' at ')[0]} (${nextEv.text.match(/\d{4}-\d{2}-\d{2}/)?.[0] || 'soon'}); what it needs is ready.`, why: 'You asked to stay ahead of it.', sources: [cal.id, nextEv.id] });
  const oe = find(/open enrollment/i, 'e:');
  const due = oe && (oe.text.match(/closes (\d{4}-\d{2}-\d{2})/) || [])[1];
  if (oe && due) goals.push({ title: 'Finish open enrollment', tier: 'tracking', due, request: 'Make sure open enrollment is done before it closes, including the FSA election.',
    status_line: 'Not started; the FSA election does not roll over.', why: 'It has a deadline.', sources: [oe.id] });
  out({ ideas, goals });
}

out('{}');
