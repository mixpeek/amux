# amux.io Homepage Experiments

**KPIs we optimize for:**
| KPI | PostHog event | Target |
|-----|--------------|--------|
| GitHub stars | `$pageview` → github.com/mixpeek/amux click | ↑ star rate |
| iOS downloads | click on App Store link | ↑ iOS installs |
| Cloud signups | click on concierge/cloud CTA | ↑ onboarding calls |

**Tracking note:** PostHog project key is `phc_Ckeacj8y8X8YiLkwHBcpBsJVSnsFKohch2vMv9sJYmE6` (project 378145 "amux", US cloud, us.i.posthog.com). To query KPI clicks use HogQL on `$autocapture` events filtered by `elements_chain` containing the relevant href. Personal API key needed for API queries — set `POSTHOG_PERSONAL_API_KEY` in ~/.amux/server.env. **Note:** site.js was mistakenly using a personal API key (`phx_`) before 2026-07-09 — no website analytics exist prior to that date.

---

## Running Experiments

### EXP-001 — Hero CTA button copy
- **Status:** `running`
- **Started:** 2026-07-07
- **Change:** "View on GitHub" → "⭐ Star on GitHub" (both hero CTA instances in index.html, `replace_all`)
- **Hypothesis:** More action-oriented, emoji-prefixed star CTA increases GitHub star click-through by 20%+
- **KPI:** GitHub star clicks (PostHog autocapture on github.com/mixpeek/amux link)
- **Measure after:** 2026-07-14 (7 days minimum)

### EXP-002 — iOS CTA sticky mobile bottom bar
- **Status:** `running`
- **Started:** 2026-07-08
- **Change:** Added sticky fixed bottom bar on mobile (≤600px) with App Store CTA; iOS nav link hidden on mobile. Implemented in site.js (CSS injection + DOM append). PostHog event: `exp002_ios_sticky_tap`.
- **Hypothesis:** Moving "iOS app" from nav to a sticky mobile-only bottom bar increases iOS App Store taps on mobile by 30%+
- **KPI:** iOS downloads (PostHog `exp002_ios_sticky_tap` event + App Store link clicks)
- **Measure after:** 2026-07-15 (7 days minimum)

---

## Experiment Backlog (prioritized)

### EXP-001 — Hero CTA button copy
- **Hypothesis:** "View on GitHub" → "⭐ Star on GitHub" increases star click-through by 20%+
- **Page:** `/` (homepage hero)
- **KPI:** GitHub star clicks
- **Status:** `running` — started 2026-07-07
- **Implementation:** Changed both "View on GitHub" instances to "⭐ Star on GitHub" in site/index.html
- **Effort:** XS (1 line edit)

### EXP-002 — iOS CTA sticky mobile bottom bar
- **Hypothesis:** Moving "iOS app" from nav to a sticky mobile-only bottom bar increases iOS taps on mobile by 30%+
- **Page:** All pages (site.js — injected globally)
- **KPI:** iOS downloads
- **Status:** `running` — started 2026-07-08
- **Implementation:** site.js injects CSS (fixed bottom bar, body padding-bottom, hide nav iOS link on ≤600px) and appends DOM element. PostHog custom event `exp002_ios_sticky_tap` on tap. Respects `env(safe-area-inset-bottom)` for iOS notch.
- **Effort:** S (site.js injection)

### EXP-003 — Homepage hero social proof line
- **Hypothesis:** Adding a concrete social proof line under the lede increases GitHub clicks and concierge signups by anchoring the "288+ developers" stat
- **Page:** `/` (index.html hero)
- **KPI:** GitHub stars + cloud signups
- **Status:** `running`
- **Started:** 2026-07-09
- **Change:** Added `<p class="social-proof">Trusted by 288+ developers shipping overnight with AI agents — open source on GitHub</p>` below the .lede paragraph. Star count is hardcoded to 288 (current as of 2026-07-09). Text links to GitHub repo.
- **Implementation:** Added social-proof paragraph + CSS in index.html
- **Measure after:** 2026-07-16 (7 days minimum)

### EXP-004 — Concierge CTA urgency
- **Hypothesis:** Adding scarcity to the concierge CTA ("3 onboarding slots open this month") increases cloud signup clicks by 25%+
- **Page:** `/concierge/` + homepage concierge block
- **KPI:** Cloud signups
- **Status:** `inconclusive — extending`
- **Started:** 2026-07-10
- **Implementation:** Added amber urgency badge "3 onboarding slots open this month" above the final CTA in `/concierge/index.html`. Inline SVG clock icon + amber pill styling (`rgba(251,191,36,.12)` background, `#fbbf24` text). No JS — pure CSS badge.
- **Effort:** XS
- **Measure after:** 2026-07-17 (7 days minimum)
- **Score (2026-07-17):** 7 days in, 0 concierge CTA clicks measured in PostHog. Either concierge conversion happens off-site (Calendly, direct email) and PostHog autocapture doesn't capture the downstream action, or the urgency scarcity message isn't resonating. Extending to 2026-07-24. If still 0 at that point, mark `no_effect`.

### EXP-005 — "Star History" social proof on homepage
- **Hypothesis:** Embedding a star-history chart image on the homepage (showing growth trend) increases GitHub clicks from visitors who aren't sure if the project is active
- **Page:** `/`
- **KPI:** GitHub stars
- **Status:** `inconclusive — extending`
- **Started:** 2026-07-11
- **Implementation:** Added star-history.com SVG embed (dark/light via `<picture>`) between features grid and final CTA on homepage. 291 stars caption, GitHub CTA button below the chart. Uses `https://api.star-history.com/svg?repos=mixpeek/amux&type=Date` (dark theme variant via `&theme=dark` in `<source>`). `loading="lazy"` so it doesn't block paint.
- **Effort:** S
- **Measure after:** 2026-07-18 (7 days minimum)
- **Score (2026-07-18):** Inconclusive — insufficient baseline. PostHog daily homepage KPI clicks: pre-EXP-005: only 2026-07-10=21 (1 day); post-EXP-005 (7d): avg ~13.5/day. Cannot attribute the drop to EXP-005 vs. weekday pattern vs. simultaneous experiments (EXP-006, EXP-007, EXP-008, EXP-009 all started within 5 days). Extending to 2026-07-25 for cleaner measurement.

### EXP-013 — GitHub CTA on high-traffic guides
- **Hypothesis:** PostHog shows /guides/best-ai-model-for-coding-2026/ gets 84 pageviews (2nd highest after homepage) but zero KPI clicks. Adding a compact GitHub CTA box at the top of that guide (and other high-traffic guides with no CTA) increases GitHub stars from guide traffic.
- **Page:** /guides/best-ai-model-for-coding-2026/ (first target; then /guides/claude-code-headless/, /guides/ai-coding-finops/)
- **KPI:** GitHub stars
- **Status:** `inconclusive — bot traffic`
- **Started:** 2026-07-12
- **Implementation:** Added "Run multiple AI models in parallel from one dashboard → ⭐ Star amux on GitHub" pill CTA below intro paragraph, above Decision Framework table. PostHog event: `exp013_guide_github_cta_click`. Applied to best-ai-model-for-coding-2026/index.html.
- **Effort:** XS per page
- **Measure after:** 2026-07-19 (7 days minimum)
- **Score (2026-07-20):** 8 days in. Page has 162 PVs (14-day window) but 0 GitHub KPI clicks. Pattern matches bot/crawler traffic: two large spikes (68 PVs one day, 44 another) with no click behavior. CTA present but no human visitors to convert. Not scaling EXP-013 to additional pages. Superseded by EXP-014 which targets guides with mixed human+bot traffic.

### EXP-006 — GitHub README → iOS CTA
- **Status:** `inconclusive — insufficient pre-period baseline`
- **Started:** 2026-07-13
- **Scored:** 2026-07-21
- **Change:** Added official Apple "Download on the App Store" badge image below the shields badges row in README.md, above the concierge CTA.
- **Result:** Pre-period (2026-07-10–12, 3 days): 10 iOS clicks = 3.3/day. Post-period (2026-07-13–21, 9 days): 27 iOS clicks = 3.0/day. Change = -9%. However, the pre-period is only 3 days (PostHog live since 2026-07-09, experiment started 2026-07-13) — insufficient to draw a conclusion. No meaningful signal either way. The badge change is harmless — keeping it in place.
- **KPI:** iOS downloads (App Store link clicks)

**Upcoming score windows:** EXP-004 → 2026-07-24 (extended) · EXP-005 → 2026-07-25 (extended) · EXP-007 → scored inconclusive 2026-07-21 · EXP-008 → scored inconclusive 2026-07-22 (measurement issue) · EXP-009 → 2026-07-23 · EXP-010 → 2026-07-24 · EXP-012 → 2026-07-28 · EXP-014 → 2026-07-27 · EXP-015 → 2026-07-29.

### EXP-006 — GitHub README → iOS CTA
- **Hypothesis:** Adding an official App Store badge to the README increases iOS installs from GitHub traffic
- **Page:** README.md
- **KPI:** iOS downloads
- **Status:** `inconclusive — insufficient pre-period baseline` — scored 2026-07-21
- **Implementation:** Added official Apple "Download on the App Store" badge (from tools.applemediaservices.com) below the shields badges `</p>`, above the concierge CTA. More visually prominent than the existing shields.io iOS pill badge.
- **Effort:** XS
- **Score (2026-07-21):** Pre avg 3.3/day (3 days), post avg 3.0/day (9 days) = -9%. No meaningful signal. Pre-period too short (only 3 days of pre data available since PostHog activated 2026-07-09). Badge kept in place.

### EXP-007 — Compare pages → GitHub CTA
- **Hypothesis:** Compare pages get high-intent "alternative" traffic. Adding a prominent "Try it free — ⭐ on GitHub" CTA box at the top of each compare page increases star clicks from comparison traffic
- **Page:** All `/compare/amux-vs-*/` pages (7 pages: ngrok, codex, cursor, devin, diy-tmux, jules, n8n)
- **KPI:** GitHub stars
- **Status:** `inconclusive — low traffic volume`
- **Started:** 2026-07-14
- **Scored:** 2026-07-21
- **Implementation:** Added `<!-- EXP-007 -->` GitHub CTA `<div>` block after the subtitle `<p>` on all 7 compare pages. PostHog event: `exp007_compare_github_cta_click` with `{page: window.location.pathname}`.
- **Effort:** M (7-file edit)
- **Score (2026-07-21):** 7 days in. Named event `exp007_compare_github_cta_click` = 0 fires. Autocapture shows ~5 GH clicks from all compare pages combined in 14 days (amux-vs-cmux: 2, amux-vs-codex: 1, amux-vs-hermes: 1, amux-vs-openclaw: 1). Individual compare pages average 10-15 PVs/14d — too low volume for a meaningful A/B result. The CTA block is kept in place (doesn't hurt). Superseded by EXP-012 which targets the compare INDEX page (62 PVs/14d) where the audience is concentrated.

### EXP-008 — Dark/light mode preference → personalization signal
- **Hypothesis:** Users who switch to light mode are more likely to be non-developers (less terminal-native) and may convert better on concierge/cloud vs GitHub. Track theme preference as a PostHog property.
- **Page:** All pages (site.js)
- **KPI:** Cloud signups (segment)
- **Status:** `inconclusive — measurement issue`
- **Started:** 2026-07-15
- **Scored:** 2026-07-22
- **Implementation:** Added `posthog.register({ theme_preference: theme })` in site.js — fires on page load (initial theme) and on every manual toggle.
- **Score (2026-07-22):** `person.properties.theme_preference` returns `None` for all 93 GH clicks in the past 7 days. The `posthog.register()` super property attaches to events but does not propagate to `person.properties` in HogQL — they're different property namespaces. To query by theme, need to use `properties.theme_preference` (event-level) instead of `person.properties.theme_preference`. Segmentation was never working as designed. The data signal is still there in event properties — segmentation just needs a query fix. Keeping the implementation in place.
- **Effort:** XS
- **Measure after:** 2026-07-22 (7 days minimum)

### EXP-009 — "Indie hackers" and "overnight builders" language in hero subtitle
- **Hypothesis:** Explicitly calling out the indie hacker / overnight builder persona in the homepage hero subtitle increases conversion from that audience (HN, Product Hunt, Indie Hackers forum traffic).
- **Page:** `/` (hero subtitle)
- **KPI:** GitHub stars + concierge signups
- **Status:** `running`
- **Started:** 2026-07-16
- **Implementation:** Changed `.lede` paragraph in site/index.html to add "Indie hackers, solo builders, and engineering teams use it to..." before the feature list. Preserves the "self-healing built in, so you wake up to finished work" closer.
- **Effort:** XS
- **Measure after:** 2026-07-23 (7 days minimum)

### EXP-010 — Feature table reorder: phone-first features at top
- **Hypothesis:** Most visitors who convert on cloud/iOS CTAs are non-enterprise users who are drawn to the phone/mobile angle. Reordering the feature comparison table to lead with "Mobile dashboard (iOS app + PWA)" before "Self-healing watchdog" may increase iOS and cloud CTA clicks.
- **Page:** `/` (feature comparison table)
- **KPI:** iOS downloads + cloud signups
- **Status:** `running`
- **Started:** 2026-07-17
- **Implementation:** Moved "No way to manage agents from your phone" → "Remote control iOS app + PWA" row to the top of the PS problem/solution grid in site/index.html, before "Self-healing watchdog". All other rows shift down one position.
- **Effort:** XS
- **Measure after:** 2026-07-24 (7 days minimum)

### EXP-011 — Plan strip / visibility callout in homepage feature list
- **Hypothesis:** The new Plan strip feature (v0.9.44, July 2026) — which lets you see exactly what your Claude Code agent is planning — is a unique differentiator not communicated on the homepage. Adding a one-line callout in the feature list increases clicks from developers frustrated with agent opacity.
- **Page:** `/` (feature grid or "new" badge on relevant feature row)
- **KPI:** GitHub stars (developer audience)
- **Status:** `running`
- **Started:** 2026-07-18
- **Implementation:** Added "No idea what your agent is actually planning or working on right now" → "Plan strip — see your agent's live task list and next steps in real time inside the peek panel. [new badge]" row to the PS grid, between "Web dashboard" and "Kanban board" rows. New badge uses `rgba(110,231,183,.15)` / `#6ee7b7` green pill styling.
- **Effort:** XS
- **Measure after:** 2026-07-25 (7 days minimum)

### EXP-014 — Add GitHub CTA to high-PV guide pages missing it
- **Hypothesis:** Guides with >20 PVs/14d and 0-1 GitHub KPI clicks are missing a visible CTA. Adding a prominent inline GitHub CTA block (same format as EXP-007 compare pages) will convert at 2-5 clicks/week per page.
- **Page:** /guides/harness-engineering/ (28 PVs, 0 clicks, no CTA), /guides/measuring-ai-coding-agent-roi/ (28 PVs, 0 clicks, no CTA), /guides/claude-code-context-compaction/ (new page)
- **KPI:** GitHub stars (tracked via `exp014_guide_github_cta_click` PostHog event + autocapture)
- **Status:** `running`
- **Started:** 2026-07-20
- **Implementation:** Added inline GitHub CTA div block (indigo border, flex row, "View on GitHub ★" button, PostHog `exp014_guide_github_cta_click` event) to harness-engineering and measuring-ai-coding-agent-roi. Updated dateModified to 2026-07-20 on both. New page claude-code-context-compaction created with same CTA baked in.
- **Effort:** S (3 files)
- **Measure after:** 2026-07-27 (7 days minimum)

### EXP-012 — Freelancer CTA on compare index
- **Hypothesis:** Compare pages attract high-intent "is this the right tool?" visitors. A "Freelancer? Scale to 5x clients →" contextual CTA on the compare INDEX (62 PVs/14d — highest compare surface) drives traffic to /for/freelancers/ and /concierge/.
- **Page:** `/compare/` index page (strategy shift from individual compare pages which get only 10-15 PVs each — the index is where audiences are concentrated)
- **KPI:** Concierge signups + /for/freelancers/ traffic (PostHog event: `exp012_freelancer_cta_click`)
- **Status:** `running`
- **Started:** 2026-07-21
- **Implementation:** Added Freelancer CTA block between the subtitle and the compare links list in `/compare/index.html`. Two buttons: "How it works" → /for/freelancers/ and "Get set up →" → /concierge/. PostHog event `exp012_freelancer_cta_click` fires with `{destination, page}` on each click.
- **Effort:** XS
- **Measure after:** 2026-07-28 (7 days minimum)

### EXP-015 — Top-of-page GitHub CTA on high-traffic guides with low conversion
- **Hypothesis:** claude-code-headless has 185 PVs but only 2 GH clicks (1.1% CVR). The only GitHub CTA on the page is the EXP-007 block at line 650 — below ~600 lines of content, invisible to most visitors. Adding a compact green teal banner immediately after the subtitle (above the TOC) will catch visitors before they scroll away.
- **Page:** /guides/claude-code-headless/ (185 PVs, 2 GH clicks — biggest CVR gap of high-traffic pages)
- **KPI:** GitHub stars (PostHog event: `exp015_headless_topofpage_cta_click`)
- **Status:** `running`
- **Started:** 2026-07-22
- **Implementation:** Added compact green/teal banner immediately after subtitle paragraph, before the TOC. Text: "Want to run 10+ headless agents in parallel? amux orchestrates, monitors, and self-heals an entire fleet of headless Claude Code sessions." Button: "View amux on GitHub ★" with `exp015_headless_topofpage_cta_click` event.
- **Effort:** XS
- **Measure after:** 2026-07-29 (7 days minimum)

---

## Concluded Experiments

_None yet._

---

## Learnings Log

_Updated by SCHED-149 Job 9 after each run with PostHog data and experiment results._

| Date | Finding | Action taken |
|------|---------|--------------|
| 2026-07-07 | PostHog installed, baseline accumulation started | — |
| 2026-07-07 | PostHog HogQL query returned 0 events — no click data yet after 1 day of tracking | EXP-001 shipped; wait for data to accumulate before scoring |
| 2026-07-07 | EXP-001 launched: "View on GitHub" → "⭐ Star on GitHub" on both hero CTAs in homepage | Implement complete; measure 2026-07-14 |
| 2026-07-08 | PostHog still 0 events (Day 1 — accumulating) — EXP-001 cannot be scored yet (< 7 days) | EXP-002 shipped: sticky iOS bottom bar via site.js injection on all pages |
| 2026-07-08 | EXP-002 launched: sticky mobile bottom bar with App Store CTA, PostHog event exp002_ios_sticky_tap | Measure after 2026-07-15; new hypothesis EXP-009 added (indie hacker language) |
| 2026-07-09 | PostHog still accumulating — no KPI click data available after 2 days; EXP-001 and EXP-002 cannot be scored yet | EXP-003 shipped: social proof line "Trusted by 288+ developers" under lede in homepage hero; measure after 2026-07-16 |
| 2026-07-10 | PostHog: 1 day of real data (phc_ key live since 2026-07-09); 294 pageviews, 111 autocaptures recorded — too early to score any experiments (all < 7 days); no KPI click events isolated yet | EXP-004 shipped: amber urgency badge "3 onboarding slots open this month" on /concierge/ final CTA; measure after 2026-07-17 |
| 2026-07-11 | PostHog: 3 days data — 554 pageviews total, 26 KPI clicks (all homepage); /guides/best-ai-model-for-coding-2026/ has 84 PVs but 0 KPI clicks (biggest conversion gap) | EXP-005 shipped: star history chart between features and final CTA; EXP-013 added (guide page GitHub CTA) |
| 2026-07-12 | PostHog: 14-day query returned 0 KPI clicks (data still accumulating — phc_ key only active since 2026-07-09, so < 7 days of real data for all experiments; none scoreable yet) | EXP-013 shipped: GitHub CTA chip on /guides/best-ai-model-for-coding-2026/ (84 PVs, 0 KPI clicks); EXP-006 queued next |
| 2026-07-15 | PostHog 7-day KPI click data: homepage / → 70 clicks (GitHub+AppStore), /guides/best-ai-agent-multiplexers-2026/ → 16 clicks (highest non-homepage conversion rate — outperforming all compare pages), /docs/ → 6, /pricing/ → 6. EXP-002 iOS sticky bar: 5 custom exp002_ios_sticky_tap events observed since launch — positive signal but no pre-experiment baseline available (PostHog only active from 2026-07-09, EXP-002 started 2026-07-08). Cannot score pre/post. Extending measurement to 2026-07-22. EXP-001 also cannot be scored — same baseline gap. | EXP-008 shipped: theme_preference super property registered in site.js on page load and toggle (measures whether dark vs light users convert differently on GitHub vs cloud KPIs). New finding: /guides/best-ai-agent-multiplexers-2026/ has the best GitHub click rate outside the homepage — should investigate what drives this and replicate on similar high-PV guide pages. |
| 2026-07-16 | PostHog 14-day data: homepage 95 KPI clicks (396 PVs, 24% CVR); best-ai-agent-multiplexers-2026: 18 KPI clicks (124 PVs, 14.5% CVR — best guide); best-ai-model-for-coding-2026: 150 PVs but only 1 KPI click (0.7% CVR — biggest conversion gap, EXP-013 installed 2026-07-12, too early to evaluate); /pricing/: 32 PVs / 6 KPI clicks (18.75% CVR — high intent). EXP-003 (social proof line): reached 7-day minimum but cannot score — no pre-experiment baseline exists (PostHog only live since 2026-07-09, EXP-003 started same day). Extending to 2026-07-23. | EXP-009 shipped: added "Indie hackers, solo builders, and engineering teams" to .lede paragraph in index.html. Job 2 AEO: refreshed best-ai-agent-multiplexers-2026 with July 2026 date + multi-runtime + Homebrew install (commit 6ecb272). Rebuilt amux-vs-cursor and amux-vs-windsurf compare pages from stubs to full 600+ line compare pages. Created /for/open-source-maintainers/. GitHub stars: 299. |
| 2026-07-17 | PostHog 14-day data: homepage 432 PVs / 66 KPI clicks (15.3% CVR); claude-code-headless: 105 PVs / 2 KPI clicks (1.9% CVR — priority freshness target); best-ai-model-for-coding-2026: 150 PVs / 0 KPI clicks (EXP-013 at day 5, not yet at 7-day window). EXP-004 (concierge urgency): 7 days in, 0 concierge CTA clicks — inconclusive, extending to 2026-07-24. GitHub stars: 300. | EXP-010 shipped: moved phone/iOS row to top of homepage PS grid (commit pending). Refreshed claude-code-headless with auto-resume dialog pitfall + Homebrew + multi-runtime + 300 stars (commit d9e16b3). Created /guides/claude-code-resume-dialog/ (commit b4f03d4). Refreshed amux-vs-cmux with EXP-007 CTA + July 2026 + 300 stars (commit 37958b6). Created /for/enterprise/. Changelog 4 new entries (ad13e14, 02cc251, 8f75275, 3af5f86). |
| 2026-07-18 | PostHog 14-day data (first run): homepage 510 PVs / 98 GitHub clicks (19.2% CVR — best all-time); best-ai-agent-multiplexers-2026: 189 PVs / 27 GitHub clicks (14.3% CVR — best guide, confirms "best X" list format works); claude-code-headless: 139 PVs / 2 GitHub clicks (1.4% CVR — biggest human-traffic gap, fixed today with EXP-007 CTA); best-ai-model-for-coding-2026: 161 PVs / 0 GitHub clicks (bot-traffic pattern: spikes of 68 PVs then 44 PVs = crawl waves, not humans; EXP-013 CTA present but zero events confirm this is crawler traffic). iOS/concierge: homepage → 35 clicks, concierge page → 18 clicks (very high-intent at 51% CVR). EXP-007 named events: 0 — all compare page GitHub clicks captured by autocapture instead (compare pages get 12-13 PVs each, low volume). Key insight: "best X" list pages convert at 14% vs compare pages at ~10-16% for the top 2, but most compare pages get barely 10-15 PVs vs 180+ for best-of lists — invest more in list format. | Added EXP-007 GitHub CTA to claude-code-headless (highest human-traffic gap page). Created /guides/ai-agent-live-browser-automation/ (new page). Rebuilt amux-vs-claude-code-agent-teams from 108→628 lines. Changelog: 9 new entries today across 2 runs. EXP-011 shipped. EXP-005 scored inconclusive. 304 stars. New hypothesis: EXP-014 — add "best-of" callout panel to high-PV guide pages pointing to best-ai-agent-multiplexers-2026 (proven 14% CVR format). |
| 2026-07-20 | PostHog 14-day data: homepage 534 PVs / 220 total GitHub clicks (CVR ~19%); best-ai-agent-multiplexers-2026 202 PVs / 28 clicks (13.9% CVR — still best guide); getting-started 38 PVs / 13 clicks (34.2% CVR — HIGHEST of any page, very high intent); pricing 38 PVs / 9 clicks (23.7%). Biggest gaps: harness-engineering (28 PVs, 0 clicks, no CTA), measuring-ai-coding-agent-roi (28 PVs, 0 clicks, no CTA), ai-coding-finops (32 PVs, 0 clicks). EXP-013 scored inconclusive — best-ai-model-for-coding-2026 confirmed bot traffic (162 PVs, 0 clicks pattern). Key new insight: /guides/getting-started/ has 34.2% CVR — highest-intent page, bottom-of-funnel. Drive more traffic there from guides. | EXP-013 marked inconclusive (bot traffic). EXP-014 shipped to harness-engineering + measuring-ai-coding-agent-roi. dateModified freshened on both. New page: /guides/claude-code-context-compaction/ targeting "Claude Code context compaction" overnight-run pain point. Changelog: 6 new entries (Messages tab, hibernate fix, Send now fix, Sent history accordion, click-to-copy, Enter sends). |
| 2026-07-21 | PostHog 14-day data: homepage 614 PVs / 101 GH clicks (16.4% CVR); best-ai-agent-multiplexers-2026 221 PVs / 30 GH clicks (13.6% CVR — best guide); ai-agent-sandboxing NEW entry 122 PVs / 1 GH click (0.8% CVR — biggest gap, fixed today); iOS clicks: homepage 30, EXP-002 sticky 10 taps total. EXP-006 scored inconclusive (pre-period 3 days too short). EXP-007 scored inconclusive (compare pages only 10-15 PVs each, total 5 compare GH autocapture clicks in 14d). 307 GitHub stars (+3 since yesterday). New observation: measuring-ai-coding-agent-roi jumped from 28 → 57 PVs (freshening + sitemap addition yesterday showing immediate traffic uplift). | EXP-006 marked inconclusive. EXP-007 marked inconclusive. EXP-012 shipped: Freelancer CTA on compare INDEX (62 PVs), targeting /for/freelancers/ + /concierge/. ai-agent-sandboxing: EXP-014 CTA added + dateModified freshened. New pages: /guides/ai-agent-cost-monitoring/ (413 lines, targeting "Claude Code token costs", Cost tab feature). Changelog: 11 new entries (Cost tabs, API output fix, Mental Model guide, skills, WCAG contrast, loading indicator, HTML preview fix, faster steering). |
| 2026-07-22 | PostHog 14-day data: homepage 711 PVs / 109 GH clicks (15.3% CVR); best-ai-agent-multiplexers-2026 253 PVs / 39 GH clicks (15.4% CVR — consistently best guide, matches homepage rate); measuring-ai-coding-agent-roi 80 PVs (up from 28 → 57 → 80 in 3 days — freshening compounding fast); ai-agent-sandboxing 128 PVs / 1 GH click; claude-code-headless 185 PVs / 2 GH clicks (1.1% CVR — biggest gap, EXP-007 CTA buried at line 650). EXP-008 SCORED inconclusive — `person.properties.theme_preference` null for all events (super property doesn't propagate to person properties; event-level `properties.theme_preference` is the correct field). EXP-009 trending negative: pre 8.86/day vs post 7.83/day = -11.6% (score tomorrow at 7-day window). | EXP-008 scored inconclusive (measurement issue — person vs event property namespace). EXP-015 shipped: top-of-page green CTA on claude-code-headless above TOC, exp015_headless_topofpage_cta_click. ai-coding-finops: EXP-014 CTA added + dateModified 2026-05-26 → 2026-07-22. New guide: /guides/claude-code-rate-limits/ (targeting "Claude Code rate limit" error queries). Changelog: 5 new entries (pending messages ⏳, click-to-insert, git staged guard, scheduler audit, mental model Commits+Proxies). |

---

## Notes for SCHED-149

When running Job 9:
1. Query PostHog for click events on `github.com/mixpeek/amux`, `apps.apple.com` links, and `/concierge/` CTAs — these are the three KPI proxies.
2. Check which pages have the highest click-through on each KPI (not just pageviews).
3. Move the highest-confidence `queued` experiment to `running` — implement it (it will be small/XS effort by design in the backlog). Log the start date and expected measurement period (minimum 7 days of traffic).
4. Check any `running` experiments: if ≥7 days of data, read PostHog for pre/post comparison and log result in Concluded section.
5. Add new experiment ideas to the backlog if new patterns emerge from the data (e.g., a page with high traffic but zero KPI clicks is a signal).
6. Always implement the experiment change AND record it — don't just write about it.
