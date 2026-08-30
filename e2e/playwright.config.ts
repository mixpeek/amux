// Playwright golden-scenario harness (RR-0025, Invariants 44/45/46).
//
// Boots the RUST server against a throwaway AMUX_HOME so every run starts
// from a deterministic state — the Python gate-contract suite was green for
// weeks on ambient machine state and red the moment CI ran it clean; this
// harness builds its own world instead.
import { defineConfig, devices } from '@playwright/test';
import * as path from 'path';
import * as fs from 'fs';
import * as os from 'os';

// ONE SERVER AND ONE AMUX_HOME PER PROJECT — not one shared by all of them
// (AF-46, measured 2026-08-13).
//
// Projects multiply every spec file: with three of them, settings.spec.ts runs
// three times CONCURRENTLY. That file POSTs global server state (/api/prefs,
// /api/settings/*) and then asserts the UI reflects the value IT just wrote, so
// against a shared store the three copies overwrite each other and whichever
// loses reports a product bug. Under the shared-server config the failures were
// real, repeatable in aggregate and different every run (7 tests one run, 3 the
// next); run alone against the SAME WebKit target the same file passed 27/27.
//
// The tempting reading is "WebKit is flaky" and the tempting fix is a retry or
// a skip — which would have bought green by deleting the signal. The defect is
// that the harness had no isolation boundary between projects at all, and it
// would have come back the moment anyone added a fourth project or a fourth
// state-mutating spec. A port and a home each is the boundary; adding a target
// below now costs nothing and races nothing.
const TARGETS = [
  { name: 'desktop', port: 18823, use: { viewport: { width: 1280, height: 800 } } },
  // Mobile is a first-class target (amux is mobile-first): 375px must
  // render without overflow.
  { name: 'mobile', port: 18833, use: { viewport: { width: 375, height: 667 } } },
  // iOS SAFARI IS ITS OWN TARGET, not a viewport (AMUX tab-customizer, 2026-08-13).
  // A bottom-sheet fix passed desktop+mobile Chromium at 375px while Ethan still
  // could not see the menu on his phone — Chromium at a phone WIDTH is not Safari
  // on a phone. WebKit differs on exactly what this UI depends on: position:fixed
  // inside a transformed ancestor, env(safe-area-inset-*), and dvh/vh behaviour.
  //
  // It runs the WHOLE suite. It briefly did not: a `testMatch` narrowed it to
  // four layout specs because the cross-project races above were misread as
  // iOS-specific reds. That scoping was honest about what it covered and still
  // wrong in effect — a new mobile spec would silently never reach iOS, which
  // is the one target that motivated the project. Isolation removed the reason
  // for it, so the list is gone; do not reintroduce one to quiet a red.
  { name: 'ios-safari', port: 18843, use: { ...devices['iPhone 15'] } },
];

// One temp home per TARGET, created eagerly so each server and its tests agree.
const homes: Record<string, string> = Object.fromEntries(
  TARGETS.map((t) => [t.name, fs.mkdtempSync(path.join(os.tmpdir(), `amux-e2e-${t.name}-`))]),
);

// SAY OUT LOUD WHAT EACH TARGET ACTUALLY COVERS, every run.
//
// The failure this exists to catch is not a red test — it is a GREEN one that
// covered less than its name promised. ios-safari carried a `testMatch` listing
// four spec files, so "ios-safari ✓" meant four files while reading, to anyone
// scanning CI, like iOS was covered. Nothing in the output distinguished those
// two states, and the narrowing was invisible in exactly the place people look.
//
// So a scoped target now announces itself on every run and names the count. A
// future narrowing cannot be silent, which is the property that was missing —
// not a rule telling the next person to remember (CLAUDE.md: a fix owes a
// signal, or the next instance is just as invisible as this one was).
// WHICH TARGETS THIS INVOCATION ACTUALLY RUNS (AMUX-3605).
//
// The loop below described the CONFIG, not the RUN, so `--project=desktop`
// printed "target ios-safari: full suite" while running zero iOS tests. That is
// the same defect this banner was written to catch, one level up: a line that
// reads as coverage on a run that had none. Measured on this file: a
// `--project=ios-safari` run announced all three targets.
//
// Read off process.argv because Playwright applies --project AFTER evaluating
// this file, so the config object cannot know. Both spellings, since
// `--project=x` and `--project x` are equally common.
const selected: string[] = (() => {
  const out: string[] = [];
  const argv = process.argv;
  for (let i = 0; i < argv.length; i++) {
    if (argv[i].startsWith('--project=')) out.push(argv[i].slice('--project='.length));
    else if (argv[i] === '--project' && argv[i + 1]) out.push(argv[i + 1]);
  }
  return out;
})();

for (const t of TARGETS) {
  const scoped = (t as { testMatch?: unknown }).testMatch;
  const running = selected.length === 0 || selected.includes(t.name);
  const what = scoped
    ? `SCOPED to ${String(scoped)} — it does NOT run the full suite`
    : 'full suite';
  console.log(
    running
      ? `[e2e] target ${t.name}: ${what}.`
      : `[e2e] target ${t.name}: NOT RUN by this invocation (--project=${selected.join(',')}) — ${what} when it does run.`,
  );
}
if (selected.length && !selected.includes('ios-safari')) {
  // Named explicitly because ios-safari is the target that exists to catch what
  // the other two structurally cannot: a bottom-sheet fix passed desktop+mobile
  // Chromium at 375px while the menu was still invisible on a real phone.
  // Omitting it is legitimate for a quick loop and must not be silent.
  console.log(
    '[e2e] NOTE: ios-safari is omitted from this run. It is the only WebKit target, and ' +
      'Chromium at a phone width is not Safari on a phone — a green run here says nothing ' +
      'about position:fixed in a transformed ancestor, env(safe-area-inset-*), or dvh.',
  );
}

// WHAT THE MACHINE WAS DOING, on every run (AMUX-3605 + AMUX-3646).
//
// AMUX-3605 reported `browserType.launch: Timeout 180000ms exceeded` for
// ios-safari on every dev-machine run. It does not reproduce: WebKit launches
// here in 451ms, and playwright 1.62.1 wants webkit-2336 which is installed, so
// the card's own revision hypothesis is dead. The cause is unknown and the
// reason it is unknown is that a launch timeout records nothing about the host.
//
// This box compiles and tests amux continuously — 28 cores routinely carrying a
// load average in the thirties (AMUX-3646) — and a 180s launch budget is not
// obviously safe under that. Stated as a candidate, not a conclusion: printing
// the number every run is what makes the NEXT report decidable instead of
// another round of hypotheses.
console.log(
  `[e2e] host: load ${os.loadavg().map((n) => n.toFixed(2)).join(' / ')} on ${os.cpus().length} cores. ` +
    'If a browser launch times out, this line is the first thing to check.',
);

export default defineConfig({
  testDir: '.',
  timeout: 30_000,
  retries: 0,
  use: {
    ignoreHTTPSErrors: true, // self-signed cert is the product behavior
    // SERVICE WORKERS OFF BY DEFAULT — opt-OUT, not opt-in (AMUX-3057 class).
    //
    // sw.js reloads the page on `controllerchange` the moment a freshly
    // registered worker claims the client, which on the clean profile each
    // project gets lands mid-`page.evaluate` and throws "Execution context was
    // destroyed, most likely because of a navigation". It also swallows
    // page.route stubs, since a request passing through the worker's fetch
    // handler is invisible to them.
    //
    // This was fixed three times per-spec (tab-customizer, system-jobs,
    // peek-tab-menu-anchor) and then omitted from the next three specs I wrote,
    // which is how it reached CI. A per-file convention that must be REMEMBERED
    // is the thing that failed; the default has to carry it. sw-fail-bar.spec.ts
    // — the one spec that actually tests the worker — opts back in with
    // test.use({ serviceWorkers: 'allow' }).
    serviceWorkers: 'block',
  },
  projects: TARGETS.map((t) => ({
    name: t.name,
    use: { ...t.use, baseURL: `https://localhost:${t.port}` },
  })),
  webServer: TARGETS.map((t) => ({
    // Builds from COMMITTED HEAD, not this shared working tree — a peer
    // mid-edit used to fail runs with a Rust error against JS-only changes,
    // and a red run caused by a stranger's half-saved file is
    // indistinguishable from one caused by your own patch (AMUX-2924).
    // Uncommitted rust changes are announced by name every run;
    // AMUX_E2E_WORKING_TREE=1 opts back in to testing the tree.
    command: `bash ${path.join(__dirname, 'serve-head.sh')}`,
    url: `https://localhost:${t.port}/health`,
    ignoreHTTPSErrors: true,
    reuseExistingServer: false,
    // PIPE, not the default 'ignore'. serve-head.sh prints which uncommitted
    // rust changes are NOT under test, and that notice is the only thing
    // standing between "builds from HEAD" and a developer silently testing
    // code they did not write. A warning nobody can see is not a warning —
    // the default would have swallowed it whole (AMUX-2924).
    stdout: 'pipe',
    stderr: 'pipe',
    // 10 min, not 3: the HEAD-worktree build now uses its OWN target dir
    // (AMUX-2961 — sharing the fleet's dir let worktree dep-info poison repo
    // builds into silent no-ops), so its FIRST local build is fully cold.
    // Later runs are cached; CI skips the worktree entirely via
    // AMUX_E2E_WORKING_TREE=1.
    timeout: 600_000,
    env: {
      AMUX_HOME: homes[t.name],
      AMUX_RS_PORT: String(t.port),
      // A browser test necessarily connects over loopback, which the server
      // deliberately auth-bypasses (Python parity). Without this, every
      // "rejects a bad token" assertion is a check that cannot pass — it was
      // read as an auth regression on 2026-08-09 when the code was correct.
      AMUX_RS_NO_LOOPBACK_BYPASS: '1',
    },
  })),
});
