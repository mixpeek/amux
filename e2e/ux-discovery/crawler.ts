// UX discovery harness core (RR-0028a/RR-0028b, Invariant 46).
//
// Starts from a page and automatically explores every reachable user-facing
// surface, producing a UiState/UiAction/UiEdge graph. The graph IS the
// browser acceptance test plan: a control the crawler cannot discover is a
// control no test covers, and CI fails on interactive controls that lack a
// semantic test identity.
//
// Semantic state hashing: timestamps, ULIDs, animation classes, and counters
// are normalized away so "the same screen five seconds later" is the same
// state, while a genuinely new dialog or tab is a new one.

import type { Page } from '@playwright/test';
import * as crypto from 'crypto';

export interface UiAction {
  // Stable identity for the control: data-testid preferred, else role+name,
  // else a structural selector (flagged — structural selectors are the
  // "missing semantic identifier" CI failure).
  id: string;
  kind: 'click' | 'fill' | 'select' | 'hover' | 'keyboard';
  selector: string;
  hasSemanticId: boolean;
  label: string;
}

export interface UiState {
  hash: string;
  url: string;
  title: string;
  actions: UiAction[];
  discoveredAt: number; // BFS depth, not wall time — determinism
}

export interface UiEdge {
  from: string; // state hash
  action: string; // action id
  to: string; // state hash
}

export interface InteractionGraph {
  states: UiState[];
  edges: UiEdge[];
  missingSemanticIds: UiAction[];
}

// Everything that can receive an interaction (RR-0028b's canonical list).
const INTERACTIVE_SELECTOR = [
  'button',
  'a[href]',
  'input',
  'textarea',
  'select',
  '[role=button]',
  '[role=menuitem]',
  '[role=tab]',
  '[role=checkbox]',
  '[role=switch]',
  '[role=radio]',
  '[contenteditable]',
  '[tabindex]:not([tabindex="-1"])',
  '[data-action]',
  '[data-testid]',
  '[draggable=true]',
].join(', ');

/// Normalize DOM-derived text so volatile content does not fork states.
export function normalizeForHash(s: string): string {
  return s
    // ULIDs / long ids
    .replace(/[0-7][0-9A-HJKMNP-TV-Z]{25}/g, '<ULID>')
    .replace(/\b[a-f0-9]{8,}\b/g, '<HEX>')
    // ISO timestamps, clock times, relative times
    .replace(/\d{4}-\d{2}-\d{2}[T ]\d{2}:\d{2}(:\d{2})?(\.\d+)?(Z|[+-]\d{2}:?\d{2})?/g, '<TS>')
    .replace(/\b\d{1,2}:\d{2}(:\d{2})?\s*(am|pm)?\b/gi, '<TIME>')
    .replace(/\b\d+\s*(seconds?|minutes?|hours?|days?)\s+ago\b/gi, '<AGO>')
    // Bare counters ("3 active", "12 items")
    .replace(/\b\d+\b/g, '<N>')
    .replace(/\s+/g, ' ')
    .trim();
}

export async function captureState(page: Page, depth: number): Promise<UiState> {
  // Wait for the page to settle enough to be inventoried; animations are
  // normalized away by the hasher, not awaited.
  await page.waitForLoadState('domcontentloaded');

  const raw = await page.evaluate((selector) => {
    const controls = Array.from(document.querySelectorAll(selector)).map((el) => {
      const e = el as HTMLElement;
      const testid = e.getAttribute('data-testid') || '';
      const role = e.getAttribute('role') || e.tagName.toLowerCase();
      const label =
        e.getAttribute('aria-label') ||
        (e.textContent || '').trim().slice(0, 60) ||
        e.getAttribute('placeholder') ||
        '';
      // A structural path used only when no semantic identity exists.
      let path = e.tagName.toLowerCase();
      let cur: Element | null = e;
      let hops = 0;
      while (cur && cur.parentElement && hops < 4) {
        const idx = Array.from(cur.parentElement.children).indexOf(cur);
        path = `${cur.parentElement.tagName.toLowerCase()}>${idx}>${path}`;
        cur = cur.parentElement;
        hops++;
      }
      const visible = !!(e.offsetWidth || e.offsetHeight || e.getClientRects().length);
      return { testid, role, label, path, tag: e.tagName.toLowerCase(), visible };
    });
    return {
      title: document.title,
      bodyText: document.body ? document.body.innerText.slice(0, 5000) : '',
      controls,
    };
  }, INTERACTIVE_SELECTOR);

  const actions: UiAction[] = raw.controls
    .filter((c) => c.visible)
    .map((c) => {
      const hasSemanticId = !!c.testid;
      const id = c.testid || (c.label ? `${c.role}:${c.label}` : `structural:${c.path}`);
      const selector = c.testid
        ? `[data-testid="${c.testid}"]`
        : c.label
          ? `${c.tag}:has-text("${c.label.replace(/"/g, '\\"')}")`
          : c.path;
      const kind: UiAction['kind'] =
        c.tag === 'input' || c.tag === 'textarea' || c.role === 'contenteditable'
          ? 'fill'
          : c.tag === 'select'
            ? 'select'
            : 'click';
      return { id, kind, selector, hasSemanticId, label: c.label };
    });

  const hashInput = normalizeForHash(
    `${new URL(page.url()).pathname}|${raw.title}|${raw.bodyText}|${actions
      .map((a) => a.id)
      .sort()
      .join(',')}`,
  );
  const hash = crypto.createHash('sha256').update(hashInput).digest('hex').slice(0, 16);

  return { hash, url: page.url(), title: raw.title, actions, discoveredAt: depth };
}

export interface CrawlOptions {
  maxDepth: number;
  maxStates: number;
  // Actions matching any of these action-id patterns are recorded but not
  // executed (destructive controls in a shared fixture).
  skipActions?: RegExp[];
}

/// BFS exploration with semantic deduplication. Executes click actions only
/// (fill/select/hover exploration layers on in RR-0028b's later slices);
/// every discovered state and edge lands in the graph either way.
export async function crawl(
  page: Page,
  startPath: string,
  opts: CrawlOptions,
): Promise<InteractionGraph> {
  const states = new Map<string, UiState>();
  const edges: UiEdge[] = [];
  const queue: { hash: string; depth: number }[] = [];

  await page.goto(startPath);
  const start = await captureState(page, 0);
  states.set(start.hash, start);
  queue.push({ hash: start.hash, depth: 0 });

  while (queue.length > 0 && states.size < opts.maxStates) {
    const { hash, depth } = queue.shift()!;
    if (depth >= opts.maxDepth) continue;
    const state = states.get(hash)!;

    for (const action of state.actions) {
      if (action.kind !== 'click') continue;
      if (opts.skipActions?.some((re) => re.test(action.id))) continue;
      // Re-establish the source state: navigate to its URL. (True replay of
      // action PATHS arrives with seed-state fixtures in RR-0028c; URL-level
      // reset is the Phase 0 core.)
      try {
        await page.goto(state.url);
        const el = page.locator(action.selector).first();
        if ((await el.count()) === 0) continue;
        // A control hidden at this viewport cannot be clicked, so its click
        // only waited out the 2s timeout. On the 375px fixture that was most
        // of 29s of a 30s test, and CI load pushed it over (2026-11-08).
        if (!(await el.isVisible())) continue;
        await el.click({ timeout: 2000, noWaitAfter: false });
      } catch {
        continue; // control vanished or not clickable from here — recorded, unreachable
      }
      const next = await captureState(page, depth + 1);
      if (!states.has(next.hash)) {
        states.set(next.hash, next);
        queue.push({ hash: next.hash, depth: depth + 1 });
      }
      edges.push({ from: state.hash, action: action.id, to: next.hash });
    }
  }

  const missingSemanticIds = Array.from(states.values())
    .flatMap((s) => s.actions)
    .filter((a) => !a.hasSemanticId)
    // Dedup by id
    .filter((a, i, arr) => arr.findIndex((b) => b.id === a.id) === i);

  return { states: Array.from(states.values()), edges, missingSemanticIds };
}
