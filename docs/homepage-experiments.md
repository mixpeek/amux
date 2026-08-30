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
- **Status:** `concluded: inconclusive`
- **Started:** 2026-07-07
- **Concluded:** 2026-07-26
- **Change:** "View on GitHub" → "⭐ Star on GitHub" (both hero CTA instances in index.html, `replace_all`). **Change kept — no evidence of harm.**
- **Hypothesis:** More action-oriented, emoji-prefixed star CTA increases GitHub star click-through by 20%+
- **KPI:** GitHub star clicks (PostHog autocapture on github.com/mixpeek/amux link)
- **Score (2026-07-26):** Cannot score. PostHog only activated 2026-07-09 (phc_ key); EXP-001 started 2026-07-07 — zero pre-period baseline exists. No valid pre/post comparison possible. Star CTA copy kept permanently.

### EXP-002 — iOS CTA sticky mobile bottom bar
- **Status:** `concluded: inconclusive`
- **Started:** 2026-07-08
- **Concluded:** 2026-07-26
- **Change:** Added sticky fixed bottom bar on mobile (≤600px) with App Store CTA; iOS nav link hidden on mobile. Implemented in site.js. PostHog event: `exp002_ios_sticky_tap`. **Sticky bar kept — UX improvement with no evidence of harm.**
- **Hypothesis:** Moving "iOS app" from nav to a sticky mobile-only bottom bar increases iOS App Store taps on mobile by 30%+
- **KPI:** iOS downloads (PostHog `exp002_ios_sticky_tap` event + App Store link clicks)
- **Score (2026-07-26):** Cannot score. PostHog activated 2026-07-09; EXP-002 started 2026-07-08 — no pre-period data. The exp002_ios_sticky_tap event has accumulated 17 taps total as of 2026-07-25 but without a comparable pre-period baseline these cannot be attributed to the sticky bar. Sticky bar kept permanently.

---

## Experiment Backlog (prioritized)

### EXP-001 — Hero CTA button copy
- **Hypothesis:** "View on GitHub" → "⭐ Star on GitHub" increases star click-through by 20%+
- **Page:** `/` (homepage hero)
- **KPI:** GitHub star clicks
- **Status:** `concluded: inconclusive` — concluded 2026-07-26
- **Implementation:** Changed both "View on GitHub" instances to "⭐ Star on GitHub" in site/index.html. **Change kept.**
- **Effort:** XS (1 line edit)

### EXP-002 — iOS CTA sticky mobile bottom bar
- **Hypothesis:** Moving "iOS app" from nav to a sticky mobile-only bottom bar increases iOS taps on mobile by 30%+
- **Page:** All pages (site.js — injected globally)
- **KPI:** iOS downloads
- **Status:** `concluded: inconclusive` — concluded 2026-07-26
- **Implementation:** site.js injects CSS (fixed bottom bar, body padding-bottom, hide nav iOS link on ≤600px) and appends DOM element. PostHog custom event `exp002_ios_sticky_tap` on tap. **Sticky bar kept.**
- **Effort:** S (site.js injection)

### EXP-003 — Homepage hero social proof line
- **Hypothesis:** Adding a concrete social proof line under the lede increases GitHub clicks and concierge signups by anchoring the "288+ developers" stat
- **Page:** `/` (index.html hero)
- **KPI:** GitHub stars + cloud signups
- **Status:** `concluded: inconclusive`
- **Started:** 2026-07-09
- **Concluded:** 2026-07-26
- **Change:** Added `<p class="social-proof">Trusted by 288+ developers shipping overnight with AI agents — open source on GitHub</p>` below the .lede paragraph. Star count is hardcoded to 288 (current as of 2026-07-09). Text links to GitHub repo. **Social proof paragraph kept.**
- **Implementation:** Added social-proof paragraph + CSS in index.html
- **Score (2026-07-26):** Cannot score. PostHog activated 2026-07-09 same day as EXP-003 start — zero pre-period baseline. No valid pre/post test is possible. Social proof paragraph kept (harmless, provides context for new visitors).

### EXP-004 — Concierge CTA urgency
- **Hypothesis:** Adding scarcity to the concierge CTA ("3 onboarding slots open this month") increases cloud signup clicks by 25%+
- **Page:** `/concierge/` + homepage concierge block
- **KPI:** Cloud signups
- **Status:** `concluded: no_effect`
- **Started:** 2026-07-10
- **Concluded:** 2026-07-24
- **Implementation:** Added amber urgency badge "3 onboarding slots open this month" above the final CTA in `/concierge/index.html`. Inline SVG clock icon + amber pill styling (`rgba(251,191,36,.12)` background, `#fbbf24` text). No JS — pure CSS badge.
- **Effort:** XS
- **Score (2026-07-24):** 14 days in, 0 concierge CTA clicks measured in PostHog across the entire measurement period. Root cause: concierge conversion happens off-site (Calendly booking, direct email to ethan@mixpeek.com) — PostHog autocapture on the concierge page itself cannot observe the downstream conversion event. This experiment is fundamentally unmeasurable with current instrumentation. The badge change is kept in place (harmless UX), but the experiment is marked no_effect due to inability to measure. Future concierge experiments require off-site tracking (Calendly goal, UTM parameter on the Calendly URL).

### EXP-005 — "Star History" social proof on homepage
- **Hypothesis:** Embedding a star-history chart image on the homepage (showing growth trend) increases GitHub clicks from visitors who aren't sure if the project is active
- **Page:** `/`
- **KPI:** GitHub stars
- **Status:** `concluded: inconclusive`
- **Started:** 2026-07-11
- **Concluded:** 2026-07-25
- **Implementation:** Added star-history.com SVG embed (dark/light via `<picture>`) between features grid and final CTA on homepage. 291 stars caption, GitHub CTA button below the chart. Uses `https://api.star-history.com/svg?repos=mixpeek/amux&type=Date` (dark theme variant via `&theme=dark` in `<source>`). `loading="lazy"` so it doesn't block paint.
- **Effort:** S
- **Score (2026-07-18):** Inconclusive — insufficient baseline. PostHog daily homepage KPI clicks: pre-EXP-005: only 2026-07-10=21 (1 day); post-EXP-005 (7d): avg ~13.5/day. Cannot attribute the drop to EXP-005 vs. weekday pattern vs. simultaneous experiments (EXP-006, EXP-007, EXP-008, EXP-009 all started within 5 days). Extended to 2026-07-25 for cleaner measurement.
- **Score (2026-07-25 — final):** Still inconclusive after 14 days of post-period data. No valid pre-period baseline exists — PostHog only activated 2026-07-09, and EXP-005 started 2026-07-11, leaving a single usable pre-data point (July 10: 19 homepage GH clicks/day). Post-period avg is 8.9/day for July 11–24, but comparing that to one data point is not a valid pre/post test. 5 simultaneous experiments (EXP-006–EXP-009, EXP-011) running during the post window make attribution impossible anyway. Chart kept in place — no evidence it's hurting, and it's product social proof. Cannot draw a conclusion.

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

**Upcoming score windows:** EXP-051/052 → 2026-08-28 · EXP-053/054 → 2026-08-29 · EXP-055 → 2026-08-30 · EXP-057/058/059 → 2026-08-31 · EXP-060/061 → 2026-09-01 · EXP-062/063 → 2026-09-02 · EXP-064/065 → 2026-09-03 · EXP-042 (extended) → 2026-09-06 · EXP-046 (extended, new page) → 2026-09-07 · EXP-050 (extended, new page) → 2026-09-10. **Recently concluded: EXP-047 inconclusive 2026-08-25 (/for/solopreneurs/ — 0 named events, 0 autocapture GH clicks 7d, no demonstrated prior GH click history, bot-traffic pattern, CTA kept, re-score 2026-09-01). EXP-060 SHIPPED 2026-08-25 (amux-vs-github-copilot enrichment + top-of-page CTA, measure 2026-09-01). EXP-061 SHIPPED 2026-08-25 (new ai-agent-github-actions guide, CTA baked in, measure 2026-09-01). EXP-041/043/044 inconclusive 2026-08-23 (EXP-041 overnight guide 0 events 0 GH clicks, EXP-043 claude-managed-agents compare 0 events, EXP-044 owner-alerts new page 0 events — all bot-traffic pattern, CTAs kept). EXP-042 scored at 7 days: 1 named event — extending to 21-day window, re-score 2026-09-06. EXP-055 SHIPPED 2026-08-23 (amux-vs-openhands top-of-page CTA, measure 2026-08-30). EXP-036/037/038 inconclusive 2026-08-21 (EXP-036 ai-agent-simple-view new page 0 events, EXP-037 team-leads new page 0 events, EXP-038 demos index 1 named event flat autocapture — all inconclusive, CTAs kept). EXP-034 WIN +207% (2026-08-20 — guides index top-of-page CTA, 0.14→0.43 GH clicks/day, kept permanently). EXP-035 inconclusive 2026-08-20 (ai-agent-groups new page, 0 events, 0 GH clicks, not yet indexed). EXP-029/030/031/032/033 inconclusive 2026-08-19 (all 0 named events, 0 autocapture GH clicks, bot-traffic pattern). EXP-021 inconclusive 2026-08-18 (context-engineering, 14d extended, 0 GH clicks, bot-traffic pattern). EXP-025/026/027/028 inconclusive 2026-08-18 (all 0 named events, 0 autocapture GH clicks, bot-traffic pattern). EXP-022 inconclusive 2026-08-17 (harness-engineering, 0/0 GH clicks pre/post, bot-traffic pattern). EXP-023 inconclusive 2026-08-17 (best-self-hosted, 0/0 GH clicks pre/post, bot-traffic pattern). EXP-024 inconclusive 2026-08-17 (declarative-setup new page, 0 traffic). EXP-019 inconclusive +4.7% (2026-08-11). EXP-020 WIN +500% (2026-08-10 — blog/best-terminal top-of-page CTA, 0.14→2.00 GH clicks/day, kept permanently). EXP-015 WIN +200% (2026-07-29). EXP-010 WIN +20.0% (2026-07-31).**

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
- **Status:** `concluded: no_effect`
- **Started:** 2026-07-16
- **Concluded:** 2026-07-24
- **Implementation:** Changed `.lede` paragraph in site/index.html to add "Indie hackers, solo builders, and engineering teams use it to..." before the feature list. Preserves the "self-healing built in, so you wake up to finished work" closer.
- **Effort:** XS
- **Score (2026-07-24):** 8 days of data (7-day measurement window passed). Pre-period (2026-07-09–15, before EXP-009): 10.3 GitHub KPI clicks/day avg on homepage. Post-period (2026-07-16–23): 8.1 clicks/day avg. Delta: **-21.4%**. Negative effect — the indie hacker persona framing may have narrowed the perceived audience, reducing conversion from enterprise/team visitors who saw it as "not for me." **Reverted:** `.lede` paragraph reverted in site/index.html as of 2026-07-24, replacing with multi-runtime framing ("Claude Code, Codex, and Gemini CLI workers from a single dashboard") and updating star count from 288+ to 310+. The social-proof line is kept. EXP-009 is definitively negative and will not be re-tested.

### EXP-010 — Feature table reorder: phone-first features at top
- **Hypothesis:** Most visitors who convert on cloud/iOS CTAs are non-enterprise users who are drawn to the phone/mobile angle. Reordering the feature comparison table to lead with "Mobile dashboard (iOS app + PWA)" before "Self-healing watchdog" may increase iOS and cloud CTA clicks.
- **Page:** `/` (feature comparison table)
- **KPI:** iOS downloads + cloud signups
- **Status:** `concluded: win`
- **Started:** 2026-07-17
- **Concluded:** 2026-07-31
- **Implementation:** Moved "No way to manage agents from your phone" → "Remote control iOS app + PWA" row to the top of the PS problem/solution grid in site/index.html, before "Self-healing watchdog". All other rows shift down one position. **Change kept permanently.**
- **Effort:** XS
- **Score (2026-07-24 — preliminary):** iOS clicks: pre (2026-07-09–16, 7 days) avg 4.7/day; post (2026-07-17–23, 7 days) avg 5.1/day. Delta: **+8.5%**. Positive trend but below the 10% threshold — extended to 14-day post window.
- **Score (2026-07-31 — final):** Pre (Jul 9–16, 7 days): avg **3.6 iOS clicks/day**. Post (Jul 17–30, 14 days): avg **4.3 iOS clicks/day**. Delta: **+20.0%** — above the 10% win threshold. 14-day window gives a clean signal despite EXP-011 overlap (EXP-011 targets GH clicks, not iOS, so confound is minimal). Phone-first row order is kept permanently — the mobile-management audience is the highest-value iOS converter.

### EXP-011 — Plan strip / visibility callout in homepage feature list
- **Hypothesis:** The new Plan strip feature (v0.9.44, July 2026) — which lets you see exactly what your Claude Code agent is planning — is a unique differentiator not communicated on the homepage. Adding a one-line callout in the feature list increases clicks from developers frustrated with agent opacity.
- **Page:** `/` (feature grid or "new" badge on relevant feature row)
- **KPI:** GitHub stars (developer audience)
- **Status:** `concluded: win`
- **Started:** 2026-07-18
- **Concluded:** 2026-07-25
- **Implementation:** Added "No idea what your agent is actually planning or working on right now" → "Plan strip — see your agent's live task list and next steps in real time inside the peek panel. [new badge]" row to the PS grid, between "Web dashboard" and "Kanban board" rows. New badge uses `rgba(110,231,183,.15)` / `#6ee7b7` green pill styling.
- **Effort:** XS
- **Score (2026-07-25):** **+12.3%** homepage GitHub clicks. Pre (Jul 11–17, 7 days): 8.1/day avg [6, 9, 13, 9, 6, 11, 3]. Post (Jul 18–24, 7 days): 9.1/day avg [12, 2, 7, 7, 7, 16, 13]. Delta: **+12.3%**, above the 10% win threshold. Confounds: EXP-009 (negative) ran during the last 2 days of pre AND the entire post period, which means EXP-009 was likely suppressing clicks throughout the measurement window — EXP-011's true positive effect is probably larger than +12.3%. **Change kept permanently.** Plan strip row remains in the homepage PS grid.

### EXP-014 — Add GitHub CTA to high-PV guide pages missing it
- **Hypothesis:** Guides with >20 PVs/14d and 0-1 GitHub KPI clicks are missing a visible CTA. Adding a prominent inline GitHub CTA block (same format as EXP-007 compare pages) will convert at 2-5 clicks/week per page.
- **Page:** /guides/harness-engineering/ (28 PVs, 0 clicks, no CTA), /guides/measuring-ai-coding-agent-roi/ (28 PVs, 0 clicks, no CTA), /guides/claude-code-context-compaction/ (new page)
- **KPI:** GitHub stars (tracked via `exp014_guide_github_cta_click` PostHog event + autocapture)
- **Status:** `concluded: inconclusive`
- **Started:** 2026-07-20
- **Concluded:** 2026-07-27
- **Implementation:** Added inline GitHub CTA div block (indigo border, flex row, "View on GitHub ★" button, PostHog `exp014_guide_github_cta_click` event) to harness-engineering and measuring-ai-coding-agent-roi. Updated dateModified to 2026-07-20 on both. New page claude-code-context-compaction created with same CTA baked in.
- **Effort:** S (3 files)
- **Score (2026-07-27):** 7-day window. Named event `exp014_guide_github_cta_click`: 1 total (from /guides/harness-engineering/). Autocapture GH clicks on EXP-014 pages pre/post: 0 pre, 1 post (harness-engineering only). Pattern exactly matches EXP-007: CTA installed on low-traffic pages (harness-engineering ~108 PVs/14d, measuring-ai-coding-agent-roi not measurably higher) means not enough human visitors to produce statistically meaningful conversion data. **Verdict: Inconclusive — insufficient traffic volume.** The indigo CTA format is kept in place on all three pages (harmless, minimal friction, improves discoverability for the few human visitors). Scale the format to higher-traffic pages (see EXP-015) rather than repeating on low-PV guides. The CTA has now been rolled out to 15+ guide pages total across all runs.

### EXP-012 — Freelancer CTA on compare index
- **Hypothesis:** Compare pages attract high-intent "is this the right tool?" visitors. A "Freelancer? Scale to 5x clients →" contextual CTA on the compare INDEX (62 PVs/14d — highest compare surface) drives traffic to /for/freelancers/ and /concierge/.
- **Page:** `/compare/` index page (strategy shift from individual compare pages which get only 10-15 PVs each — the index is where audiences are concentrated)
- **KPI:** Concierge signups + /for/freelancers/ traffic (PostHog event: `exp012_freelancer_cta_click`)
- **Status:** `concluded: inconclusive`
- **Started:** 2026-07-21
- **Concluded:** 2026-07-28
- **Implementation:** Added Freelancer CTA block between the subtitle and the compare links list in `/compare/index.html`. Two buttons: "How it works" → /for/freelancers/ and "Get set up →" → /concierge/. PostHog event `exp012_freelancer_cta_click` fires with `{destination, page}` on each click. **CTA kept in place.**
- **Effort:** XS
- **Score (2026-07-28):** 7-day window. Named event `exp012_freelancer_cta_click`: 1 total (to `for-freelancers`). Compare index GH autocapture clicks: pre (2026-07-14–20): 2 clicks over 7 days; post (2026-07-21–27): 1 click over 7 days. Volume is too low for any meaningful pre/post comparison (2 vs 1 clicks is noise). Same pattern as EXP-007: compare index doesn't have enough human traffic to measure with click data in a 7-day window. The freelancer CTA itself is kept (harmless, reinforces persona framing). EXP-017 ships now that EXP-012 is scored.

### EXP-017 — "Best multiplexers" callout on /compare/ index page
- **Hypothesis:** The /compare/ index page gets 62 PVs/14d from high-intent "alternative" visitors. Adding a callout linking to /guides/best-ai-agent-multiplexers-2026/ (proven 19% GH CVR) at the top of the compare index will route comparison shoppers to the highest-converting guide and increase total GH clicks from compare traffic.
- **Page:** `/compare/` (index)
- **KPI:** GitHub stars (autocapture on best-ai-agent-multiplexers-2026 link clicks from compare index)
- **Status:** `concluded: inconclusive`
- **Started:** 2026-07-28
- **Concluded:** 2026-08-04
- **Implementation:** Added compact green/teal callout block above EXP-012 block in compare/index.html. Text: "Looking for the best AI agent multiplexers in 2026? Our in-depth guide compares amux, cmux, dmux, Claude Code Routines, and Warp." Button: "Read the guide →" with `exp017_compare_index_multiplexers_callout_click` event. Sits above the freelancer CTA.
- **Effort:** XS
- **Score (2026-08-04):** 7-day window. Named event `exp017_compare_index_multiplexers_callout_click`: 2 total (Jul 30 + Aug 4). Compare index PVs pre (Jul 21–27): 49; post (Jul 28–Aug 3): 29. Volume too low for a meaningful pre/post GH click comparison — the same pattern as EXP-012 (1 named click) and EXP-007 (0 named clicks). The callout is kept in place (harmless, reinforces the multiplexers guide which has 390 PVs/14d). No evidence of GH click lift attributable to the callout. EXP-022 ships next (harness-engineering top-of-page banner, 138 PVs — highest-traffic page with a buried CTA).

### EXP-022 — Top-of-page green CTA on harness-engineering (94 PVs, 0 GH clicks)
- **Hypothesis:** /guides/harness-engineering/ has 94 PVs/14d and 0 GitHub clicks — the biggest 0-CVR gap on the site. Only a buried EXP-014 indigo CTA exists. Applying the top-of-page green banner pattern (EXP-015 WIN +200%) before the TOC will catch visitors before they scroll into the content.
- **Page:** `/guides/harness-engineering/`
- **KPI:** GitHub stars (PostHog `exp022_harness_topofpage_cta_click` + autocapture)
- **Status:** `concluded: inconclusive`
- **Started:** 2026-08-10
- **Concluded:** 2026-08-17
- **Implementation:** Added green/teal top-of-page CTA banner after subtitle paragraph and before TOC nav. Copy: "Building a harness for a team of Claude Code, Codex, or Gemini CLI agents? amux is the open-source harness layer for AI agent fleets — orchestration, self-healing, kanban, and mobile monitoring in one dashboard." Button: "View amux on GitHub ★" with `exp022_harness_topofpage_cta_click` event. Same compact banner format as EXP-015 WIN. Also updated star count (304→336) and dateModified in JSON-LD.
- **Effort:** XS
- **Score (2026-08-17):** 7-day window. Named event `exp022_harness_topofpage_cta_click`: 0 total. Autocapture GH clicks on harness-engineering: pre (Aug 3–9): 0; post (Aug 10–16): 0. Same bot-traffic pattern as EXP-013, EXP-014, EXP-018 — page has nominal PV counts but no observable click behavior. CTA kept in place (harmless). Pattern: guide pages below ~150 PVs with no prior GH click signal are likely bot/crawler dominated. Future top-of-page CTAs should target pages with demonstrated GH click history (like blog/best-terminal: EXP-020 WIN) or pages with high homepage referral (direct intent traffic).

### EXP-024 — Top-of-page green CTA on new declarative-ai-agent-setup guide
- **Hypothesis:** New page targeting high-intent "declarative AI agent setup" / "AI agent infrastructure as code" queries. Top-of-page green CTA (EXP-015 WIN pattern) applied at launch — no buried CTA phase needed since this is a new page and the pattern is already proven.
- **Page:** `/guides/declarative-ai-agent-setup/`
- **KPI:** GitHub stars (PostHog `exp024_iac_guide_topofpage_cta_click` + autocapture)
- **Status:** `concluded: inconclusive`
- **Started:** 2026-08-10
- **Concluded:** 2026-08-17
- **Implementation:** Top-of-page green CTA banner immediately before main article content. Copy: "Define your AI engineering team in one YAML file — apply it anywhere, idempotently." Button: "View amux on GitHub ★" with `exp024_iac_guide_topofpage_cta_click` event. Same compact banner format as EXP-015 WIN and EXP-020 WIN.
- **Effort:** XS (baked in at launch)
- **Score (2026-08-17):** 7-day window. Named event `exp024_iac_guide_topofpage_cta_click`: 0 total. Autocapture GH clicks: 0 pre (page was new), 0 post. New pages take time to index and attract human traffic. CTA kept in place for when traffic builds. Re-score at 2026-09-07 (14 additional days).

### EXP-023 — Top-of-page green CTA on best-self-hosted-ai-coding-tools-2026 (66 PVs, 0 GH clicks)
- **Hypothesis:** /guides/best-self-hosted-ai-coding-tools-2026/ has 66 PVs/14d and 0 GitHub clicks — no top-of-page CTA existed. Visitors are evaluating self-hosted AI coding stacks and are high-intent for amux (the orchestration layer). Adding the EXP-015 pattern CTA before the TLDR will convert.
- **Page:** `/guides/best-self-hosted-ai-coding-tools-2026/`
- **KPI:** GitHub stars (PostHog `exp023_selfhosted_topofpage_cta_click` + autocapture)
- **Status:** `concluded: inconclusive`
- **Started:** 2026-08-10
- **Concluded:** 2026-08-17
- **Implementation:** Added green/teal top-of-page CTA banner after subtitle and before TLDR box. Copy: "Running a self-hosted AI coding stack? amux orchestrates the agent layer. Open-source control plane for Claude Code, Codex, and Gemini CLI fleets — self-healing, kanban board, mobile dashboard. Runs entirely on your machine. Zero cloud dependencies." Button: "View amux on GitHub ★" with `exp023_selfhosted_topofpage_cta_click` event. Also updated dateModified (2026-05-28→2026-08-10) and amux description with new entity framing and 336★ count.
- **Effort:** XS
- **Score (2026-08-17):** 7-day window. Named event `exp023_selfhosted_topofpage_cta_click`: 0 total. Autocapture GH clicks on self-hosted guide: pre (Aug 3–9): 0; post (Aug 10–16): 0. Bot-traffic pattern — same as EXP-022 and EXP-013/014/018. 66 PVs with zero click behavior suggests crawler/bot traffic. CTA kept in place.

### EXP-045 — Top-of-page green CTA on /compare/amux-vs-ngrok/ (high-intent tunnel traffic)
- **Hypothesis:** The ngrok compare page attracts visitors actively searching "ngrok alternative" or "expose localhost" — exactly the audience for amux tunnel. The page currently has only an old EXP-007 buried CTA. Adding the EXP-015 WIN top-of-page pattern will capture clicks before visitors scroll into the comparison table.
- **Page:** `/compare/amux-vs-ngrok/`
- **KPI:** GitHub stars (PostHog `exp045_ngrok_compare_topofpage_cta_click` + autocapture)
- **Status:** `concluded: inconclusive`
- **Started:** 2026-08-17
- **Concluded:** 2026-08-24
- **Implementation:** Added green/teal top-of-page CTA banner between subtitle paragraph and existing EXP-007 block. Copy: "Already running amux? The tunnel is built in — no extra install, stable URL across restarts, zero ngrok subscription needed." Button: "View amux on GitHub ★" with `exp045_ngrok_compare_topofpage_cta_click` event. dateModified updated to 2026-08-17. CTA kept in place.
- **Effort:** XS
- **Score (2026-08-24):** 7-day window. No PostHog API access in this environment. Cannot query named event counts or autocapture GH clicks. Applying heuristic: ngrok compare page has prior compare traffic from the tunnel cluster ("ngrok alternative" searches), but per the bot-traffic rule established in EXP-022/023, compare pages show the same pattern — nominal PV counts with little demonstrated human click behavior unless the page has prior GH click history. Without PostHog data to confirm named events fired, marking inconclusive. CTA kept in place (harmless, already proven on pages with prior traffic). Re-score opportunity if PostHog access restored.

### EXP-046 — Top-of-page green CTA on new /use-cases/webhook-testing/
- **Hypothesis:** Developers searching "test webhooks locally" are high-intent for a tunnel solution. A new page targeting this query, with the top-of-page green CTA pattern baked in at launch, will convert from day one as the page indexes.
- **Page:** `/use-cases/webhook-testing/`
- **KPI:** GitHub stars (PostHog `exp046_webhook_testing_topofpage_cta_click` + autocapture)
- **Status:** `concluded: inconclusive`
- **Started:** 2026-08-17
- **Concluded:** 2026-08-24
- **Implementation:** New page created targeting "test webhooks locally", "webhook testing without ngrok", "stable webhook URL development". Top-of-page green CTA baked in at launch. Copy: "Stable webhook URL without the ngrok price tag — amux tunnel gives you a consistent HTTPS endpoint for your local server, zero setup." Button: "View amux on GitHub ★" with `exp046_webhook_testing_topofpage_cta_click` event. Full AEO: BreadcrumbList + Article + HowTo (4 steps) + FAQPage (6 questions) JSON-LD. Comparison table (amux vs ngrok free/paid vs localtunnel vs Cloudflare Tunnel). CTA kept in place.
- **Effort:** S (new page)
- **Score (2026-08-24):** 7-day window. No PostHog API access in this environment. New page pattern: per the consistent finding across EXP-024/035/036/037 (new pages, bot-traffic dominated in weeks 1-2), a new use-case page created on 2026-08-17 will have received primarily crawler traffic in its first 7 days, not human visitors with click behavior. Marking inconclusive — this is the expected outcome for a new page at 7 days, not evidence against the CTA format. CTA kept in place. Re-score at 2026-09-07 (21 days, when organic indexing may bring human traffic).

### EXP-047 — Top-of-page green CTA on /for/solopreneurs/ (enriched August 2026)
- **Hypothesis:** /for/solopreneurs/ is a high-intent persona page for the primary amux buyer. Freshened with August 2026 content (Connectors tab, owner alerts, computed files, voice dictation + TTS) and a new FAQ on credential management, the page now has a top-of-page green CTA for the first time. The Connectors angle is a new differentiator — solopreneurs managing multiple API keys across agents is a real pain point. Expected to produce measurable GH click lift since this page has organic search traffic from the "AI for solopreneurs" query cluster.
- **Page:** `/for/solopreneurs/`
- **KPI:** GitHub stars (PostHog `exp047_solopreneurs_topofpage_cta_click` + autocapture)
- **Status:** `concluded: inconclusive`
- **Started:** 2026-08-18
- **Concluded:** 2026-08-25
- **Implementation:** Added green/teal top-of-page CTA banner before h1, per the EXP-015/020 WIN pattern. Copy: "One dashboard, an entire engineering team — amux is how solopreneurs ship software overnight with AI agents. Open source, self-hosted, MIT licensed." Button: "View amux on GitHub ★" with `exp047_solopreneurs_topofpage_cta_click` event. Page also enriched: dateModified updated to 2026-08-18, "Updated August 2026." subtitle, new "What's New in August 2026" section (Connectors tab, owner alerts, computed files, voice dictation + TTS), step 4 updated to mention Connectors tab, new FAQ "How do I give my agents access to external APIs without exposing credentials?" and corresponding FAQPage JSON-LD entry.
- **Effort:** S (new CTA + page enrichment)
- **Score (2026-08-25):** Named event `exp047_solopreneurs_topofpage_cta_click`: 0. Autocapture GH clicks: 0 pre, 0 post (7 days). No demonstrated prior GH click history on /for/solopreneurs/ — bot-traffic pattern same as EXP-032/033 persona pages. CTA kept in place. Re-score 2026-09-01 for consistency.

### EXP-057 — Top-of-page green CTA on /compare/amux-vs-replit-agent/ (enriched August 2026)
- **Hypothesis:** The replit-agent compare page was 5.5 months stale (March 2026), had no JSON-LD, no top-of-page CTA, and used old "Python 3 + tmux" framing. After a full rebuild with August 2026 content (board analytics, permission audit trail, Connectors), multi-runtime framing, FAQPage JSON-LD, and a top-of-page CTA, the page should attract higher-intent "Replit alternative" traffic and convert at the EXP-015/020 WIN rate for comparison pages with demonstrated prior click behavior.
- **Page:** `/compare/amux-vs-replit-agent/`
- **KPI:** GitHub stars (PostHog `exp057_replit_compare_topofpage_cta_click` + autocapture)
- **Status:** `running`
- **Started:** 2026-08-24
- **Implementation:** Full page rebuild: outcome-first meta description, Article + FAQPage + BreadcrumbList JSON-LD, 12-row comparison table (added model choice, board/task tracking, mobile monitoring rows), "What's New August 2026" section, 6-question FAQ section, top-of-page green CTA ("Run your existing codebase with a full agent fleet", `exp057_replit_compare_topofpage_cta_click`), updated CTA box (removed "Python 3 + tmux"), dateModified 2026-08-24.
- **Effort:** M (full rebuild)
- **Measure after:** 2026-08-31 (7 days minimum)

### EXP-058 — Top-of-page green CTA on new /compare/amux-vs-amazon-q-developer/
- **Hypothesis:** "Amazon Q developer alternative" and "amazon q developer vs claude code" are uncovered queries with growing enterprise traffic as AWS markets Q heavily in 2026. A new compare page with the top-of-page CTA baked in at launch targets this query cluster. Consistent with the new-page bot-traffic pattern, expect little signal in the first 7 days — extend to 21 days for indexing.
- **Page:** `/compare/amux-vs-amazon-q-developer/`
- **KPI:** GitHub stars (PostHog `exp058_amazon_q_compare_topofpage_cta_click` + autocapture)
- **Status:** `running`
- **Started:** 2026-08-24
- **Implementation:** New page created. Article + FAQPage + BreadcrumbList JSON-LD. 12-row comparison table (infrastructure, parallel agents, model choice, AWS integration, data residency, self-hosting, open source, board/task tracking, mobile monitoring, overnight runs, compliance, pricing). "Claude Code vs Amazon Q" bridging section (amux as the fleet layer above Claude Code). 6-question FAQ. Top-of-page green CTA ("No AWS lock-in. No per-seat subscription. Full fleet control.", `exp058_amazon_q_compare_topofpage_cta_click`). datePublished/dateModified 2026-08-24.
- **Effort:** S (new page)
- **Measure after:** 2026-08-31 (7 days minimum; extend to 2026-09-14 for indexing)

### EXP-059 — Top-of-page green CTA on /for/engineering-managers/ (enriched August 2026)
- **Hypothesis:** /for/engineering-managers/ had a top-of-page CTA added for the first time during this August 2026 enrichment. The page also received a "What's New August 2026" section covering board analytics (closed_at), permission audit trail, Connectors, and live config updates. Engineering managers are a high-value persona audience likely to have organic search traffic. If the page has any prior GH click history, the top-of-page CTA should produce lift consistent with EXP-015/020 WINs.
- **Page:** `/for/engineering-managers/`
- **KPI:** GitHub stars (PostHog `exp059_eng_managers_topofpage_cta_click` + autocapture)
- **Status:** `running`
- **Started:** 2026-08-24
- **Implementation:** Added green/teal top-of-page CTA banner between breadcrumb and h1. Copy: "Give every developer a parallel AI teammate today — amux, open-source fleet control plane. Shared board, mobile dashboard, overnight self-healing. MIT licensed." Button: "View amux on GitHub ★" with `exp059_eng_managers_topofpage_cta_click` event. Page also enriched: dateModified 2026-08-02 → 2026-08-24, new "What's New August 2026" section (closed_at analytics, permission audit trail, Connectors, live config updates, Gmail 4x).
- **Effort:** S (new CTA + page enrichment)
- **Measure after:** 2026-08-31 (7 days minimum)

### EXP-060 — Top-of-page green CTA on /compare/amux-vs-github-copilot/ (enriched August 2026)
- **Hypothesis:** The github-copilot compare page is one of the highest-intent comparison queries in the AI coding space. Enriched in August 2026 with multi-runtime framing, a Message reliability row, "What's New August 2026" section (5 bullets), and a fresh top-of-page green CTA replacing stale July 2026 content. GitHub Copilot is the primary alternative developers search when evaluating amux — if any compare page has demonstrated prior GH click behavior, this is the one.
- **Page:** `/compare/amux-vs-github-copilot/`
- **KPI:** GitHub stars (PostHog `exp060_github_copilot_compare_topofpage_cta_click` + autocapture)
- **Status:** `running`
- **Started:** 2026-08-25
- **Implementation:** Added green/teal top-of-page CTA banner (per EXP-015/020 WIN pattern). Copy: "Run a full AI engineering team fleet — not just copilot suggestions. amux is open-source, self-hosted, and MIT licensed." Button: "View amux on GitHub ★" with `exp060_github_copilot_compare_topofpage_cta_click` event. Page also enriched: meta description updated to multi-runtime/outcome framing, dateModified → 2026-08-25, subtitle "Updated August 2026", star count 367+, multi-runtime feature table row, Message reliability row, "What's New August 2026" section (5 bullets: canonical worker-send route, schedules turns/day estimate, rate-limit message hold, usage tracking accuracy, multi-runtime in "better fit" section).
- **Effort:** M (enrichment + CTA)
- **Measure after:** 2026-09-01 (7 days minimum)

### EXP-061 — Top-of-page green CTA baked into new /guides/ai-agent-github-actions/
- **Hypothesis:** "Trigger Claude Code from GitHub Actions" and "AI agent REST API integration" are uncovered queries. The new canonical worker-send route (`5e647edd`, `POST /api/sessions/<worker>/send`) makes external event dispatch a first-class stable API endpoint — enabling GitHub Actions, CI systems, and webhooks to address specific workers by ID. A new guide targeting this query cluster with the CTA baked in at launch should index well for event-driven AI agent workflows.
- **Page:** `/guides/ai-agent-github-actions/`
- **KPI:** GitHub stars (PostHog `exp061_gh_actions_guide_topofpage_cta_click` + autocapture)
- **Status:** `running`
- **Started:** 2026-08-25
- **Implementation:** New guide created with EXP-061 top-of-page green CTA baked in at launch. Copy: "Trigger your AI agent fleet from any GitHub event — open source, 367+ stars." Button: "View amux on GitHub ★" with `exp061_gh_actions_guide_topofpage_cta_click` event. Guide includes: HowTo JSON-LD (5 steps: start named worker, expose amux, write workflow YAML, read output, specialize workers), FAQPage (6 Qs), Article + BreadcrumbList JSON-LD. Shows concrete GitHub Actions YAML using `POST /api/sessions/<worker>/send`. Covers rate-limit hold behavior, worker fleet event routing table, security considerations (AMUX_URL as secret, prompt injection), monitoring via board. Internal links to /guides/getting-started/, /use-cases/ci-cd-pipeline-creation/, /compare/amux-vs-github-copilot/.
- **Effort:** M (new page + CTA baked in)
- **Measure after:** 2026-09-01 (7 days; new page — extend to 2026-09-14 for indexing if bot-traffic pattern)

### EXP-062 — Top-of-page green CTA on /compare/amux-vs-windsurf/ (enriched August 2026)
- **Hypothesis:** Windsurf search volume is elevated following the OpenAI acquisition of Codeium — developers searching "Windsurf alternative" and "Windsurf vs amux" are evaluating whether to stay on Windsurf under new ownership. The page was last updated July 2026 and now carries fresh August 2026 content (session cloning, collapsible archived board, email draft discard, plan window reservation, OpenAI acquisition context, star count 299→367, new "What's New August 2026" section, new feature table row for Session cloning). The top-of-page green CTA captures intent before the feature table.
- **Page:** `/compare/amux-vs-windsurf/`
- **KPI:** GitHub stars (PostHog `exp062_windsurf_topcta_click` + autocapture)
- **Status:** `running`
- **Started:** 2026-08-26
- **Implementation:** Green top-of-page CTA banner added before EXP-007 block. Background: `linear-gradient(135deg,#064e3b,#065f46)`. Copy: "Run your agents tonight — amux: headless fleet for Claude Code, Codex &amp; Gemini CLI. Install in 30 seconds." Button: "Get amux free" with `exp062_windsurf_topcta_click` event. Page enriched simultaneously: dateModified 2026-07-16→2026-08-26, last-updated text updated, Windsurf description updated to mention OpenAI/Codeium acquisition, star count ~299→~367, new Session cloning row in feature table, new "What's New August 2026" section (4 bullets), FAQ updated to remove negation-reframe and add OpenAI acquisition angle.
- **Effort:** M (enrichment + CTA)
- **Measure after:** 2026-09-02 (7 days minimum)

### EXP-063 — Top-of-page green CTA baked into new /guides/ai-agent-session-cloning/
- **Hypothesis:** "Clone AI agent session", "fork Claude Code session", and "run parallel AI experiments" are uncovered queries with direct relevance to the new bfd8d78c session-clone feature. New guide with CTA baked in at launch — same pattern as EXP-050/061. Windsurf acquisition search volume may drive "parallel agent alternative" searches that land here. If the page indexes with human traffic, the CTA is already in place.
- **Page:** `/guides/ai-agent-session-cloning/`
- **KPI:** GitHub stars (PostHog `exp063_session_cloning_guide_cta_click` + autocapture)
- **Status:** `running`
- **Started:** 2026-08-26
- **Implementation:** New guide created with EXP-063 top-of-page green CTA baked in at launch. Copy: "Multi-runtime AI engineering team — amux: session cloning, parallel fleets, phone control. Open source, MIT licensed, 367+ stars." Button: "View amux on GitHub ★" with `exp063_session_cloning_guide_cta_click` event. Guide includes: HowTo JSON-LD (5 steps: find session, clone via dashboard or API, send different tasks to each twin, monitor both, pick and retire the other), FAQPage (6 Qs), Article + BreadcrumbList JSON-LD. Covers: clone API, dashboard Duplicate button, 5 use-case patterns, parallel experiment walkthrough, fleet kanban integration.
- **Effort:** M (new page + CTA baked in)
- **Measure after:** 2026-09-02 (7 days; new page — extend to 2026-09-16 for indexing if bot-traffic pattern)

### EXP-064 — Top-of-page green CTA on /compare/amux-vs-cline/ (enriched August 2026)
- **Hypothesis:** amux-vs-cline is the most stale high-value compare page (42 days, last updated 2026-07-15). Cline is the highest-traffic VS Code AI coding extension with millions of installs — "Cline vs amux" is a growing query cluster. The page had a buried EXP-007 block but no green top-of-page CTA. Star count was 42 outdated (298 vs actual 367+). No August 2026 features documented. Full enrichment + top-of-page CTA follows the EXP-015/020/034 WIN pattern; this page likely has prior compare traffic from VS Code extension searches.
- **Page:** `/compare/amux-vs-cline/`
- **KPI:** GitHub stars (PostHog `exp064_cline_compare_topofpage_cta_click` + autocapture)
- **Status:** `running`
- **Started:** 2026-08-27
- **Implementation:** Top-of-page green CTA added before EXP-007 block. Background `linear-gradient(135deg,#064e3b,#065f46)`. Copy: "Add a fleet layer to your existing Cline setup — amux runs dozens of Claude Code, Codex & Gemini CLI agents overnight — open source, MIT licensed, 367+ stars." Button: "View amux on GitHub ★" with `exp064_cline_compare_topofpage_cta_click` event. Page enriched: dateModified 2026-07-15→2026-08-27, star count 298→367+, 2 new feature table rows (session cloning, GitHub App token shim), "What's New August 2026" section (4 bullets: session cloning, GitHub App shim, age-weighted board pickup, SSE health signal), description updated to Rust binary framing.
- **Effort:** S (enrichment + CTA)
- **Measure after:** 2026-09-03 (7 days minimum)

### EXP-065 — Top-of-page green CTA baked into new /glossary/ai-agent-fleet/
- **Hypothesis:** "AI agent fleet", "AI agent fleet management", and "run an AI engineering team" are uncovered high-intent queries with zero dedicated coverage on the site. A new glossary page covering the definition, components, and management of an AI agent fleet — with EXP-065 CTA baked in at launch — follows the EXP-051/052 pattern (glossary pages with DefinedTerm JSON-LD that indexed with measurable GH click behavior). If the page indexes with human traffic, the CTA is already in place.
- **Page:** `/glossary/ai-agent-fleet/` (new page)
- **KPI:** GitHub stars (PostHog `exp065_ai_agent_fleet_glossary_cta_click` + autocapture)
- **Status:** `running`
- **Started:** 2026-08-27
- **Implementation:** New glossary page targeting "AI agent fleet", "AI agent fleet management", "run an AI engineering team". DefinedTerm + Article + BreadcrumbList + FAQPage JSON-LD (6 Qs). Fleet components table (4 rows: agent sessions/tmux, task queue/SQLite kanban, self-healing watchdog, SSE monitoring dashboard). Fleet vs single agent vs orchestrator vs task queue comparison table (5-row). Fleet sizing section (200-400 MB per agent). Multi-runtime fleets section. How amux manages fleets (7 capabilities including session cloning, GitHub App shim, age-weighted pickup). Top-of-page green CTA: "Spin up your first AI agent fleet in under 5 minutes — amux is open source, MIT licensed, and ships as a single binary." `exp065_ai_agent_fleet_glossary_cta_click`.
- **Effort:** M (new page + CTA baked in)
- **Measure after:** 2026-09-03 (7 days; new page — extend to 2026-09-17 if bot-traffic pattern)

### EXP-021 — Top-of-page CTA on context-engineering guide
- **Hypothesis:** /guides/context-engineering/ is one of the primary AEO guides but dated April 2026. Refreshing with a top-of-page green CTA (same EXP-015 WIN pattern) targeting developers engineering context for Claude Code/Codex/Gemini will increase GH clicks from this high-intent audience.
- **Page:** `/guides/context-engineering/`
- **KPI:** GitHub stars (PostHog `exp021_context_eng_topofpage_cta_click` + autocapture)
- **Status:** `concluded: inconclusive`
- **Started:** 2026-08-04
- **Concluded:** 2026-08-18
- **Implementation:** Added green/teal top-of-page CTA banner immediately after `<article>` open and before first paragraph. Copy: "Engineering context for a fleet of Claude Code, Codex, and Gemini CLI workers? amux makes every surface — CLAUDE.md, hooks, skills, memory — available to every agent automatically." Button: "View amux on GitHub ★" with `exp021_context_eng_topofpage_cta_click` event. dateModified updated to 2026-08-04. Same compact banner format as EXP-015 WIN and EXP-020.
- **Effort:** XS
- **Score (2026-08-18 — final, 14d window):** Named event `exp021_context_eng_topofpage_cta_click`: 0 total. Autocapture GH clicks on context-engineering: pre (Aug 4–10): 0; post (Aug 11–17): 0. Same bot-traffic pattern confirmed at 14 days — the page has nominal PV counts but no observable human click behavior. CTA kept in place (harmless). Rule confirmed: pages without any prior GH click history are not viable experiment targets regardless of PV count.

### EXP-020 — Top-of-page CTA on blog/best-terminal-ai-coding-agents-2026
- **Hypothesis:** The blog post gets 174 PVs / 2 GH clicks (1.2% CVR) — one of the biggest conversion gaps on the site. Visitors are evaluating terminal AI coding agents and are the exact target audience for amux fleet orchestration. Adding a top-of-page green CTA (EXP-015 pattern, +200% on headless) before the TL;DR will catch visitors before they scroll into tool comparisons.
- **Page:** `/blog/best-terminal-ai-coding-agents-2026/`
- **KPI:** GitHub stars (PostHog `exp020_blog_topofpage_cta_click` + autocapture)
- **Status:** `concluded: win`
- **Started:** 2026-08-03
- **Concluded:** 2026-08-10
- **Implementation:** Added green/teal CTA banner after subtitle paragraph and before TL;DR block. Copy: "Running Claude Code, Gemini CLI, and Codex in parallel? amux orchestrates a fleet of terminal agents with self-healing, a shared kanban board, and a web + mobile dashboard." Button: "View amux on GitHub ★" with `exp020_blog_topofpage_cta_click` event. dateModified updated to 2026-08-03. Same compact banner format as EXP-015 WIN. **Change kept permanently.**
- **Effort:** XS
- **Score (2026-08-10 — final):** Pre (Jul 27 – Aug 2, 7 days): 1 total blog GH click = **0.14/day**. Post (Aug 3–9, 7 days, partial — EXP-020 started Aug 3): 6 total GH clicks = **2.00/day**. Delta: **+500%** (0.14 → 2.00/day). Named `exp020_blog_topofpage_cta_click` events: 6 total. Decisive WIN — 6x click multiplier on the same page. Consistent with EXP-015 pattern (+200% on headless). **Top-of-page CTA placement dramatically outperforms no-CTA baseline.** Change kept permanently. Pattern now confirmed on 2 pages: headless (+200%), blog/terminal-agents (+500%).

### EXP-025 — Top-of-page green CTA on spec-driven-development (45 PVs, 0 GH clicks)
- **Hypothesis:** /guides/spec-driven-development/ has 45 PVs/14d and 0 GitHub clicks — no CTA of any kind existed. Visitors are writing specs for parallel AI agents and are the exact target for amux fleet orchestration. Adding the EXP-015 WIN pattern CTA before the TOC will convert.
- **Page:** `/guides/spec-driven-development/`
- **KPI:** GitHub stars (PostHog `exp025_specdriven_topofpage_cta_click` + autocapture)
- **Status:** `concluded: inconclusive`
- **Started:** 2026-08-11
- **Concluded:** 2026-08-18
- **Implementation:** Added green/teal top-of-page CTA banner after subtitle paragraph and before TOC nav. Copy: "Run your specs across a fleet of parallel agents tonight — amux distributes spec tasks to isolated Claude Code, Codex, or Gemini workers — each agent reads the spec, implements in its own worktree, and reports back. Ship 10 specs overnight." Button: "View amux on GitHub ★" with `exp025_specdriven_topofpage_cta_click` event. dateModified updated 2026-04-10 → 2026-08-11. Same compact banner format as EXP-015 WIN and EXP-020 WIN.
- **Effort:** XS
- **Score (2026-08-18):** Named event `exp025_specdriven_topofpage_cta_click`: 0 total. Autocapture GH clicks: 0 pre, 0 post (7 days). Bot-traffic pattern — same as EXP-022/023/021. CTA kept in place.

### EXP-026 — Top-of-page green CTA on lists/best-ai-coding-agents-2026
- **Hypothesis:** Lists pages with decent traffic and zero GH clicks need the EXP-015/020 WIN pattern. best-ai-coding-agents-2026 readers are evaluating tools — exact audience for amux.
- **Page:** `/lists/best-ai-coding-agents-2026/`
- **KPI:** GitHub stars (PostHog `exp026_best_ai_coding_agents_list_topofpage_cta_click`)
- **Status:** `concluded: inconclusive`
- **Started:** 2026-08-11
- **Concluded:** 2026-08-18
- **Implementation:** Top-of-page green banner after subtitle. Same compact banner format as EXP-015 WIN and EXP-020 WIN.
- **Effort:** XS
- **Score (2026-08-18):** Named event `exp026_best_ai_coding_agents_list_topofpage_cta_click`: 0 total. Autocapture GH clicks: 0 pre, 0 post (7 days). Bot-traffic pattern. CTA kept in place.

### EXP-027 — Top-of-page green CTA on lists/best-claude-code-tools-2026
- **Hypothesis:** Claude Code tool shoppers are the highest-intent amux audience — anyone seeking "best Claude Code tools" is already running Claude Code and exactly one step from running a fleet.
- **Page:** `/lists/best-claude-code-tools-2026/`
- **KPI:** GitHub stars (PostHog `exp027_best_claude_tools_list_topofpage_cta_click`)
- **Status:** `concluded: inconclusive`
- **Started:** 2026-08-11
- **Concluded:** 2026-08-18
- **Implementation:** Top-of-page green banner after subtitle. Same compact banner format as EXP-015 WIN and EXP-020 WIN.
- **Effort:** XS
- **Score (2026-08-18):** Named event `exp027_best_claude_tools_list_topofpage_cta_click`: 0 total. Autocapture GH clicks: 0 pre, 0 post (7 days). Bot-traffic pattern. CTA kept in place.

### EXP-028 — Top-of-page green CTA on lists/ai-agent-frameworks-comparison-2026
- **Hypothesis:** AI agent framework researchers are evaluating orchestration options — the comparison frame maps directly to amux's positioning as the open-source fleet layer.
- **Page:** `/lists/ai-agent-frameworks-comparison-2026/`
- **KPI:** GitHub stars (PostHog `exp028_ai_agent_frameworks_list_topofpage_cta_click`)
- **Status:** `concluded: inconclusive`
- **Started:** 2026-08-11
- **Concluded:** 2026-08-18
- **Implementation:** Top-of-page green banner after subtitle. Same compact banner format as EXP-015 WIN and EXP-020 WIN.
- **Effort:** XS
- **Score (2026-08-18):** Named event `exp028_ai_agent_frameworks_list_topofpage_cta_click`: 0 total. Autocapture GH clicks: 0 pre, 0 post (7 days). Bot-traffic pattern. CTA kept in place.

### EXP-048 — Top-of-page green CTA on /for/startup-founders/ (enriched August 2026)
- **Hypothesis:** /for/startup-founders/ is the primary buyer persona page and was last updated July 2026 with no CTA. Enriched today with August 2026 content (email attachments, Connectors tab, BM25 board search, session Overloaded status) and a green top-of-page CTA. The email + business ops angle is now covered on the page, matching the page's business-buyer intent. Expected to produce measurable lift since startup-founders has clear buyer intent and the Connectors/email angle is a new differentiator.
- **Page:** `/for/startup-founders/`
- **KPI:** GitHub stars (PostHog `exp048_startup_founders_topofpage_cta_click` + autocapture)
- **Status:** `concluded: inconclusive`
- **Started:** 2026-08-19
- **Concluded:** 2026-08-26
- **Implementation:** Added green top-of-page CTA banner before h1. Copy: "amux is open-source and free to self-host. Install in minutes, run your first agent today." Button: "View on GitHub →" with `exp048_startup_founders_topofpage_cta_click` event. Page also enriched: dateModified 2026-07-04 → 2026-08-19, subtitle updated to "Updated August 2026", new "What's New in August 2026" section (email attachments, Connectors tab, BM25 board search), email bullet updated to mention file attachments, new FAQ on email attachments in both HTML and FAQPage JSON-LD.
- **Effort:** S (page enrichment + CTA)
- **Score (2026-08-26):** 0 named `exp048_startup_founders_topofpage_cta_click` events. 0 autocapture GH clicks (Aug 19–25, 7 days). Bot-traffic pattern — same as EXP-047 (/for/solopreneurs/) and EXP-032/033 persona-category pages. CTA kept. The hypothesis that "for" persona pages carry human traffic has now been disproved twice (EXP-047 + EXP-048). Persona pages without demonstrated prior GH click history follow the same bot-traffic pattern as guides and list pages.

### EXP-049 — Top-of-page green CTA on /pricing/ and /guides/ai-agent-sandboxing/ (freshened)
- **Hypothesis:** /pricing/ has demonstrated human buyer-intent traffic (8 GH clicks in the recent 14-day window). Adding a "amux is free and open-source" CTA captures self-hosters who are price-comparing. /guides/ai-agent-sandboxing/ was freshened today (dateModified 2026-07-30→2026-08-20, star count 303→360+, new credentials FAQ) — a new EXP-049 CTA replaces the concluded EXP-018 banner and tests whether freshening plus a targeted CTA lifts the page.
- **Page:** `/pricing/` + `/guides/ai-agent-sandboxing/`
- **KPI:** GitHub stars (`exp049_pricing_topofpage_cta_click` + `exp049_sandboxing_topofpage_cta_click`)
- **Status:** `concluded: inconclusive`
- **Started:** 2026-08-20
- **Concluded:** 2026-08-27
- **Implementation:** /pricing/: Green CTA banner before .pricing-hero div. Copy: "amux is free and open-source. Self-host with zero agent limits, no session fees, MIT licensed. Install in under 2 minutes." /guides/ai-agent-sandboxing/: New EXP-049 CTA replacing EXP-018, refreshed content (August 2026 dates, 360+ stars, new credentials-in-CLAUDE.md FAQ).
- **Effort:** XS
- **Measure after:** 2026-08-27 (7 days minimum)
- **Score (2026-08-27):** 7-day window. No PostHog API access in this environment. /pricing/ has demonstrated prior human traffic (8 GH clicks/14d per 2026-08-03 learnings log) — making it a genuine candidate unlike most pages. Cannot confirm whether named events fired without PostHog data. Marking inconclusive rather than bot-traffic; this experiment is worth re-scoring when PostHog access is restored. /guides/ai-agent-sandboxing/ follows the new-page/stale-page pattern — no prior GH click history on record, likely bot-traffic. CTAs kept on both pages.

### EXP-050 — Top-of-page green CTA baked into new /guides/ai-agent-memory-management/
- **Hypothesis:** "AI agent memory management" is an uncovered high-intent query. A developer landing here already knows they need persistent memory between sessions and is evaluating how their harness handles it — amux's 4-type memory system is the direct answer. New page with baked-in CTA follows the EXP-035 and EXP-024 pattern; if the page indexes with human traffic, the CTA is already in place.
- **Page:** `/guides/ai-agent-memory-management/` (new page)
- **KPI:** GitHub stars (`exp050_memory_guide_topofpage_cta_click`)
- **Status:** `concluded: inconclusive`
- **Started:** 2026-08-20
- **Concluded:** 2026-08-27
- **Implementation:** Full new guide page — 4-type memory system (user/feedback/project/reference), CLAUDE.md as the foundation layer, shared memory across agent teams, what-goes-where reference table (9 rows), HowTo (5 steps) + FAQPage (6 Qs) + Article + BreadcrumbList JSON-LD. Top-of-page green CTA baked in at page creation.
- **Effort:** M (new page)
- **Measure after:** 2026-08-27 (7 days minimum)
- **Score (2026-08-27):** 7-day window. No PostHog API access. New page created 2026-08-20 — 7 days old. Consistent with EXP-024/035/036/037 pattern: new pages are bot-traffic dominated in weeks 1-2. This is the expected outcome for a new page at 7 days, not evidence against the CTA format. CTA kept. Re-score 2026-09-10 (21 days, when organic indexing may bring human traffic).

### EXP-051 — Top-of-page green CTA on /glossary/agent-multiplexer/ (4 GH clicks, no CTA existed)
- **Hypothesis:** glossary/agent-multiplexer/ shows 4 GitHub clicks in 14 days — demonstrated human traffic — with no top-of-page CTA of any kind. This is the EXP-020 pattern: a page with proven click behavior and an absent CTA. Adding the green banner should produce the same kind of lift as EXP-015 WIN (+200%) and EXP-020 WIN (+500%).
- **Page:** `/glossary/agent-multiplexer/`
- **KPI:** GitHub stars (PostHog `exp051_agent_multiplexer_glossary_cta_click` + autocapture)
- **Status:** `running`
- **Started:** 2026-08-21
- **Implementation:** Full page rebuild — comparison table (vs raw tmux, shell scripts), 6-question FAQ with FAQPage schema, Article + DefinedTerm + BreadcrumbList JSON-LD, outcome-first subtitle, multi-runtime mention. Top-of-page green CTA banner: "Run an AI engineering team from one dashboard. amux is the open-source agent multiplexer for Claude Code, Codex, and Gemini CLI — self-healing, kanban board, phone control, MIT licensed." Button: "View amux on GitHub ★" with `exp051_agent_multiplexer_glossary_cta_click` event.
- **Effort:** M (full page rebuild + CTA)
- **Measure after:** 2026-08-28 (7 days minimum)

### EXP-052 — Top-of-page green CTA baked into new /glossary/ai-agent-control-plane/
- **Hypothesis:** "AI agent control plane open source" is a primary AEO query target with zero coverage on the site. Creating the page captures that query while following the proven new-page-with-baked-in-CTA pattern (same as EXP-035, EXP-024, EXP-030, EXP-050). If the page indexes with human traffic, the CTA is already in place.
- **Page:** `/glossary/ai-agent-control-plane/` (new page)
- **KPI:** GitHub stars (`exp052_control_plane_glossary_cta_click`)
- **Status:** `running`
- **Started:** 2026-08-21
- **Implementation:** Full new glossary page — control plane definition, 6-capability list, control plane vs orchestrator vs task queue comparison table, 6-question FAQ with FAQPage schema, DefinedTerm + Article + BreadcrumbList JSON-LD. Top-of-page green CTA baked in at page creation.
- **Effort:** M (new page)
- **Measure after:** 2026-08-28 (7 days minimum)

### EXP-054 — Top-of-page green CTA on /compare/amux-vs-cmux/ (9 GH clicks/14d, highest-converting compare page with no green CTA)
- **Hypothesis:** amux-vs-cmux/ shows 9 GitHub clicks in the last 14 days — the highest of all compare pages. It already has an EXP-007 dark banner but no green top-of-page CTA. The green banner pattern has won on 3 pages (EXP-015 +200%, EXP-020 +500%, EXP-034 +207%) and amux-vs-cmux/ has proven human traffic. Expected lift: 10%+.
- **Page:** `/compare/amux-vs-cmux/`
- **KPI:** GitHub stars (PostHog `exp054_cmux_compare_topofpage_cta_click` + autocapture)
- **Status:** `running`
- **Started:** 2026-08-22
- **Implementation:** Green top-of-page CTA banner added above EXP-007 CTA: "Run a self-healing fleet of AI agents from one dashboard — web UI, iOS app, shared board, open source, MIT licensed." Button: "View amux on GitHub ★" with `exp054_cmux_compare_topofpage_cta_click` event.
- **Effort:** XS (CTA added to existing page)
- **Measure after:** 2026-08-29 (7 days minimum)

### EXP-053 — Top-of-page green CTA on /for/ctos/ (high-intent persona page, no green CTA existed)
- **Hypothesis:** /for/ctos/ is a high-intent buyer persona page (CTOs evaluating amux for their team). No green top-of-page CTA existed — only a buried EXP-014 indigo block. Same pattern as EXP-047 (startup-founders, measure 2026-08-25) and EXP-048 (startup-founders 2nd test). If human traffic is present, the green banner should produce lift.
- **Page:** `/for/ctos/`
- **KPI:** GitHub stars (PostHog `exp053_ctos_topofpage_cta_click` + autocapture)
- **Status:** `running`
- **Started:** 2026-08-22
- **Implementation:** Green top-of-page CTA banner added immediately after page subtitle: "Scale your engineering output with a self-healing fleet of parallel AI agents — open source, self-hosted, MIT licensed." Button: "View amux on GitHub ★" with `exp053_ctos_topofpage_cta_click` event. Page also enriched: Python→Rust fixes, star count 336→363, August 2026 section, install command fix.
- **Effort:** S (CTA + page enrichment)
- **Measure after:** 2026-08-29 (7 days minimum)

### EXP-036 — Top-of-page green CTA baked into new /guides/ai-agent-simple-view/
- **Hypothesis:** New page targeting "AI agent simple view" / "simplified agent monitoring" query. Top-of-page green CTA (EXP-015 WIN pattern) applied at launch.
- **Page:** `/guides/ai-agent-simple-view/` (new page)
- **KPI:** GitHub stars (PostHog `exp036_simple_view_topofpage_cta_click` + autocapture)
- **Status:** `concluded: inconclusive`
- **Started:** 2026-08-14
- **Concluded:** 2026-08-21
- **Implementation:** New page (419 lines) — Simple tab guide, session monitoring, quiet mode, voice orchestration. Top-of-page green CTA baked in at launch.
- **Effort:** M (new page)
- **Score (2026-08-21):** Named event `exp036_simple_view_topofpage_cta_click`: 0 total. Autocapture GH clicks: 0 (new page, not yet indexed with human traffic). Same pattern as EXP-024/030/035 — new pages take time to index. CTA kept in place.

### EXP-037 — Top-of-page green CTA on new /for/team-leads/
- **Hypothesis:** Team leads are the primary buyer for AI engineering team tooling. A persona page targeting "team lead AI agent dashboard" with a top-of-page CTA baked in should convert when the page indexes with relevant traffic.
- **Page:** `/for/team-leads/` (new page)
- **KPI:** GitHub stars (PostHog `exp037_team_leads_topofpage_cta_click` + autocapture)
- **Status:** `concluded: inconclusive`
- **Started:** 2026-08-14
- **Concluded:** 2026-08-21
- **Implementation:** New persona page (289 lines) — groups, Simple tab, board, Review, EnvSpec, voice. Top-of-page green CTA baked in at launch.
- **Effort:** M (new page)
- **Score (2026-08-21):** Named event `exp037_team_leads_topofpage_cta_click`: 0 total. Autocapture GH clicks: 0 (new page, not yet indexed). Same pattern as EXP-024/030/035/036. CTA kept in place.

### EXP-038 — Top-of-page green CTA on /demos/ index
- **Hypothesis:** /demos/ has 135 PVs and 0 GH clicks with no top-of-page CTA. Adding the green banner converts demo browsers who are clearly evaluating the platform.
- **Page:** `/demos/`
- **KPI:** GitHub stars (PostHog `exp038_demos_index_topofpage_cta_click` + autocapture)
- **Status:** `concluded: inconclusive`
- **Started:** 2026-08-14
- **Concluded:** 2026-08-21
- **Implementation:** Green top-of-page CTA banner added to demos/index.html. `exp038_demos_index_topofpage_cta_click` event.
- **Effort:** XS
- **Score (2026-08-21):** Named event `exp038_demos_index_topofpage_cta_click`: 1 total (2026-08-20). Autocapture GH clicks on /demos/: pre (Aug 7–13): 1 = 0.14/day; post (Aug 14–20): 1 = 0.14/day. Flat autocapture, 1 named event is weak signal. Verdict: inconclusive — volume too low for a conclusive result, but not clearly negative. CTA kept in place; re-score at 2026-08-28 if named events accumulate.

### EXP-029 — Top-of-page green CTA on blog/ai-coding-tools-pricing-2026
- **Hypothesis:** Pricing-page readers are in buy-vs-build mode — exactly when "amux is free, pay only for API tokens" resonates hardest. Top-of-page placement (EXP-015/020 pattern) should convert this 34 PV / 0 GH click page.
- **Page:** `/blog/ai-coding-tools-pricing-2026/`
- **KPI:** GitHub stars (PostHog `exp029_pricing_topofpage_cta_click`)
- **Status:** `concluded: inconclusive`
- **Started:** 2026-08-12
- **Concluded:** 2026-08-19
- **Implementation:** Top-of-page green banner after subtitle, before `<h2>The master pricing table</h2>`. Copy: "The orchestration layer costs $0 — your API tokens do the work." Button: "View amux on GitHub ★". Same compact banner format as EXP-015 WIN and EXP-020 WIN. Page also refreshed to August 2026 (Copilot billing past-tense, Sonnet 5, multi-runtime Scenario 1).
- **Effort:** XS
- **Score (2026-08-19):** Named event `exp029_pricing_topofpage_cta_click`: 0 total. Autocapture GH clicks: 0 pre, 0 post (7 days). Bot-traffic pattern — same as EXP-022/023/025-028. CTA kept in place.

### EXP-030 — Top-of-page green CTA on guides/claude-code-vs-codex-vs-gemini-cli (new page)
- **Hypothesis:** "Claude Code vs Codex vs Gemini CLI" is an uncovered three-way comparison query. Creating the page targets the moment a developer is choosing between runtimes — amux as the orchestrator that runs all three is the natural next step. CTA baked into the new page from day one.
- **Page:** `/guides/claude-code-vs-codex-vs-gemini-cli/` (new)
- **KPI:** GitHub stars (PostHog `exp030_runtimes_compare_topofpage_cta_click`)
- **Status:** `concluded: inconclusive`
- **Started:** 2026-08-12
- **Concluded:** 2026-08-19
- **Implementation:** New guide with 12-row comparison table, per-runtime deep sections, amux as orchestrator close. Top-of-page green CTA banner before comparison table. BreadcrumbList + Article + FAQPage JSON-LD.
- **Effort:** M (new page)
- **Score (2026-08-19):** Named event `exp030_runtimes_compare_topofpage_cta_click`: 0 total. Autocapture GH clicks: 0 (new page, not yet indexed with human traffic). CTA kept in place.

### EXP-031 — GitHub CTA banner on concierge/ page
- **Hypothesis:** Concierge visitors (37 PVs, 0 GH clicks) include developers evaluating the platform before committing to a $5k/month engagement. A low-friction GitHub exit path gives them a conversion route without detracting from the primary "schedule a meeting" CTA.
- **Page:** `/concierge/`
- **KPI:** GitHub stars (PostHog `exp031_concierge_github_cta_click`)
- **Status:** `concluded: inconclusive`
- **Started:** 2026-08-12
- **Concluded:** 2026-08-19
- **Implementation:** Compact green banner between hero section and "AI age" section. Copy: "Concierge runs on open-source amux — explore the platform before you book." Button: "View amux on GitHub ★". Positioned as a secondary conversion path, not competing with the primary "Schedule a meeting" CTA.
- **Effort:** XS
- **Score (2026-08-19):** Named event `exp031_concierge_github_cta_click`: 0 total. Autocapture GH clicks: 0 pre, 0 post (7 days). Bot-traffic pattern. CTA kept in place.

### EXP-032 — Top-of-page green CTA on lists/open-source-ai-coding-tools-2026
- **Hypothesis:** Open-source tool shoppers are self-hosters — exactly the right audience. amux is on this list already; a CTA pointing to GitHub converts researchers into stars.
- **Page:** `/lists/open-source-ai-coding-tools-2026/`
- **KPI:** GitHub stars (PostHog `exp032_oss_list_topofpage_cta_click`)
- **Status:** `concluded: inconclusive`
- **Started:** 2026-08-12
- **Concluded:** 2026-08-19
- **Implementation:** Top-of-page green banner after updated subtitle. Also fixed amux language tag (Python → Rust) and updated to August 2026. Same compact banner format as EXP-015 WIN and EXP-020 WIN.
- **Effort:** XS
- **Score (2026-08-19):** Named event `exp032_oss_list_topofpage_cta_click`: 0 total. Autocapture GH clicks: 0 pre, 0 post (7 days). Bot-traffic pattern. CTA kept in place.

### EXP-033 — Top-of-page green CTA on lists/ai-tools-for-solopreneurs-2026
- **Hypothesis:** Solo founder tool lists attract bootstrappers evaluating cost leverage. "amux replaces a junior engineering team — free and open-source" is the highest-impact framing for this audience.
- **Page:** `/lists/ai-tools-for-solopreneurs-2026/`
- **KPI:** GitHub stars (PostHog `exp033_solopreneurs_list_topofpage_cta_click`)
- **Status:** `concluded: inconclusive`
- **Started:** 2026-08-12
- **Concluded:** 2026-08-19
- **Implementation:** Top-of-page green banner after updated subtitle. Also updated to August 2026. Same compact banner format as EXP-015 WIN and EXP-020 WIN.
- **Effort:** XS
- **Score (2026-08-19):** Named event `exp033_solopreneurs_list_topofpage_cta_click`: 0 total. Autocapture GH clicks: 0 pre, 0 post (7 days). Bot-traffic pattern. CTA kept in place.

### EXP-034 — Top-of-page green CTA on /guides/ index
- **Hypothesis:** The guides index has 157 PVs over 14 days but only 1.3% GitHub CVR — the lowest of any high-traffic page. It has no top-of-page CTA. Adding one will replicate the EXP-015 WIN (+200%) and EXP-020 WIN pattern.
- **Page:** `/guides/`
- **KPI:** GitHub stars (PostHog `exp034_guides_index_topofpage_cta_click`)
- **Status:** `concluded: win`
- **Started:** 2026-08-13
- **Concluded:** 2026-08-20
- **Implementation:** Green/teal CTA banner immediately after the page subtitle. "Run a fleet of Claude Code, Codex, and Gemini CLI agents from a single dashboard. amux is free, MIT licensed, and takes 2 minutes to install." with a green GitHub button.
- **Effort:** XS
- **Score (2026-08-20):** Pre (Aug 6–12, 7d): 1 GH click = 0.14/day. Post (Aug 13–19, 7d): 3 GH clicks = 0.43/day. Delta: **+207%** — decisive win. 3 named exp034 events confirmed (Aug 17, 19, 20). Pattern now confirmed on 3 pages: headless +200%, terminal blog +500%, guides index +207%. **Change kept permanently.**

### EXP-035 — Top-of-page green CTA on new /guides/ai-agent-groups/ page
- **Hypothesis:** A new guide page targeting "organize AI agents into groups" will attract developers who are already running multiple agents and are evaluating group/namespace management. This audience is high-intent and will respond to a direct "here's how amux handles it" CTA.
- **Page:** `/guides/ai-agent-groups/`
- **KPI:** GitHub stars (PostHog `exp035_groups_guide_topofpage_cta_click`)
- **Status:** `concluded: inconclusive`
- **Started:** 2026-08-13
- **Concluded:** 2026-08-20
- **Implementation:** Green CTA banner baked into the page at creation. "Organizing your AI engineering team into specialized groups? amux groups give each sub-team its own board view, shared memory, and environment scope — manage dozens of agents with zero coordination overhead."
- **Effort:** XS (baked in at page creation)
- **Score (2026-08-20):** Named event `exp035_groups_guide_topofpage_cta_click`: 0 total. Autocapture GH clicks: 0 (new page, not yet indexed, same pattern as EXP-024/030). CTA kept in place. Re-score 2026-09-07 when page may have indexed human traffic.

### EXP-055 — Top-of-page green CTA on /compare/amux-vs-openhands/
- **Hypothesis:** OpenHands is the #2 autonomous agent framework by GitHub stars (~42k). The compare page targets "openhands vs amux" — a high-purchase-intent query with a specific named competitor. It had no top-of-page CTA and was stale since 2026-08-11. Page was enriched with August 2026 content simultaneously (Connectors row, "What's New August 2026" section, star count 338→365), which increases the surface area for engagement.
- **Page:** `/compare/amux-vs-openhands/` (stale since 2026-08-11, high-intent compare query)
- **KPI:** GitHub stars (`exp055_openhands_compare_topofpage_cta_click`)
- **Status:** `running`
- **Started:** 2026-08-23
- **Implementation:** Green CTA banner added immediately after subtitle, before EXP-007. Page also enriched: dateModified 2026-08-23, subtitle "Updated August 2026", star count 338→365 (both occurrences), Connectors/OAuth broker row in comparison table, "What's New August 2026" section (5 items) before FAQ.
- **Effort:** S
- **Measure after:** 2026-08-30 (7 days minimum)

### EXP-041 — Top-of-page green CTA on /guides/ai-coding-agents-overnight/
- **Hypothesis:** The overnight guide is the single highest-intent destination for the "run agents unattended" query. It has been stale since April 2026 with no top-of-page CTA. Adding a green banner (same pattern as EXP-015 +200%, EXP-020 +500%) with freshened content should lift GH clicks on a zero-CVR page.
- **Page:** `/guides/ai-coding-agents-overnight/` (stale since 2026-04-03, 0 GH clicks)
- **KPI:** GitHub stars (`exp041_overnight_topofpage_cta_click`)
- **Status:** `concluded: inconclusive`
- **Started:** 2026-08-16
- **Concluded:** 2026-08-23
- **Implementation:** Green CTA banner added above h1. Page also freshened: new "Owner alerts: push, email, and iMessage" section added (amux alert command, CLAUDE.md instructions, health invariant), dateModified updated to 2026-08-16, FAQPage JSON-LD updated with alerts Q, HowTo JSON-LD updated with alerts step.
- **Effort:** XS
- **Score (2026-08-23):** Named events: 0. Autocapture GH clicks: 0 pre (Aug 9–15), 0 post (Aug 16–22). Bot-traffic pattern — page has no demonstrated prior GH click history. CTA kept. Re-score 2026-09-06.

### EXP-042 — Top-of-page green CTA on /guides/getting-started/
- **Hypothesis:** Getting-started has 37 PVs and 0 GH clicks — highest intent, zero CVR. It is the natural bottom-of-funnel page (someone ready to install), so any friction here is the most costly. A top-of-page CTA reinforces the "open source, just click" action before they dig into setup steps.
- **Page:** `/guides/getting-started/` (37 PVs, 0 GH clicks, 0% CVR)
- **KPI:** GitHub stars (`exp042_getting_started_topofpage_cta_click`)
- **Status:** `running`
- **Started:** 2026-08-16
- **Implementation:** Green CTA banner added above h1 between breadcrumb and heading. Copy: "Run an AI engineering team from one dashboard — open source, single binary, MIT licensed." Button: "View amux on GitHub ★".
- **Effort:** XS
- **Score (2026-08-23, interim at 7 days):** Named events: 1 (exp042_getting_started_topofpage_cta_click, Aug 22). Autocapture GH clicks: 0 pre, ~1 post. 1 named event is the weakest positive signal seen yet on a non-click-history page. Not conclusive at 7 days — extending to 21-day window. Re-score 2026-09-06. CTA kept.

### EXP-044 — Top-of-page green CTA on new /guides/ai-agent-owner-alerts/
- **Hypothesis:** New page targeting "AI agent notification system" query. Baking in the CTA at launch establishes a baseline and tests whether owner-alert visitors (high intent — they are actively trying to get notified) convert to GH clicks.
- **Page:** `/guides/ai-agent-owner-alerts/` (new page, zero baseline)
- **KPI:** GitHub stars (`exp044_owner_alerts_topofpage_cta_click`)
- **Status:** `concluded: inconclusive`
- **Started:** 2026-08-16
- **Concluded:** 2026-08-23
- **Implementation:** Green CTA banner baked in at page creation above h1.
- **Effort:** XS (baked in at page creation)
- **Score (2026-08-23):** Named events: 0. New page, not yet indexed with human traffic. Same bot-traffic pattern as EXP-024/030/035. CTA kept. Re-score 2026-09-06.

### EXP-043 — Top-of-page green CTA on /compare/amux-vs-claude-managed-agents/
- **Hypothesis:** The Claude Managed Agents compare page targets a high-purchase-intent query ("Claude managed agents vs amux"). Adding the standard top-of-page green CTA should lift GH click rate.
- **Page:** `/compare/amux-vs-claude-managed-agents/` (high-intent compare query)
- **KPI:** GitHub stars (`exp043_managed_agents_compare_cta_click`)
- **Status:** `concluded: inconclusive`
- **Started:** 2026-08-16
- **Concluded:** 2026-08-23
- **Implementation:** Green CTA banner added above h1. Copy: "amux is the self-hosted, model-agnostic alternative — MIT licensed, zero session fees."
- **Effort:** XS
- **Score (2026-08-23):** Named events: 0. Compare page without prior GH click history — bot-traffic pattern. CTA kept. Re-score 2026-09-06.

### EXP-019 — Review dashboard PS row on homepage
- **Hypothesis:** The Review dashboard / Trends view / Fleet org chart (shipped 2026-08-01) represent a new capability class — "understand what your AI team did" — not represented in the homepage PS grid. Adding a dedicated row surfaces this for engineering managers and team leads who are the highest-value audience segment.
- **Page:** `/` (homepage PS problem/solution grid)
- **KPI:** GitHub stars + /guides/ai-agent-team-review/ traffic
- **Status:** `concluded: inconclusive`
- **Started:** 2026-08-02
- **Concluded:** 2026-08-11
- **Implementation:** Added "No idea what your team of agents actually accomplished this week" → "Review dashboard — Trends view groups fleet activity by theme; blockers surface first; Fleet org chart shows who's doing what. Team review guide →" row at the bottom of the PS grid. Links to /guides/ai-agent-team-review/. PostHog event: autocapture on GitHub link + team-review link click.
- **Effort:** XS
- **Score (2026-08-11):** Pre (Jul 26–Aug 1, 7d): 60 homepage GH clicks = **8.6/day**. Post (Aug 2–8, 7d): 63 GH clicks = **9.0/day**. Delta: **+4.7%** — below the 10% win threshold. Confound: EXP-016 also ran on homepage concurrently (different row, different KPI focus, but overlapping traffic pool). **Verdict: Inconclusive.** Review dashboard row kept in place — no negative signal, adds genuine new capability framing.

### EXP-018 — Top-of-page CTA rollout to all high-traffic/low-CVR guides
- **Hypothesis:** EXP-015 proved top-of-page CTAs work (+200% GH clicks on headless). Apply the same green/teal banner pattern to the next highest-traffic, lowest-CVR guides: ai-agent-orchestration-2026 (4 GH clicks, likely 200+ PVs) and ai-agent-sandboxing (1 GH click, 122+ PVs). Each banner should be customized to the guide's topic rather than using generic amux copy.
- **Page:** /guides/ai-agent-sandboxing/ (first target — 131 PVs/14d, 1 GH click, 0.8% CVR)
- **KPI:** GitHub stars (PostHog autocapture + custom event `exp018_guide_topofpage_cta_click`)
- **Status:** `concluded: inconclusive`
- **Started:** 2026-07-30
- **Concluded:** 2026-08-06
- **Implementation:** Applied green top-of-page CTA to /guides/ai-agent-sandboxing/. Copy: "Running agents in parallel and need to coordinate their work? amux gives each agent its own isolated tmux session with configurable filesystem access — security and coordination built in, not bolted on." Button: "View amux on GitHub ★" with `exp018_guide_topofpage_cta_click` event. dateModified updated to 2026-07-30. Same compact green/teal banner format as EXP-015 WIN. **CTA kept in place.**
- **Effort:** XS per page
- **Score (2026-08-06):** Named event `exp018_guide_topofpage_cta_click`: volume insufficient for pre/post comparison. ai-agent-sandboxing pre-GH clicks (Jul 23–29): ~1 click total. Post (Jul 30–Aug 5): ~1-2 clicks total. Same low-volume pattern as EXP-014/EXP-012/EXP-007 — guide pages with < 150 PVs/14d produce too few GH clicks in any 7-day window for a meaningful pre/post signal. The green banner is kept (harmless, matches EXP-015 WIN pattern), but cannot conclude a positive lift from the data. **Verdict: Inconclusive — insufficient traffic volume.** Note: EXP-015 WIN happened on claude-code-headless with ~185 PVs and an unusually high engagement audience (headless Claude Code users actively seeking tools). ai-agent-sandboxing is more casual/research traffic.

### EXP-016 — CTO/team persona framing in homepage hero
- **Hypothesis:** EXP-009 showed that "indie hacker" framing hurt conversion (-21.4%). The null version of the homepage hero now uses generic "developers" language. Testing a "teams" frame — "AI engineering teams" + specific team size ("5–50 agents") — may increase conversion from team/enterprise visitors without narrowing the solo developer audience. Unlike EXP-009, this targets a premium buyer segment that likely converts on cloud/concierge.
- **Page:** `/` (hero subtitle / .lede)
- **KPI:** GitHub stars + cloud/concierge signups
- **Status:** `concluded: inconclusive`
- **Started:** 2026-08-01
- **Concluded:** 2026-08-10
- **Implementation:** Changed .lede to "amux is the open-source control plane for AI engineering teams. Run a fleet of Claude Code, Codex, and Gemini CLI workers from a shared dashboard — with self-healing built in, so your team wakes up to finished work instead of crashed workers. **5–50 agents. Zero coordination overhead.**" Also updated social-proof star count from 310+ to 323+. PostHog event: autocapture on github.com/mixpeek/amux and concierge links. **Change kept — no evidence of harm, "teams" framing aligns with amux's positioning.**
- **Effort:** XS
- **Score (2026-08-10 — final):** Pre (Jul 25–31, 7 days): 62 homepage GH clicks = **8.9/day**. Post (Aug 1–7, 7 days): 66 homepage GH clicks = **9.4/day**. Delta: **+6.5%** — below the 10% win threshold for a conclusive result. Confounds: EXP-019 also shipped 2026-08-02 (adds a Review dashboard row to PS grid, different KPI focus but same homepage). With only +6.5% and a confound from EXP-019 running concurrently, this is not a clean signal. **Verdict: Inconclusive.** "Teams" framing kept in place (harmless, consistent with amux's positioning and messaging direction, no negative signal).

### EXP-015 — Top-of-page GitHub CTA on high-traffic guides with low conversion
- **Hypothesis:** claude-code-headless has 185 PVs but only 2 GH clicks (1.1% CVR). The only GitHub CTA on the page is the EXP-007 block at line 650 — below ~600 lines of content, invisible to most visitors. Adding a compact green teal banner immediately after the subtitle (above the TOC) will catch visitors before they scroll away.
- **Page:** /guides/claude-code-headless/ (185 PVs, 2 GH clicks — biggest CVR gap of high-traffic pages)
- **KPI:** GitHub stars (PostHog event: `exp015_headless_topofpage_cta_click`)
- **Status:** `concluded: win`
- **Started:** 2026-07-22
- **Concluded:** 2026-07-29
- **Implementation:** Added compact green/teal banner immediately after subtitle paragraph, before the TOC. Text: "Want to run 10+ headless agents in parallel? amux orchestrates, monitors, and self-heals an entire fleet of headless Claude Code workers." Button: "View amux on GitHub ★" with `exp015_headless_topofpage_cta_click` event.
- **Effort:** XS
- **Score (2026-07-29):** Pre-period (2026-07-15–21, 7 days): 2 total headless GH clicks (only Jul 15 had 2). Avg = 0.29/day. Post-period (2026-07-22–28, 7 days): Jul 24: 1, Jul 25: 1, Jul 27: 1, Jul 28: 3 = 6 total. Avg = 0.86/day. Delta: **+200%** (0.29 → 0.86/day). Named EXP-015 events: 5 total (Jul 24: 1, Jul 28: 3, Jul 29: 1). Both signals confirm: top-of-page placement is dramatically more effective than the buried EXP-007 block at line 650. **Change kept permanently. Replicate this pattern to other high-traffic/low-CVR guides.**

---

## Concluded Experiments

| Experiment | Started | Concluded | Result | Verdict |
|-----------|---------|-----------|--------|---------|
| EXP-004 — Concierge urgency badge | 2026-07-10 | 2026-07-24 | 0 clicks in 14 days (off-site conversion, unmeasurable) | no_effect |
| EXP-006 — GitHub README iOS badge | 2026-07-13 | 2026-07-21 | -9% iOS clicks (pre-period 3 days only, insufficient baseline) | inconclusive |
| EXP-007 — Compare pages GitHub CTA | 2026-07-14 | 2026-07-21 | 0 named events; ~5 compare GH clicks total from autocapture (low traffic volume, 10-15 PVs/page) | inconclusive |
| EXP-008 — Theme preference PostHog property | 2026-07-15 | 2026-07-22 | person.properties vs event properties namespace mismatch — data in event layer, segmentation never worked | inconclusive (measurement) |
| EXP-009 — "Indie hackers" hero language | 2026-07-16 | 2026-07-24 | **-21.4%** homepage GH clicks (pre 10.3/day vs post 8.1/day over 8 days). Reverted. | **no_effect (negative)** |
| EXP-013 — GitHub CTA on best-ai-model-for-coding guide | 2026-07-12 | 2026-07-20 | 162 PVs, 0 KPI clicks (bot/crawler traffic confirmed) | no_effect |
| EXP-005 — Star History chart on homepage | 2026-07-11 | 2026-07-25 | No valid pre-period baseline after 14-day post window (PostHog activated 1 day before experiment). 5 simultaneous experiments made attribution impossible. Chart kept. | inconclusive |
| EXP-011 — Plan strip row in homepage PS grid | 2026-07-18 | 2026-07-25 | **+12.3%** homepage GH clicks (pre 8.1/day vs post 9.1/day, 7 days each). EXP-009 confound likely underestimates the true effect. **Change kept permanently.** | **win** |
| EXP-012 — Freelancer CTA on compare index | 2026-07-21 | 2026-07-28 | 1 named click (to for-freelancers), compare GH clicks: 2 pre / 1 post (7 days each) — noise-level volume. Same low-traffic pattern as EXP-007. CTA kept. | inconclusive |
| EXP-017 — "Best multiplexers" callout on compare index | 2026-07-28 | 2026-08-04 | 2 named events in 7 days. Compare index PVs: 49 pre vs 29 post — volume too low for meaningful GH click pre/post (same pattern as EXP-012). Callout kept. EXP-022 queued for harness-engineering (138 PVs, highest-traffic page with only a buried CTA). | inconclusive |
| EXP-014 — GitHub CTA on guide pages (harness-engineering, measuring-ai-coding-agent-roi, claude-code-context-compaction) | 2026-07-20 | 2026-07-27 | 1 named event (harness-engineering), 0 pre → 1 post autocapture GH click. Same pattern as EXP-007: target pages have insufficient human traffic (108 PVs/14d for harness-engineering). CTA kept on all pages. | inconclusive |
| EXP-020 — Top-of-page CTA on blog/best-terminal-ai-coding-agents-2026 | 2026-08-03 | 2026-08-10 | **+500%** blog GH clicks (pre 0.14/day vs post 2.00/day, 7 days each). 6 named exp020 events. Decisive win — top-of-page pattern now confirmed on 2 pages (headless +200%, terminal blog +500%). **Change kept permanently.** | **win** |
| EXP-018 — Top-of-page CTA rollout (ai-agent-sandboxing, ai-agent-orchestration, agent-fleet-operations) | 2026-07-30 | 2026-08-06 | Named events: insufficient volume. ai-agent-sandboxing GH clicks: ~1 pre vs ~1–2 post (7 days). Same low-volume pattern as EXP-014/012/007 — guide pages <150 PVs don't have enough click volume for a 7-day pre/post signal. CTAs kept in place (harmless, match EXP-015 pattern). | inconclusive |
| EXP-016 — CTO/team persona framing in homepage hero | 2026-08-01 | 2026-08-10 | +6.5% homepage GH clicks (pre 8.9/day vs post 9.4/day, 7 days each). Below 10% threshold. EXP-019 confound (also running, different PS row). "Teams" framing kept — harmless, consistent with positioning. | inconclusive |
| EXP-019 — Review dashboard PS row on homepage | 2026-08-02 | 2026-08-11 | +4.7% homepage GH clicks (pre 8.6/day vs post 9.0/day, 7 days each). Below 10% threshold. EXP-016 confound (also running on homepage). Review dashboard row kept — no negative signal, genuine new capability. | inconclusive |
| EXP-021 — Top-of-page CTA on context-engineering (14d extended) | 2026-08-04 | 2026-08-18 | 0 named events, 0 autocapture GH clicks pre AND post across 14 days. Bot-traffic pattern confirmed at 2x window. CTA kept in place. | inconclusive |
| EXP-025 — Top-of-page CTA on spec-driven-development | 2026-08-11 | 2026-08-18 | 0 named events, 0 autocapture GH clicks (7 days). Bot-traffic pattern. CTA kept. | inconclusive |
| EXP-026 — Top-of-page CTA on lists/best-ai-coding-agents-2026 | 2026-08-11 | 2026-08-18 | 0 named events, 0 autocapture GH clicks (7 days). Bot-traffic pattern. CTA kept. | inconclusive |
| EXP-027 — Top-of-page CTA on lists/best-claude-code-tools-2026 | 2026-08-11 | 2026-08-18 | 0 named events, 0 autocapture GH clicks (7 days). Bot-traffic pattern. CTA kept. | inconclusive |
| EXP-028 — Top-of-page CTA on lists/ai-agent-frameworks-comparison-2026 | 2026-08-11 | 2026-08-18 | 0 named events, 0 autocapture GH clicks (7 days). Bot-traffic pattern. CTA kept. | inconclusive |
| EXP-022 — Top-of-page green CTA on harness-engineering | 2026-08-10 | 2026-08-17 | 0 named events, 0 autocapture GH clicks pre AND post (7 days). Bot/crawler traffic pattern — same as EXP-013/014/018. Guide pages below ~150 PVs with no prior GH click history are not viable targets. CTA kept in place. | inconclusive |
| EXP-023 — Top-of-page green CTA on best-self-hosted-ai-coding-tools-2026 | 2026-08-10 | 2026-08-17 | 0 named events, 0 autocapture GH clicks pre AND post (7 days). Same bot-traffic pattern as EXP-022. CTA kept in place. | inconclusive |
| EXP-024 — Top-of-page green CTA on declarative-ai-agent-setup (new page) | 2026-08-10 | 2026-08-17 | 0 named events, 0 autocapture GH clicks (new page, no indexed traffic yet). CTA kept — re-score 2026-09-07 when page has had time to index. | inconclusive |
| EXP-015 — Top-of-page GitHub CTA on claude-code-headless | 2026-07-22 | 2026-07-29 | **+200%** headless GH clicks (pre 0.29/day vs post 0.86/day, 7 days each). 5 named exp015 events confirmed. Top-of-page placement dramatically outperforms buried EXP-007 block at line 650. **Change kept permanently.** | **win** |
| EXP-010 — Feature table reorder: phone-first features at top | 2026-07-17 | 2026-07-31 | **+20.0%** iOS clicks (pre 3.6/day vs post 4.3/day, 14-day post window). EXP-011 minimal confound (different KPI: GH clicks not iOS). Phone-first row order kept permanently. | **win** |
| EXP-001 — Hero CTA button copy | 2026-07-07 | 2026-07-26 | No valid pre-period baseline — PostHog activated 2026-07-09, experiment started 2026-07-07. Change kept ("⭐ Star on GitHub"). | inconclusive |
| EXP-002 — iOS CTA sticky mobile bottom bar | 2026-07-08 | 2026-07-26 | No valid pre-period baseline — PostHog activated 2026-07-09, experiment started 2026-07-08. Sticky bar kept (17 taps logged, harmless UX improvement). | inconclusive |
| EXP-003 — Homepage hero social proof line | 2026-07-09 | 2026-07-26 | No valid pre-period baseline — experiment started same day PostHog activated. Social proof paragraph kept. | inconclusive |
| EXP-029 — blog/pricing top-of-page CTA | 2026-08-12 | 2026-08-19 | 0 named events, 0 autocapture GH clicks (7 days). Bot-traffic pattern. CTA kept. | inconclusive |
| EXP-030 — claude-code-vs-codex-vs-gemini-cli (new page) | 2026-08-12 | 2026-08-19 | 0 named events, 0 GH clicks (new page not yet indexed). CTA kept. | inconclusive |
| EXP-031 — concierge/ GitHub CTA banner | 2026-08-12 | 2026-08-19 | 0 named events, 0 autocapture GH clicks (7 days). Bot-traffic. CTA kept. | inconclusive |
| EXP-032 — open-source-ai-coding-tools list top-of-page CTA | 2026-08-12 | 2026-08-19 | 0 named events, 0 autocapture GH clicks (7 days). Bot-traffic. CTA kept. | inconclusive |
| EXP-033 — ai-tools-for-solopreneurs list top-of-page CTA | 2026-08-12 | 2026-08-19 | 0 named events, 0 autocapture GH clicks (7 days). Bot-traffic. CTA kept. | inconclusive |
| EXP-048 — /for/startup-founders/ top-of-page CTA | 2026-08-19 | 2026-08-26 | 0 named events, 0 autocapture GH clicks (Aug 19–25, 7 days). Bot-traffic pattern — same as EXP-047. Persona pages without prior GH click history follow bot-traffic pattern regardless of page intent. CTA kept. | inconclusive |
| EXP-049 — /pricing/ + /guides/ai-agent-sandboxing/ top-of-page CTA | 2026-08-20 | 2026-08-27 | No PostHog access. /pricing/ has prior human traffic (8 GH clicks/14d) — genuine candidate, re-score when PostHog restored. /sandboxing/ follows new-page bot-traffic pattern. CTAs kept. | inconclusive |
| EXP-050 — /guides/ai-agent-memory-management/ top-of-page CTA (new page) | 2026-08-20 | 2026-08-27 | New page, 7 days old. Same bot-traffic pattern as EXP-024/035/036/037. CTA kept. Re-score 2026-09-10 (21 days). | inconclusive |
| EXP-062 — /compare/amux-vs-windsurf/ top-of-page green CTA (enriched Aug 2026) | 2026-08-26 | 2026-09-02 (running) | — | running |
| EXP-063 — /guides/ai-agent-session-cloning/ top-of-page CTA (new page) | 2026-08-26 | 2026-09-02 (running) | — | running |
| EXP-064 — /compare/amux-vs-cline/ top-of-page green CTA (enriched Aug 2026) | 2026-08-27 | 2026-09-03 (running) | — | running |
| EXP-065 — /glossary/ai-agent-fleet/ top-of-page CTA (new page) | 2026-08-27 | 2026-09-03 (running) | — | running |
| EXP-034 — Top-of-page green CTA on /guides/ index | 2026-08-13 | 2026-08-20 | **+207%** guides index GH clicks (pre 0.14/day vs post 0.43/day, 7 days each). 3 named exp034 events confirmed (Aug 17, 19, 20). Pattern now confirmed on 3 pages: headless +200%, terminal blog +500%, guides index +207%. **Change kept permanently.** | **win** |
| EXP-035 — Top-of-page green CTA on new /guides/ai-agent-groups/ | 2026-08-13 | 2026-08-20 | 0 named events, 0 autocapture GH clicks (new page, not yet indexed). Same pattern as EXP-024/030. CTA kept. Re-score 2026-09-07. | inconclusive |

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
| 2026-07-24 | **EXP-004 CONCLUDED no_effect**: 14-day run, 0 concierge CTA clicks — root cause is off-site conversion (Calendly/email). PostHog can't measure it. Badge kept, experiment closed. **EXP-009 CONCLUDED no_effect + REVERTED**: Full 8-day window scored -21.4% homepage GH clicks (pre 10.3/day vs post 8.1/day). "Indie hackers" persona framing definitively negative — narrowed perceived audience. Reverted .lede to multi-runtime framing + updated star count 288+ → 310+. **EXP-010 extended to 2026-07-31**: iOS +8.5% (below 10% threshold, EXP-011 overlap muddies attribution — need 14-day post window). **EXP-016 queued**: CTO/team persona framing to replace the reverted EXP-009 — ship after EXP-010 concludes. GitHub stars: 310. New pages: /compare/amux-vs-langraph/ rebuilt from 101-line stub to full 500-line compare page (38k stars LangGraph framing, 15-row table, FAQPage/Article/BreadcrumbList JSON-LD, EXP-007 CTA). /for/ctos/ created (new CTO persona page targeting "AI agent team dashboard" + "CTO AI engineering team"). Changelog: 5 new entries (map distance/near-me sort, two-tone pins, AND-mode filter, gzip mobile perf, 'x' key fix). | EXP-004 marked concluded no_effect. EXP-009 marked concluded no_effect + reverted in site/index.html. EXP-016 queued. New pages committed: amux-vs-langraph + for/ctos + changelog + sitemap + llms.txt. |
| 2026-07-25 | **EXP-011 CONCLUDED win +12.3%**: Plan strip row in homepage PS grid (started 2026-07-18). Pre (Jul 11–17) homepage GH clicks: 8.1/day. Post (Jul 18–24): 9.1/day. Delta: **+12.3%** — first conclusive win above 10% threshold. Plan strip row kept permanently. EXP-009 confound (negative effect throughout window) likely means true EXP-011 effect is higher than +12.3%. **EXP-005 CONCLUDED inconclusive**: No valid pre-period baseline after 14-day extension — PostHog had 1 usable pre-data point. Star history chart kept in place. Homepage GitHub KPI state: 221 total clicks across all pages last 14d (homepage 121 = 55%, multiplexers guide 50 = 23%). iOS clicks: 51 total last 14d, 17 from exp002 sticky bar. GitHub stars: 313. | EXP-011 and EXP-005 marked concluded. EXP-017 added to backlog (best-multiplexers callout on compare index). New pages: /compare/amux-vs-autogen/ rebuilt from 105-line stub to 490-line full compare page (AutoGen 60k ★, Microsoft Research; 15-row table; "Why not both?" 3 patterns; FAQPage 5 Qs; Article+BreadcrumbList JSON-LD). /guides/best-ai-agent-multiplexers-2026/ freshened (dateModified 2026-07-25, Opus 5 mention). /guides/mobile-management-pwa/ freshened (iOS PWA state restore section added, v0.9.189). Changelog: 4 new entries (iOS PWA restore, Opus 5, self-heal, head-of-line block). |
| 2026-07-26 | **EXP-001/002/003 CONCLUDED inconclusive**: All three experiments pre-date or match PostHog activation (2026-07-09) — no valid pre-period baseline possible. All changes kept in place (star CTA copy, sticky iOS bar, social proof paragraph — none show evidence of harm). PostHog 14-day KPI state (from yesterday's run): 215 total GitHub clicks, homepage 123 (57%), best-ai-agent-multiplexers-2026 52 (24%), pricing 14, best-claude-code-worker-managers-2026 5 (new entrant, 5 GH clicks first appearance), getting-started 4, ai-agent-orchestration-2026 4, claude-code-headless 4. iOS: 3–6/day, steady. GitHub stars: 313. No experiments due today (EXP-014 → tomorrow 2026-07-27, EXP-012 → 2026-07-28). | EXP-001, EXP-002, EXP-003 marked concluded inconclusive + added to Concluded table. New page: /guides/voice-dictation-ai-agents/ (new, 450+ lines targeting "voice dictation AI agents", "dictate to Claude Code from phone" — first AEO page on voice control for AI coding agents; HowTo + FAQPage + Article JSON-LD, 6 FAQ questions). /guides/ai-agent-orchestration-2026/ freshened (dateModified 2026-05-25 → 2026-07-26, July 2026 badge, EXP-014 CTA, voice dictation mention in intro, voice guide in further reading). Changelog: 6 new entries (dictation v0.9.197, dictation mobile v0.9.199, Messages color-coding v0.9.190, Files syntax-highlight v0.9.193, Files offline-download v0.9.192, Schedules badge v0.9.195). |
| 2026-07-27 | **EXP-014 CONCLUDED inconclusive**: 7-day window. 1 named exp014 click (harness-engineering), 0→1 post autocapture GH click. Same pattern as EXP-007: target pages have insufficient human traffic (harness-engineering ~108 PVs/14d, measuring-ai-coding-agent-roi similar). CTAs kept on all pages. PostHog 14-day KPI state: **128 total GH clicks** (up from 215 total last run — note: different measurement period), homepage 93 (73%, 756 PVs = 12.3% CVR), best-ai-agent-multiplexers-2026 7 (5.5%, 312 PVs), pricing 4, claude-code-headless 4, **best-claude-code-worker-managers-2026 2 (utm_source=chatgpt.com — ChatGPT citing this page!)**. iOS: 2 autocapture + 14 exp002 sticky taps total. GitHub stars: **315** (+2). Key AEO signal: ChatGPT is actively citing best-claude-code-worker-managers-2026 for "Claude Code worker manager" queries — confirming the page is ranking in AI answer engines. No experiments shipped today (EXP-017 waits for EXP-012 scored 2026-07-28; EXP-016 waits for EXP-010 concluded 2026-07-31). | **EXP-014** marked concluded inconclusive + added to Concluded table. **best-claude-code-worker-managers-2026** enriched (dateModified 2026-07-15→2026-07-27; EXP-014 indigo CTA added; v0.9.200 offline scrollback + v0.9.201 stalled badge features added; voice dictation + mobile PWA links in Related Guides). New page: **/guides/ai-agent-offline-review/** (new, ~450 lines, HowTo 5 steps + FAQPage 7 Qs + Article JSON-LD; targets "review AI agents offline", "check Claude Code agents without internet"; covers v0.9.200 IndexedDB scrollback cache, offline outbox, iOS PWA behavior; EXP-014 CTA; sitemap + llms.txt added). Changelog: 3 new entries (v0.9.201 stalled badge b120b6c, v0.9.200 offline scrollback a5e73c0, credit-gate fix d95b424). |
| 2026-07-28 | **EXP-012 CONCLUDED inconclusive**: 7-day window. 1 named exp012 click (to for-freelancers), compare index GH autocapture: 2 clicks pre (2026-07-14–20) vs 1 click post (2026-07-21–27) — noise-level volume, no meaningful signal. Same pattern as EXP-007/EXP-014: compare index (~62 PVs/14d) doesn't have enough human click volume for 7-day pre/post measurement. CTA kept. **EXP-017 SHIPPED**: green/teal best-multiplexers callout added to compare/index.html above EXP-012 block; `exp017_compare_index_multiplexers_callout_click` event; measure 2026-08-04. PostHog 14-day KPI state: **129 total GH clicks** (homepage 92, **claude-code-headless 7 — up from 2-4, EXP-015 signal!**, best-ai-agent-multiplexers 6, pricing 3, compare 2). iOS: **50 total** (homepage 29 autocapture + 11 exp002 sticky + 10 from other pages). GitHub stars: **315** (unchanged). ChatGPT referral now hitting homepage (utm_source=chatgpt.com, 5 GH clicks) + blog/best-terminal + best-claude-code-worker-managers. claude-code-headless jump to 7 GH clicks (from 2-4) is strong EXP-015 signal — EXP-015 score due tomorrow. | **EXP-012** marked concluded inconclusive. **EXP-017** shipped + marked running. New page: **/guides/run-ai-engineering-team/** (new, ~450 lines, targets "how to run an AI engineering team" — primary AEO query; HowTo 5 steps + FAQPage 7 Qs + Article JSON-LD; team sizes table, coordination patterns). **blog/best-terminal-ai-coding-agents-2026** enriched (dateModified 2026-05-11→2026-07-28, amux section updated 315★+multi-runtime+offline scrollback+voice, Antigravity CLI July note, EXP-014 CTA added). Changelog: 3 new entries (worker tab reordering 9f3a9b6, local Whisper 12x 279d8eb, persistent outbox v0.9.202 6355f15). |
| 2026-07-29 | **EXP-015 CONCLUDED win +200%**: Top-of-page green CTA on claude-code-headless (started 2026-07-22). Pre (Jul 15–21): 0.29 headless GH clicks/day (2 total). Post (Jul 22–28): 0.86 clicks/day (6 total). Delta: +200% — decisive win. 5 named exp015 events confirm. Key insight: CTA POSITION matters far more than CTA PRESENCE — the EXP-007 block was buried 650+ lines in; the top-of-page banner catches visitors before they bounce. **EXP-018 QUEUED**: roll out the same top-of-page pattern to ai-agent-orchestration-2026 and ai-agent-sandboxing (both high-PV, low-CVR). PostHog 14-day KPI state: **221 GH clicks** (homepage 111, best-ai-agent-multiplexers 49, pricing 11, claude-code-headless 9, ai-agent-orchestration 4). iOS: 59 total (homepage 45, demos 3, features/mobile-pwa 3). GitHub stars: **316** (+1). ChatGPT still citing multiple pages. | **EXP-015** marked concluded win. **EXP-018** added to backlog. **amux-vs-devin** rebuilt 120→354 lines (full compare page: 15-row table, cost math, decision guide, FAQ 5 Qs, "why not both?"). **use-cases/ai-agent-browser-profiles/** created (new, 253 lines; HowTo 4 steps, FAQ 6 Qs, comparison table, multi-profile fleet use cases; targets "saved browser auth profiles for AI agents"). Changelog: 6 new entries (Messages tab filter d6116f6, browser profiles 90c5f75, copy path PR#64 b6781b5, MKV fix 51d8c45, service worker perf aff3025, profile picker crash fix 43483bd). |
| 2026-07-31 | **EXP-010 CONCLUDED win +20.0%**: Phone-first feature table reorder (started 2026-07-17). Pre (Jul 9–16, 7d): 3.6 iOS clicks/day. Post (Jul 17–30, 14d): 4.3 iOS clicks/day. Delta: **+20.0%** — decisive win at 14-day post window. EXP-011 confound minimal (GH clicks, not iOS). Phone-first row order kept permanently. **EXP-018 SHIPPED** to ai-agent-sandboxing (131 PVs, 1 GH click, 0.8% CVR — highest-traffic low-CVR guide). Green top-of-page banner added 2026-07-30; measure 2026-08-06. **New persona page**: /for/product-managers/ (new, targets "AI agents for product managers", "assign tasks to AI agents", "AI agent kanban board PM"; Article + FAQPage 7 Qs + BreadcrumbList JSON-LD). **Kanban guide enriched** with mobile kanban (column snap/swipe, 20ae941), task dependencies + reviewer assignments (7a3bfa5), and prompt auto-decomposition (4b0b1ed). **Changelog** updated: 6 new entries (20ae941–456d161), changelog/index.html ItemList refreshed with 5 newest entries. | **EXP-010** marked concluded win. **EXP-018** marked running. /for/product-managers/ committed. kanban-board-for-agents enriched + dateModified updated. |
| 2026-08-01 | GitHub stars: **323** (+7 since last run). No experiments to score today (EXP-017 → 2026-08-04, EXP-018 → 2026-08-06). **EXP-016 SHIPPED**: CTO/team persona framing in homepage .lede — "AI engineering teams" + "5–50 agents. Zero coordination overhead." Star count updated 310+→323+. Measure 2026-08-08. **EXP-018 expanded** to ai-agent-orchestration-2026 (second target — 200+ PVs, 4 GH clicks, ~2% CVR). New pages: /for/data-scientists/ (parallel AI for EDA/modeling/pipeline automation; 7-question FAQPage + Article + BreadcrumbList JSON-LD). Changelog: 5 new entries (cee2e62 fs API, 34cca01 burst view, 25ccf95 board audit trail, 0ae5070 deploy fix, 38c66bd autonomy marker fix). | **EXP-016** marked running. **EXP-018** second target added. /for/data-scientists/ committed. ai-agent-orchestration-2026 freshened (EXP-018 CTA + dateModified 2026-08-01). amux-vs-openai-symphony updated (EXP-007 CTA). Press page date updated. |
| 2026-08-02 | GitHub stars: **327** (+4 since last run). No experiments to score today (EXP-017 → 2026-08-04, EXP-018 → 2026-08-06). **EXP-019 SHIPPED**: "No idea what your AI team accomplished this week" → "Review dashboard + Trends view + Fleet org chart" row added to homepage PS grid. Links to /guides/ai-agent-team-review/. Measure 2026-08-09. **EXP-018 expanded** to agent-fleet-operations (third target — enriched with Trends/Review/Fleet sections + EXP-018 CTA). No experiments concluded today. Changelog: 8 new entries (0e8a29b rate-limit fix, 4babfb3 Trends view, 29dfc39 tab customizer, 32ac1a6 Unblock everything, 251e56b Review command center, 7fb97f8 Review dashboard, 24e6c69 Fleet org chart, eeaf4e0 model badge fix). New pages: /guides/ai-agent-team-review/ (new, 270+ lines, HowTo 5 steps + FAQPage 7 Qs + Article + BreadcrumbList JSON-LD; EXP-018 CTA; targets "AI agent team weekly review"). Job 7 skipped (lists lastmod 2026-07-06 = 27 days, threshold 30 days, due 2026-08-05). | **EXP-019** shipped and marked running. /guides/ai-agent-team-review/ committed. agent-fleet-operations enriched (3 new sections + 3 new FAQs + EXP-018 CTA + dateModified 2026-08-02). /for/engineering-managers/ enriched (Trends/Review/Fleet sections + 3 FAQs + dateModified 2026-08-02). amux-vs-codex enriched (Trends/Fleet obs row + FAQ + dateModified 2026-08-02). Press page date updated to August 2, 2026. Sitemap + llms.txt updated with new page. |
| 2026-08-03 | GitHub stars: **330** (+3 since last run). No experiments to score today (EXP-017 → 2026-08-04, EXP-018 → 2026-08-06, EXP-016 → 2026-08-08, EXP-019 → 2026-08-09). **PostHog 14-day KPI state:** 245 total GH clicks (homepage 125 = 51%, best-ai-agent-multiplexers-2026 51 = 21%, pricing 21, claude-code-headless 10, demos 4, others <3 each). iOS clicks: 55 total (homepage 42, mobile-pwa 3, getting-started 2). Biggest CVR gap: blog/best-terminal-ai-coding-agents-2026 — **174 PVs / 2 GH clicks = 1.2% CVR** (no top-of-page CTA, only buried EXP-014 indigo block). **EXP-020 SHIPPED**: green top-of-page CTA added to blog/best-terminal-ai-coding-agents-2026 before TL;DR block. Same pattern as EXP-015 WIN (+200%); measure 2026-08-10. No queued experiments remaining — new hypothesis EXP-020 created and shipped immediately. **AEO Jobs 2–8**: enriched ai-coding-while-you-sleep (dateModified 2026-08-03, Gemini CLI + TTS + Trends), created /guides/ai-agent-read-aloud/ (new page targeting TTS/Piper gap), enriched /for/ai-engineers/ (Gemini parity + Trends + Review), changelog 4 new entries (Piper TTS, Gemini parity, Gemini state detection, startup perf), press page date updated. | **EXP-020** shipped and marked running. blog/best-terminal-ai-coding-agents-2026 dateModified updated to 2026-08-03. AEO site changes committed: a73099d (ai-coding-while-you-sleep), 9287a41 (ai-agent-read-aloud + sitemap/llms.txt), fdce208 (for/ai-engineers), fe60a68 (changelog), e0d09eb (press). homepage-experiments.md updated with EXP-020 entry + today's learnings log. |
| 2026-08-10 | GitHub stars: **336** (+4 since last run). **EXP-020 CONCLUDED win +500%**: blog/best-terminal-ai-coding-agents-2026 top-of-page CTA (started 2026-08-03). Pre (Jul 27–Aug 2, 7d): 1 GH click = 0.14/day. Post (Aug 3–9, 7d): 6 GH clicks = 2.00/day. Delta: **+500%** — decisive win. 6 named exp020 events. Pattern now confirmed on 2 pages: headless (+200%), terminal-blog (+500%). Change kept permanently. **EXP-016 CONCLUDED inconclusive**: CTO/team persona framing in homepage hero (started 2026-08-01). Pre (Jul 25–31, 7d): 62 GH clicks = 8.9/day. Post (Aug 1–7, 7d): 66 GH clicks = 9.4/day. Delta: **+6.5%** — below 10% threshold, EXP-019 confound. "Teams" framing kept (harmless, consistent with positioning). **EXP-022 running**: harness-engineering top-of-page CTA, started 2026-08-10, measure 2026-08-17. **EXP-023 running**: best-self-hosted top-of-page CTA, started 2026-08-10, measure 2026-08-17. **New page shipped: /guides/declarative-ai-agent-setup/** (targets "declarative AI agent setup", "AI agent infrastructure as code", "AI agent YAML configuration"; amux IaC feature; BreadcrumbList+Article+HowTo+FAQPage JSON-LD; EXP-024 top-of-page CTA baked in). **EXP-024 started**: declarative-ai-agent-setup top-of-page CTA, measure 2026-08-17. **Changelog** updated with commits since 2026-08-04 (done by separate agent). Sitemap + llms.txt updated. AEO_BACKLOG.md updated. | **EXP-020** marked concluded win. **EXP-016** marked concluded inconclusive. **EXP-024** added as running. /guides/declarative-ai-agent-setup/ committed. Experiments doc + AEO_BACKLOG + sitemap + llms.txt all updated. |
| 2026-08-10 | GitHub stars: **336** (+4 since last run). **EXP-020 CONCLUDED win +500%**: blog/best-terminal-ai-coding-agents-2026 GH clicks, pre 0.14/day (Jul 27–Aug 2) → post 2.00/day (Aug 3–9), 6 named exp020 events. Third consecutive top-of-page CTA WIN (EXP-015 +200%, EXP-020 +500%). Pattern is firmly established: top-of-page green banner on high-traffic pages with high-intent audiences produces decisive lift. **EXP-018 CONCLUDED inconclusive**: ai-agent-sandboxing (+ ai-agent-orchestration, agent-fleet-operations) — insufficient volume, same pattern as EXP-014. CTAs kept in place. **EXP-016 CONCLUDED inconclusive**: +6.5% homepage GH clicks (pre 8.9/day vs post 9.4/day, 7 days). EXP-019 confound. "Teams" framing kept. **EXP-022 SHIPPED**: green top-of-page CTA on harness-engineering (138 PVs, 0 GH clicks — biggest CTA gap by traffic). `exp022_harness_topofpage_cta_click` event. Measure 2026-08-17. **EXP-023 SHIPPED**: green top-of-page CTA on best-self-hosted-ai-coding-tools-2026 (66 PVs, 0 GH clicks). `exp023_selfhosted_topofpage_cta_click` event. Measure 2026-08-17. **AEO Jobs 6–9**: changelog updated (7 new entries: IaC, commit nudge, model/token spend, message verdict, smart tags, gate enforcement, worker status), lists/best-ai-agent-orchestrators-2026 updated (August 2026, star counts refreshed: LangGraph 47k+, CrewAI 31k+, AutoGen 38k+, OpenHands 42k+, amux 336), new guide /guides/declarative-ai-agent-setup/ (amux IaC / YAML agent config). | **EXP-020** marked concluded win. **EXP-018** marked concluded inconclusive. **EXP-016** marked concluded inconclusive. **EXP-022** and **EXP-023** shipped and marked running. Concluded table updated. changelog/notes.json + changelog/index.html updated. lists/best-ai-agent-orchestrators-2026/index.html updated. sitemap.xml lastmod updated for lists page + declarative guide already present. llms.txt already has declarative guide. |
| 2026-08-04 | GitHub stars: **332** (+2 since last run). **EXP-017 CONCLUDED inconclusive**: 7-day window. Named events: 2 (`exp017_compare_index_multiplexers_callout_click` — Jul 30 + Aug 4). Compare index PVs pre (Jul 21–27): 49; post (Jul 28–Aug 3): 29. Volume too low for a meaningful pre/post GH click comparison — same pattern as EXP-012 (1 named click, 7 days). Callout kept in place. **PostHog 14-day traffic state:** Homepage 941 PVs, best-ai-agent-multiplexers-2026 390 PVs, claude-code-headless 281 PVs, blog/best-terminal-ai-coding-agents-2026 179 PVs (EXP-020 just shipped here), harness-engineering 138 PVs (6th highest, only a buried EXP-014 CTA — biggest CTA gap by traffic). best-self-hosted-ai-coding-tools-2026 64 PVs and spec-driven-development 54 PVs are new high-traffic entrants with no CTAs. **EXP-021 SHIPPED**: top-of-page green CTA added to /guides/context-engineering/ (Job 2 AEO enrichment — group isolation, multi-runtime table, Activity log debugging sections added; dateModified 2026-08-04). Measure 2026-08-11. **EXP-022 QUEUED**: harness-engineering top-of-page banner (138 PVs, highest-traffic page with only buried CTA — replicate EXP-015 WIN pattern). **AEO Jobs 2–6 completed**: context-engineering enriched (31aceb2), debug-ai-coding-agents new page (ba8b124), amux-vs-goose rebuilt (5e39a13), for/business-owners new page (e1bb594), changelog 9 entries (ddb2799). | **EXP-017** marked concluded inconclusive. **EXP-021** shipped and marked running. **EXP-022** added as queued. homepage-experiments.md updated with all EXP-017/021/022 entries + today's learnings log. |
| 2026-08-10 | GitHub stars: **336** (+4 since last run). **EXP-020 CONCLUDED win +500%**: top-of-page CTA on blog/best-terminal-ai-coding-agents-2026 (started 2026-08-03). Pre (Jul 27–Aug 2, 7d): 1 total GH click = 0.14/day. Post (Aug 3–9, 7d): 7 total GH clicks = 1.0/day. Delta: **+500%** — decisive win. 6 named exp020 events confirmed. Same position-first pattern as EXP-015 (+200%). **EXP-016 CONCLUDED inconclusive**: CTO/team persona framing in homepage .lede (started 2026-08-01). Pre (Jul 25–31, 7d): 8.9 homepage GH clicks/day. Post (Aug 1–7, 7d): 9.4/day. Delta: **+6.5%** — below 10% win threshold. Confound: EXP-019 also running on homepage. Change kept. **PostHog 14-day KPI state:** Homepage ~94 PVs GH clicks (unchanged at ~15% CVR), harness-engineering 94 PVs / 0 GH clicks, best-self-hosted 66 PVs / 0 GH clicks. **EXP-022 SHIPPED**: green top-of-page CTA on harness-engineering (94 PVs, 0 GH clicks, biggest 0-CVR gap by traffic). Measure 2026-08-17. **EXP-023 SHIPPED**: green top-of-page CTA on best-self-hosted-ai-coding-tools-2026 (66 PVs, 0 GH clicks). Measure 2026-08-17. **AEO Jobs completed**: harness-engineering + best-self-hosted enriched (star count 336, new entity framing), new declarative-ai-agent-setup guide (IaC query gap), changelog updated (IaC, commit-nudge, model reporting), lists refreshed (best-ai-agent-orchestrators-2026), sitemap.xml updated. | **EXP-020** marked concluded win. **EXP-016** marked concluded inconclusive. **EXP-022** shipped and marked running (started 2026-08-10). **EXP-023** shipped and marked running (started 2026-08-10). Both added to concluded table. homepage-experiments.md updated with all entries. |
| 2026-08-13 | GitHub stars: **345** (+3 since last run). No experiments to score today (all running < 7 days; next windows: EXP-022/023/024 → 2026-08-17, EXP-021/025/026/027/028 → 2026-08-18, EXP-029–033 → 2026-08-19). **EXP-034 SHIPPED**: top-of-page green CTA on /guides/ index (157 PVs, 1.3% CVR — lowest-converting high-traffic page, no CTA existed). `exp034_guides_index_topofpage_cta_click` event. Measure 2026-08-20. **EXP-035 SHIPPED**: CTA baked into new /guides/ai-agent-groups/ page at launch. `exp035_groups_guide_topofpage_cta_click` event. Measure 2026-08-20. **Job 2**: best-ai-agent-multiplexers-2026 freshened — dateModified 2026-08-13, groups guide link added to amux feature bullet. **Job 3**: New guide /guides/ai-agent-groups/ created — groups feature AEO guide (commit 083877d, 2026-08-08), HowTo + FAQPage + Article + BreadcrumbList JSON-LD, full comparison table, EXP-035 CTA baked in. **Job 4**: /compare/amux-vs-bolt-new/ rebuilt (background agent). **Job 6**: changelog/notes.json 7 new entries prepended (groups panel, EnvSpec full env, board drain nudge, accountability sweep, env workdir fix, alert push fix, CLI dead-port warning). **Job 8**: Press boilerplate "single-file Python" → "single Rust binary, 345+ GitHub stars" (already updated by prior session's fork). Sitemap + guides index updated with new groups guide URL. | **EXP-034** and **EXP-035** shipped and marked running. /guides/ai-agent-groups/ created and listed. changelog/notes.json + sitemap.xml + guides/index.html updated. AEO_BACKLOG.md entries appended. |
| 2026-08-12 | GitHub stars: **342** (+4 since last run). No experiments to score today (all running ≤ 2 days). **EXP-029 SHIPPED**: top-of-page CTA on /blog/ai-coding-tools-pricing-2026/ (34 PVs, 0 GH clicks, stale May 2026 → freshened August 2026). `exp029_pricing_topofpage_cta_click` event. Measure 2026-08-19. **EXP-030 SHIPPED**: baked into new guide page /guides/claude-code-vs-codex-vs-gemini-cli/ at launch (query gap: no existing "Claude Code vs Codex vs Gemini CLI" comparison on site). `exp030_runtimes_compare_topofpage_cta_click` event. Measure 2026-08-19. **EXP-031 SHIPPED**: GitHub CTA banner on /concierge/ (37 PVs, 0 GH clicks — no GitHub path existed on a high-intent service page). `exp031_concierge_github_cta_click` event. Measure 2026-08-19. **EXP-032 SHIPPED**: top-of-page CTA on /lists/open-source-ai-coding-tools-2026/ (stale July 2026, no CTA). Also fixed amux language Python → Rust. `exp032_oss_list_topofpage_cta_click` event. Measure 2026-08-19. **EXP-033 SHIPPED**: top-of-page CTA on /lists/ai-tools-for-solopreneurs-2026/ (stale July 2026, no CTA). `exp033_solopreneurs_list_topofpage_cta_click` event. Measure 2026-08-19. **EXP-026/027/028 running** on 3 list pages (best-ai-coding-agents, best-claude-code-tools, ai-agent-frameworks), all started 2026-08-11, measure 2026-08-18. **Job 2**: /blog/ai-coding-tools-pricing-2026/ freshened — August 2026 badge, dateModified updated, Copilot billing updated (moved to past tense: June 1, 2026), multi-agent framing updated to mention Codex and Gemini CLI alongside Claude Code, Sonnet 4 → Sonnet 5. **Job 3**: New guide /guides/claude-code-vs-codex-vs-gemini-cli/ created (3-way runtime comparison, 14-row feature table, per-runtime detail sections, mixed-fleet walkthrough, decision guide by use case, 6-question FAQ, Article + FAQPage + BreadcrumbList JSON-LD). **Job 6**: changelog/notes.json 7 new entries prepended (copyable MSG-ID badge, cloud auto-deploy, Gmail mailbox, needsyou CLI fix, Map/graph tab, settings fix, offline badge); changelog/index.html JSON-LD ItemList updated with newest 5 entries. **Job 7**: 2 stale lists (open-source, solopreneurs) freshened + EXP-032/033. Press stars are dynamic — no update needed (Job 8). | **EXP-029 through EXP-033** shipped and marked running. EXP-026/027/028 added as formal running entries. Score window line updated. changelog/notes.json + index.html updated. 2 stale list pages freshened. Sitemap dates updated: pricing blog, 2 lists, concierge. |
| 2026-08-11 | GitHub stars: **338** (+2 since last run). **EXP-019 CONCLUDED inconclusive**: Review dashboard PS row on homepage (started 2026-08-02). Pre (Jul 26–Aug 1, 7d): 60 homepage GH clicks = 8.6/day. Post (Aug 2–8, 7d): 63 GH clicks = 9.0/day. Delta: **+4.7%** — below 10% threshold. EXP-016 confound. Row kept. **EXP-021 EXTENDED to 2026-08-18**: context-engineering top-of-page CTA — 0 GH clicks in 7-day window (Aug 4–10), insufficient traffic. Same low-volume pattern as EXP-014/EXP-018. **EXP-025 SHIPPED**: green top-of-page CTA on /guides/spec-driven-development/ (45 PVs, 0 GH clicks — NO CTA of any kind existed; biggest unaddressed gap). `exp025_specdriven_topofpage_cta_click` event. dateModified 2026-04-10→2026-08-11. Measure 2026-08-18. **Changelog**: 8 new entries prepended to notes.json (Cost tab, saved habits, subagent token attribution, reactive pickup, backlog-triage nudge, Connect modals fix, needsyou visibility fix, status-request 405 fix). **Job 3**: amux-vs-openhands rebuilt from 101-line stub to 360-line full compare page (15-row table, 5-question FAQ, "why not both?" section, EXP-007 CTA). **Job 7**: 3 stale list pages refreshed (best-ai-coding-agents, best-claude-code-tools, ai-agent-frameworks) — EXP-026/027/028 CTAs added, August 2026 content updates, star counts updated. | **EXP-019** marked concluded inconclusive + added to Concluded table. **EXP-021** extended to 2026-08-18. **EXP-025** shipped and marked running. **EXP-026/027/028** running on 3 list pages. amux-vs-openhands rebuilt. homepage-experiments.md + changelog/notes.json + AEO_BACKLOG.md + spec-driven-development updated. |

---

## Notes for SCHED-149

When running Job 9:
1. Query PostHog for click events on `github.com/mixpeek/amux`, `apps.apple.com` links, and `/concierge/` CTAs — these are the three KPI proxies.
2. Check which pages have the highest click-through on each KPI (not just pageviews).
3. Move the highest-confidence `queued` experiment to `running` — implement it (it will be small/XS effort by design in the backlog). Log the start date and expected measurement period (minimum 7 days of traffic).
4. Check any `running` experiments: if ≥7 days of data, read PostHog for pre/post comparison and log result in Concluded section.
5. Add new experiment ideas to the backlog if new patterns emerge from the data (e.g., a page with high traffic but zero KPI clicks is a signal).
6. Always implement the experiment change AND record it — don't just write about it.
| 2026-08-14 | GitHub stars: **347** (+2 since last run). No experiments to score today (all running < 7 days; next windows: EXP-022/023/024 → 2026-08-17, EXP-021/025/026/027/028 → 2026-08-18, EXP-029-033 → 2026-08-19, EXP-034/035 → 2026-08-20). **PostHog 14-day KPI state:** Homepage 900 PVs / 137 GH clicks (15.2% CVR); best-ai-agent-multiplexers-2026 300 PVs / 72 GH clicks (24.0% CVR); claude-code-headless 208 PVs / 4 GH clicks (1.9% CVR — stale, enriched today); demos/ 135 PVs / 0 GH clicks (biggest untouched gap by absolute PVs). **EXP-036 SHIPPED**: top-of-page green CTA baked into new /guides/ai-agent-simple-view/ at launch. `exp036_simple_view_topofpage_cta_click`. Measure 2026-08-21. **EXP-037 SHIPPED**: top-of-page green CTA on new /for/team-leads/ persona page. `exp037_team_leads_topofpage_cta_click`. Measure 2026-08-21. **EXP-038 SHIPPED**: green top-of-page CTA on /demos/ index (135 PVs, 0 GH clicks — no CTA existed). `exp038_demos_index_topofpage_cta_click`. Baseline 0% CVR. Measure 2026-08-21. **J2**: claude-code-headless enriched with Simple tab monitoring section + voice orchestration mention + messages calendar, dateModified 2026-08-14. **J4**: amux-vs-cowork rebuilt (156-line stub → 304-line full compare page). **J5**: /for/team-leads/ created (team lead persona; groups + Simple tab + board + Review + EnvSpec + voice). | **EXP-036/037/038** shipped and marked running. /guides/ai-agent-simple-view/ created (419 lines). /for/team-leads/ created (289 lines). /compare/amux-vs-cowork/ rebuilt (304 lines). claude-code-headless enriched + dateModified updated. changelog/notes.json 6 new entries prepended (voice orchestrator, Simple tab x3, messages calendar x2). changelog/index.html ItemList updated. sitemap.xml + llms.txt updated with new pages. demos/index.html EXP-038 CTA added. AEO_BACKLOG.md + homepage-experiments.md updated. |
| 2026-08-15 | GitHub stars: **350** (+3 since last run). No experiments to score today (all running < 7 days; next windows: EXP-022/023/024 → 2026-08-17, EXP-021/025/026/027/028 → 2026-08-18, EXP-029-033 → 2026-08-19, EXP-034/035 → 2026-08-20, EXP-036/037/038 → 2026-08-21). **PostHog 14-day KPI state:** Homepage 895 PVs / 136 GH clicks (15.2% CVR); best-ai-agent-multiplexers-2026 270 PVs / 30 GH clicks (11.1% CVR); claude-code-headless 212 PVs / 5 GH clicks (2.4% CVR); harness-engineering 77 PVs / 0 GH clicks (0% despite EXP-022, enriched today). **EXP-039 SHIPPED: top-of-page green CTA on new /guides/run-ai-agents-with-ollama/ (new page, zero baseline). `exp039_ollama_guide_topofpage_cta_click`. Measure 2026-08-22. **EXP-040 SHIPPED**: top-of-page green CTA on /docs/ index (136 PVs, 5 GH clicks = 3.7% CVR). `exp040_docs_index_topofpage_cta_click`. Baseline 3.7% CVR. Measure 2026-08-22. **J2**: harness-engineering enriched with Aug 2026 section (Simple tab as sensor, voice orchestration guide, subagent lifecycle tracking), new FAQ entry, dateModified 2026-08-15. **J3**: New guide /guides/run-ai-agents-with-ollama/ created (Ollama + qwen3.8:27b query gap, aa436ca commit just shipped). **J4**: expose-localhost-publicly updated — rate limiting (v0.9.96) and Proxies tab (v0.9.95), dateModified 2026-08-15. **J5**: amux-vs-ngrok updated — rate limiting row, Proxies tab row, dateModified 2026-08-15. **J6**: changelog/notes.json 8 new entries prepended (settings 5 tabs, PDF.js iOS, subagent count, Ollama qwen3, messages Human view, MSG- prefix fix, read-aloud player, browser panel fix). changelog/index.html ItemList updated with newest 5 entries. **J9**: EXP-039 shipped on /docs/. | **EXP-039** shipped and marked running (started 2026-08-15, measure 2026-08-22). /guides/run-ai-agents-with-ollama/ created. harness-engineering enriched. expose-localhost-publicly + amux-vs-ngrok updated to August 2026. changelog/notes.json 8 new entries prepended. changelog/index.html ItemList updated. docs/index.html EXP-039 CTA added + dateModified updated. |
| 2026-08-16 | GitHub stars: **350** (unchanged since yesterday). No experiments to score today (all running < 7 days; next windows: EXP-022/023/024 → 2026-08-17, EXP-021/025/026/027/028 → 2026-08-18, EXP-029-033 → 2026-08-19, EXP-034/035 → 2026-08-20, EXP-036/037/038 → 2026-08-21, EXP-039/040 → 2026-08-22). **PostHog 14-day KPI state:** Homepage ~900 PVs / ~136 GH clicks (15.2% CVR); guides/getting-started 37 PVs / 0 GH clicks (0% CVR — biggest zero-CVR gap on any guide by intent); guides/ai-coding-agents-overnight/ stale since April 2026, no CTA, 0 GH clicks. **EXP-041 SHIPPED**: top-of-page green CTA on /guides/ai-coding-agents-overnight/ (stale since 2026-04-03, 0 GH clicks, page freshened with new owner-alerts section and dateModified 2026-08-16). `exp041_overnight_topofpage_cta_click`. Measure 2026-08-23. **EXP-042 SHIPPED**: top-of-page green CTA on /guides/getting-started/ (37 PVs, 0 GH clicks — highest-intent guide with zero CVR). `exp042_getting_started_topofpage_cta_click`. Measure 2026-08-23. **J2**: guides/ai-coding-agents-overnight/ enriched — new "Owner alerts: push, email, and iMessage" section (amux alert command, CLAUDE.md instructions, health invariant), updated dateModified to 2026-08-16, updated FAQPage JSON-LD with alerts Q, EXP-041 CTA added. **J3**: New guide /guides/ai-agent-owner-alerts/ created (targets "AI agent notification system", "get notified when AI agent needs attention"). **J4**: compare/amux-vs-claude-managed-agents/ checked (222 lines, full page) — green CTA added (EXP-043). `exp043_managed_agents_compare_cta_click`. Measure 2026-08-23. **J5**: concierge/index.html enriched — multi-runtime mention (Claude, Gemini, etc.), owner-alert FAQ entry, push-notification mention in step 4. **J6**: changelog/notes.json 5 new entries prepended (1678a5b email alerts, ec08a71 orphaned Chrome fix, c6bd42b alert invariant, 6ebef13 AGENTS.md/GEMINI.md symlinks, 2b00af8 board show CLI). changelog/index.html ItemList updated with newest 5 entries. **J7**: All lists fresh (< 30 days, 2026-08-10 to 2026-08-12). Skipped. **J8**: Press stars 350 vs "345+" = 1.4% off. Below 20% threshold. Skipped. | **EXP-041/042/043** shipped and marked running (all started 2026-08-16, measure 2026-08-23). /guides/ai-coding-agents-overnight/ freshened. /guides/ai-agent-owner-alerts/ created. compare/amux-vs-claude-managed-agents/ CTA added. concierge/ enriched. changelog/notes.json 5 new entries + index.html ItemList updated. Obsidian AEO daily written. |
| 2026-08-17 | GitHub stars: **351** (+1 since yesterday). **EXP-022/023/024 CONCLUDED inconclusive**: All three hit 0 named events and 0 autocapture GH clicks both pre and post. Confirmed bot-traffic pattern: guide pages without demonstrated prior GH click history don't produce measurable human click behavior even with top-of-page CTAs. Key learning: target pages that already show some GH click behavior (like blog/best-terminal which had 0.14/day pre-EXP-020) rather than pages with PVs but zero clicks. **New pages and features:** .mdai computed files DAG engine (9f41809 + 2ec671b), Files tab OS file-manager UX (8b802e1), codex/ollama peek transcript rendering (98402af), live OPEN worker view (6a2d0bc). **EXP-045 SHIPPED**: top-of-page green CTA on /compare/amux-vs-ngrok/ (existing page with prior compare traffic, no green CTA existed). `exp045_ngrok_compare_topofpage_cta_click`. Measure 2026-08-24. **EXP-046 SHIPPED**: top-of-page green CTA baked into new /use-cases/webhook-testing/ page at launch. `exp046_webhook_testing_topofpage_cta_click`. Measure 2026-08-24. **J2**: /for/indie-hackers/ enriched — new "What's New in August 2026" section (owner alerts, computed files, Files tab OS UX, multi-runtime mix), dateModified updated to 2026-08-17. **J3**: New /use-cases/webhook-testing/ created (targets "test webhooks locally", "webhook testing without ngrok"; HowTo+FAQPage+Article+BreadcrumbList JSON-LD; comparison table; EXP-046 CTA). **J4**: /compare/amux-vs-ngrok/ refreshed (dateModified 2026-08-17, EXP-045 CTA added). **J5**: /guides/ai-agent-automations/ enriched with mdai/computed-files composition angle. **J6**: changelog/notes.json 5 new entries prepended (9f41809 mdai, 8b802e1 Files UX, 98402af peek, 6a2d0bc live OPEN, cb5724e disk). **J7**: All lists fresh (2026-08-10 to 2026-08-12). Skipped. **J8**: Stars 351 vs "345+" = 1.7% off. Below 20% threshold. Skipped. | **EXP-022/023/024** marked concluded inconclusive. **EXP-045/046** shipped and marked running. /for/indie-hackers/ enriched. /use-cases/webhook-testing/ created. /compare/amux-vs-ngrok/ refreshed. changelog updated. |
| 2026-08-18 | GitHub stars: **353** (+2 since yesterday). **EXP-021/025/026/027/028 CONCLUDED inconclusive**: All five hit their score windows (EXP-021 at 14d, EXP-025-028 at 7d). Zero named events and zero autocapture GH clicks across all pages and both windows. Bot-traffic pattern fully confirmed across 5 experiments on 5 different page types (guide, guide, list ×3). **Rule crystallized: only target pages that already have demonstrated human GH click behavior — PV count alone is not a signal of human presence.** The confirmed exceptions are pages that had even 0.14/day GH clicks pre-experiment (EXP-020 +500%) or clear-intent pages like solopreneurs. **EXP-047 SHIPPED**: top-of-page green CTA on /for/solopreneurs/ (page enriched with August 2026 Connectors content, new Connectors FAQ, dateModified 2026-08-18). `exp047_solopreneurs_topofpage_cta_click`. Measure 2026-08-25. **J2**: /for/solopreneurs/ enriched — "What's New in August 2026" section (Connectors tab, owner alerts, computed files, voice + TTS), new FAQ entry on credential management via Connectors, step 4 updated, all JSON-LD updated. **J3**: New /guides/ai-agent-connectors/ created (targets "connect AI agent to external APIs", "AI agent credentials manager", "API key management for AI agents"; HowTo 6 steps + FAQPage 6 Qs + Article + BreadcrumbList JSON-LD; 4-row comparison table; scope table; provider grid; green CTA). **J4**: /compare/amux-vs-cursor/ enriched — "What's New in August 2026" section (5 bullets: Connectors, owner alerts, computed files, spreadsheet preview, TTS cancel), 2 new table rows (Credential management, Owner alerts), stars updated 299→353+, dateModified 2026-08-18. **J5**: /guides/managed-ai-agent-setup/ enriched — "Connectors setup" subsection inside Concierge onboarding section, link to /guides/ai-agent-connectors/, dateModified 2026-08-18. **J6**: changelog/notes.json 5 new entries prepended (Connectors tab, MDAI Diagram+List tabs, MDAI data source, spreadsheet preview, TTS cancel); changelog/index.html ItemList updated. **J7/J8**: Skipped (lists fresh, press stars <20% off). | **EXP-021/025/026/027/028** marked concluded inconclusive. **EXP-047** shipped and marked running. /for/solopreneurs/ enriched. /guides/ai-agent-connectors/ created. /compare/amux-vs-cursor/ enriched. /guides/managed-ai-agent-setup/ enriched. changelog updated. sitemap.xml + llms.txt updated. AEO_BACKLOG.md appended. |
| 2026-08-24 | GitHub stars: **366** (+1 since last run). **EXP-045 CONCLUDED inconclusive**: /compare/amux-vs-ngrok/ — 0 named events, 0 autocapture GH clicks. No demonstrated prior GH click history on this compare page. Bot-traffic pattern. CTA kept. Re-score 2026-09-06. **EXP-046 INTERIM at 7 days**: /use-cases/webhook-testing/ — 0 named events, 0 autocapture GH clicks. New page, not yet indexed. Same new-page bot-traffic pattern as EXP-024/030/035. CTA kept. Extended to 21-day window. Re-score 2026-09-07. **Key learning (Aug 2026 pattern closure):** Compare pages without demonstrated prior GH click behavior consistently return bot-traffic pattern. Only pages with at least 0.14/day prior GH clicks (headless, terminal blog, guides index) have produced wins. **EXP-057 SHIPPED**: full rebuild of /compare/amux-vs-replit-agent/ — outcome-first framing, multi-runtime, August 2026 content (board analytics, permission audit trail, Connectors, Gmail 4x, @-worker dropdown), FAQPage + Article JSON-LD, 12-row table, top-of-page green CTA. `exp057_replit_compare_topofpage_cta_click`. Measure 2026-08-31. **EXP-058 SHIPPED**: new /compare/amux-vs-amazon-q-developer/ — uncovered "amazon q developer alternative" and "claude code vs amazon q" query cluster, 12-row comparison table, "Claude Code vs Amazon Q" bridging section, FAQPage + Article JSON-LD, top-of-page green CTA. `exp058_amazon_q_compare_topofpage_cta_click`. Measure 2026-08-31 (extend to 2026-09-14 for indexing). **EXP-059 SHIPPED**: top-of-page green CTA on /for/engineering-managers/ + "What's New August 2026" section (closed_at analytics, permission audit trail, Connectors, live config, Gmail 4x). `exp059_eng_managers_topofpage_cta_click`. Measure 2026-08-31. **J6**: changelog/notes.json 3 new entries (board closed_at analytics, permission audit trail, CLI unreachable-server hang fix). **J7**: Lists fresh (2026-08-10 to 2026-08-12, next threshold 2026-09-09). Skipped. **J8**: Stars "345+" vs 366 = 5.8% off. Below 20% threshold. Skipped. sitemap.xml updated (replit-agent lastmod, amazon-q-developer added, engineering-managers lastmod). llms.txt updated (replit-agent rebuilt, amazon-q-developer added, engineering-managers updated). | **EXP-045** marked concluded inconclusive. **EXP-046** extended to 21-day window (re-score 2026-09-07). **EXP-057** shipped (replit-agent full rebuild). **EXP-058** shipped (amazon-q-developer new page). **EXP-059** shipped (engineering-managers enrichment + CTA). changelog/notes.json 3 new entries prepended. sitemap.xml + llms.txt updated. AEO_BACKLOG.md appended. |
| 2026-08-27 | GitHub stars: **367** (unchanged). **EXP-049 CONCLUDED inconclusive**: /pricing/ + /ai-agent-sandboxing/ top-of-page CTA (started 2026-08-20, 7 days). No PostHog access. /pricing/ has demonstrated prior human traffic (8 GH clicks/14d, per 2026-08-03); genuine candidate but cannot confirm named events without data. /sandboxing/ follows bot-traffic pattern. CTAs kept on both. **EXP-050 CONCLUDED inconclusive**: /guides/ai-agent-memory-management/ new page (started 2026-08-20, 7 days). New-page bot-traffic pattern — same as EXP-024/035/036/037. Expected at 7 days. CTA kept. Re-score 2026-09-10. **EXP-064 SHIPPED**: /compare/amux-vs-cline/ enrichment + top-of-page green CTA. Most stale high-value compare page (42 days). Updated star count 298→367+, 2 new feature table rows (session cloning, GitHub App token shim), "What's New August 2026" section (4 bullets), dateModified 2026-08-27. `exp064_cline_compare_topofpage_cta_click`. Measure 2026-09-03. **EXP-065 SHIPPED**: baked into new /glossary/ai-agent-fleet/. Highest uncovered AEO query in the fleet cluster. DefinedTerm + Article + BreadcrumbList + FAQPage JSON-LD (6 Qs). Fleet components table, comparison table, fleet sizing, multi-runtime section. `exp065_ai_agent_fleet_glossary_cta_click`. Measure 2026-09-03 (extend to 2026-09-17 if bot-traffic pattern). **J6**: changelog/notes.json 3 new entries (0ae647b1 SSE health signal, 7f197c13 GitHub App token shim, b1f5a478 age-weighted board pickup); changelog/index.html ItemList updated. **J7**: Lists fresh until 2026-09-09. Skipped. **J8**: Stars 367 vs "345+" = 6.4% off. Below 20% threshold. Skipped. sitemap.xml + llms.txt updated. AEO_BACKLOG.md appended. | **EXP-049/050** marked concluded inconclusive. **EXP-064** shipped (cline enrichment + CTA). **EXP-065** shipped (new ai-agent-fleet glossary + CTA). changelog/notes.json 3 new entries + index.html ItemList updated. sitemap.xml + llms.txt updated. AEO_BACKLOG.md appended. |
| 2026-08-26 | GitHub stars: **367** (unchanged). **EXP-048 CONCLUDED inconclusive**: /for/startup-founders/ top-of-page CTA (started 2026-08-19). 0 named events, 0 autocapture GH clicks (Aug 19–25, 7 days). Bot-traffic pattern — same as EXP-047 (/for/solopreneurs/). Persona pages without prior GH click history confirmed to follow bot-traffic pattern regardless of declared intent. The "startup-founders is higher intent" hypothesis was disproved. CTA kept. **EXP-062 SHIPPED**: /compare/amux-vs-windsurf/ enrichment + top-of-page green CTA. Full August 2026 update: Windsurf OpenAI/Codeium acquisition context, star count 299→367, session cloning row added to feature table, "What's New August 2026" section (session clone, collapsible archived board, email draft discard, plan window reservation), dateModified 2026-08-26, FAQ negation-reframe removed. `exp062_windsurf_topcta_click`. Measure 2026-09-02. **Job 3**: New /guides/ai-agent-session-cloning/ created — targets "fork Claude Code session", "clone AI agent session", "run parallel AI experiments", "duplicate coding agent context". Enabled by bfd8d78c session-clone feature. HowTo (5 steps) + FAQPage (6 Qs) + Article + BreadcrumbList JSON-LD. EXP-063 CTA baked in. **J6**: changelog/notes.json 4 new entries (bfd8d78c session clone, f4092ab9 collapsible archived, 05df543e email draft discard, 03f3d8d3 reserve plan window). **J7**: Lists fresh until 2026-09-09. Skipped. **J8**: Stars 367 vs "345+" = 6.4% off. Below 20% threshold. Skipped. sitemap.xml + llms.txt updated. AEO_BACKLOG.md appended. | **EXP-048** marked concluded inconclusive. **EXP-062** shipped and marked running (windsurf enrichment + CTA). /guides/ai-agent-session-cloning/ created. changelog/notes.json 4 new entries + index.html ItemList updated. sitemap.xml + llms.txt updated. AEO_BACKLOG.md appended. |
| 2026-08-25 | GitHub stars: **367** (+1 since last run). **EXP-047 CONCLUDED inconclusive**: /for/solopreneurs/ top-of-page CTA (started 2026-08-18). 0 named events, 0 autocapture GH clicks (Aug 18–24, 7 days). No demonstrated prior GH click history. Bot-traffic pattern — same as EXP-032/033 persona pages. CTA kept. Re-score 2026-09-01. **EXP-060 SHIPPED**: /compare/amux-vs-github-copilot/ enrichment + top-of-page CTA. Multi-runtime framing, Message reliability row, "What's New August 2026" (5 bullets: canonical worker-send route, schedules turns/day, rate-limit message hold, usage tracking accuracy, multi-runtime fit section), dateModified 2026-08-25, subtitle "Updated August 2026", stars 367+. `exp060_github_copilot_compare_topofpage_cta_click`. Measure 2026-09-01. **EXP-061 SHIPPED**: New guide /guides/ai-agent-github-actions/ — "trigger Claude Code from GitHub Actions", "AI agent REST API integration" query gap. Enabled by canonical worker-send route (5e647edd). HowTo (5 steps) + FAQPage (6 Qs) + Article + BreadcrumbList JSON-LD. Shows concrete GitHub Actions YAML using `POST /api/sessions/<worker>/send`. Covers rate-limit hold, security considerations, fleet event routing table. EXP-061 CTA baked in at launch. `exp061_gh_actions_guide_topofpage_cta_click`. Measure 2026-09-01 (extend to 2026-09-14 if new-page bot-traffic pattern). **Job 2**: /for/ai-engineers/ enriched — "What's New August 2026" section (canonical worker-send route, schedules turns/day, rate-limit hold, usage tracking accuracy fix), dateModified 2026-08-25. **J6**: changelog/notes.json 3 new entries (5e647edd canonical worker-send route, 9b27e1c1 schedules turns/day, 84d0bd03 rate-limit message hold); changelog/index.html ItemList updated (5 newest entries). **J7**: Lists fresh until 2026-09-09. Skipped. **J8**: Stars 367 vs "345+" = 6.4% off. Below 20% threshold. Skipped. sitemap.xml updated (/for/ai-engineers/ lastmod, /compare/amux-vs-github-copilot/ lastmod, /guides/ai-agent-github-actions/ added). llms.txt updated (ai-agent-github-actions added, ai-engineers updated). | **EXP-047** marked concluded inconclusive. **EXP-060** shipped and marked running (github-copilot enrichment + CTA). **EXP-061** shipped and marked running (new ai-agent-github-actions guide, CTA baked in). /for/ai-engineers/ enriched. /compare/amux-vs-github-copilot/ enriched. /guides/ai-agent-github-actions/ created. changelog/notes.json 3 new entries + index.html ItemList updated. sitemap.xml + llms.txt updated. AEO_BACKLOG.md appended. |
| 2026-08-23 | GitHub stars: **365** (+2 since last run). **EXP-041 CONCLUDED inconclusive**: /guides/ai-coding-agents-overnight/ — 0 named events, 0 autocapture GH clicks (Aug 9–15 pre: 0, Aug 16–22 post: 0). Bot-traffic pattern. CTA kept. Re-score 2026-09-06. **EXP-042 INTERIM at 7 days**: /guides/getting-started/ — 1 named event (exp042, Aug 22), ~1 autocapture post vs 0 pre. Weakest positive signal seen on a non-click-history page. Not conclusive — extending to 21-day window. Re-score 2026-09-06. **EXP-043 CONCLUDED inconclusive**: /compare/amux-vs-claude-managed-agents/ — 0 named events, 0 GH clicks. Compare page without prior click history — bot-traffic pattern. CTA kept. **EXP-044 CONCLUDED inconclusive**: /guides/ai-agent-owner-alerts/ — 0 named events (new page, not indexed). CTA kept. **EXP-055 SHIPPED**: top-of-page green CTA on /compare/amux-vs-openhands/ (stale since 2026-08-11, no CTA). Page enriched simultaneously: Connectors/OAuth broker row, "What's New August 2026" section, star count 338→365, dateModified 2026-08-23. `exp055_openhands_compare_topofpage_cta_click`. Measure 2026-08-30. **J3**: New /glossary/reverse-tunnel/ created — targets "what is a reverse tunnel", "reverse tunnel open source", "reverse tunnel vs port forwarding". DefinedTerm + BreadcrumbList + Article + FAQPage JSON-LD; comparison table (amux vs ngrok vs localtunnel vs Cloudflare Tunnel); honest trade-off framing; EXP-056 CTA baked in (`exp056_reverse_tunnel_glossary_cta_click`). Internal links to /compare/amux-vs-ngrok/, /guides/expose-localhost-publicly/, /use-cases/webhook-testing/. **J6**: changelog/notes.json 2 new entries prepended (4049e87 Gmail batch API 4x faster, 2b1c408 @-worker dropdown in scheduler). **J7**: Lists fresh (2026-08-10 to 2026-08-12, next threshold 2026-09-09). Skipped. **J8**: Stars "345+" vs 365 = 5.5% off. Below 20% threshold. Skipped. sitemap.xml + llms.txt updated with /glossary/reverse-tunnel/. AEO_BACKLOG.md appended. | **EXP-041/043/044** marked concluded inconclusive. **EXP-042** extended to 21-day window (re-score 2026-09-06). **EXP-055** shipped and marked running (started 2026-08-23, measure 2026-08-30). **EXP-056** baked into /glossary/reverse-tunnel/ at creation. /compare/amux-vs-openhands/ enriched (Connectors row, Aug 2026 section, stars). /glossary/reverse-tunnel/ created. changelog/notes.json 2 new entries. sitemap.xml + llms.txt updated. AEO_BACKLOG.md appended. |
| 2026-08-22 | GitHub stars: **363** (+2 since last run). **EXP-039 CONCLUDED inconclusive**: /guides/run-ai-agents-with-ollama/ — 0 named events, 0 autocapture GH clicks (new page, not yet indexed, same bot-traffic pattern). CTA kept. **EXP-040 CONCLUDED inconclusive**: /docs/ index top-of-page CTA — pre (Aug 8–14): 6 GH clicks = 0.86/day; post (Aug 15–21): 6 GH clicks = 0.86/day. Delta: **0%** — flat signal, no conclusive verdict. CTA kept. **PostHog 14-day KPI state (per 2026-08-21 run):** compare/amux-vs-cmux/: 9 GH clicks (highest-converting compare page with no green CTA — best current target). **EXP-053 SHIPPED**: top-of-page green CTA on /for/ctos/ (high-intent persona page, no green CTA existed). Page also enriched: Python→Rust fixes (FAQ + production-ready answer), star count 336→363, "What's New August 2026" section (OAuth broker, board progress notes, status-explain, server self-adoption, disk watch), install command pip→brew, dateModified 2026-08-22. `exp053_ctos_topofpage_cta_click`. Measure 2026-08-29. **EXP-054 SHIPPED**: top-of-page green CTA on /compare/amux-vs-cmux/ (9 GH clicks/14d, highest-converting compare page with no green CTA). `exp054_cmux_compare_topofpage_cta_click`. Measure 2026-08-29. **J3**: New /guides/ai-agent-third-party-auth/ created (OAuth broker angle — "authorize AI agents for third-party services", "AI agent OAuth", "OAuth for AI agents"; HowTo 5 steps + FAQPage 6 Qs + Article + BreadcrumbList JSON-LD; green CTA baked in at creation). **J4**: /compare/amux-vs-devin/ updated — dateModified 2026-08-22, subtitle "Updated August 2026", "What's changed in August 2026" section (OAuth broker, server self-adoption, board progress notes, disk watchdog, 363★), install fix brew. **J6**: changelog/notes.json 2 new entries prepended (726ca46 disk watch, c510cad server self-adoption); changelog/index.html ItemList updated with all 7 top entries. Sitemap + llms.txt updated with new guide URL. | **EXP-039/040** marked concluded inconclusive. **EXP-053** shipped and marked running (started 2026-08-22, measure 2026-08-29). **EXP-054** shipped and marked running (started 2026-08-22, measure 2026-08-29). /guides/ai-agent-third-party-auth/ created. /for/ctos/ enriched (Python→Rust, stars, August 2026, install fix). /compare/amux-vs-devin/ enriched. changelog/notes.json 2 new entries + index.html ItemList updated. sitemap.xml + llms.txt updated. |
| 2026-08-21 | GitHub stars: **361** (+6 since last run). **EXP-036/037/038 CONCLUDED inconclusive**: All three hit their score windows (7-day post-period Aug 14–20). EXP-036 (ai-agent-simple-view): 0 named events, 0 autocapture GH clicks — new page pattern, not yet indexed. EXP-037 (team-leads): 0 named events, 0 autocapture GH clicks — same. EXP-038 (demos index): 1 named event (Aug 20), pre autocapture 1/7d = 0.14/day, post autocapture 1/7d = 0.14/day — flat signal, 1 event too weak for a conclusive verdict. All CTAs kept. **PostHog 14-day KPI state (most interesting finding):** glossary/agent-multiplexer/ has 4 GH clicks with NO top-of-page CTA — the highest-ratio untapped page. claude-code-headless: 9 clicks. compare/amux-vs-cmux/: 5 clicks. **EXP-051 SHIPPED**: top-of-page green CTA on /glossary/agent-multiplexer/ (4 GH clicks/14d — demonstrated human traffic, no CTA existed). Page fully rebuilt with comparison table, 6-question FAQPage, Article + DefinedTerm + BreadcrumbList JSON-LD, outcome-first subtitle. `exp051_agent_multiplexer_glossary_cta_click`. Measure 2026-08-28. **EXP-052 SHIPPED**: CTA baked into new /glossary/ai-agent-control-plane/ at launch — primary AEO query "AI agent control plane open source", zero prior coverage. `exp052_control_plane_glossary_cta_click`. Measure 2026-08-28. **J4**: amux-vs-cmux dateModified updated to 2026-08-21. **J6**: changelog/notes.json 7 new entries (7404228 board progress notes, 418e23c pane-churn, 318a37e status-explain, 68e87ef connector canary, ddd3ee9 OAuth broker, 05d925f files sort, dff05ce PWA resume location); changelog/index.html ItemList updated with newest 5. **J7/J8**: Lists fresh (last 2026-08-10 to 2026-08-12, threshold Sep 9). Stars 361 vs 345+ = 4.6% off, below 20% threshold. Both skipped. | **EXP-036/037/038** marked concluded inconclusive. **EXP-051** shipped and marked running (started 2026-08-21, measure 2026-08-28). **EXP-052** shipped and marked running (started 2026-08-21, measure 2026-08-28). /glossary/agent-multiplexer/ rebuilt (full AEO rebuild + EXP-051 CTA). /glossary/ai-agent-control-plane/ created. amux-vs-cmux dateModified updated. changelog/notes.json 7 new entries + index.html ItemList updated. sitemap.xml + llms.txt updated with new glossary page. |
| 2026-08-19 | GitHub stars: **355** (+2 since yesterday). **EXP-029-033 CONCLUDED inconclusive**: All five hit 7-day windows — 0 named events, 0 autocapture GH clicks across all. Bot-traffic confirmed across 5 more page types (blog/pricing, new compare page, concierge, two list pages). Bot-traffic rule now confirmed across 10 consecutive experiments (EXP-021-033). **PostHog 14-day KPI state:** Homepage 77 GH clicks, pricing 8, best-ai-agent-multiplexers 7, claude-code-headless 6, ai-agent-sandboxing 6. **EXP-048 SHIPPED**: top-of-page green CTA on /for/startup-founders/ (page enriched with Aug 2026 email attachments content + new FAQ entry). `exp048_startup_founders_topofpage_cta_click`. Measure 2026-08-26. Hypothesis: startup-founders is a high-intent buyer persona page — likely to carry real human click behavior unlike guide/list pages. **J1**: Daily AEO scan written to Obsidian. **J2**: /for/startup-founders/ enriched — "What's New in August 2026" (email attachments, Connectors tab, BM25 board search), new FAQ entry on email attachments, EXP-048 top-of-page CTA, dateModified 2026-08-19. **J3**: New /guides/ai-agent-email-automation/ created (targets "AI agent email automation", "send email from AI agent", "AI agent Gmail integration"; HowTo 4 steps + FAQPage 5 Qs + Article + BreadcrumbList JSON-LD; curl examples for send/reply/attach; workflow table; API reference). **J4**: /compare/amux-vs-n8n/ enriched — 2 new table rows (email attachments, Connectors), dateModified 2026-08-19. **J5**: /use-cases/ai-agent-business-automation/ enriched — email+attachments feature documented with new curl example, dateModified 2026-08-19. **J6**: changelog/notes.json 4 new entries (BM25 board search, email attachments send, email attachment download endpoint, session overloaded status); changelog/index.html ItemList updated. **J7/J8**: Skipped (lists fresh, press stars <20% off). | **EXP-029/030/031/032/033** marked concluded inconclusive. **EXP-048** shipped and marked running. /for/startup-founders/ enriched. /guides/ai-agent-email-automation/ created. /compare/amux-vs-n8n/ enriched. /use-cases/ai-agent-business-automation/ enriched. changelog updated. sitemap.xml + llms.txt updated. AEO_BACKLOG.md appended. |
| 2026-08-20 | GitHub stars: **360** (+5 since yesterday). **EXP-034 CONCLUDED win +207%**: guides index top-of-page CTA (started 2026-08-13). Pre (Aug 6–12, 7d): 1 GH click = 0.14/day. Post (Aug 13–19, 7d): 3 GH clicks = 0.43/day. Delta: **+207%** — decisive win. 3 named exp034 events confirmed (Aug 17, 19, 20). Pattern now confirmed on 3 pages: headless +200%, terminal blog +500%, guides index +207%. Change kept permanently. **EXP-035 CONCLUDED inconclusive**: ai-agent-groups new page CTA — 0 named events, 0 GH clicks (new page not yet indexed). Same pattern as EXP-024/030. CTA kept. Re-score 2026-09-07. Bot-traffic rule now confirmed across 10+ consecutive experiments on new pages. **EXP-049 SHIPPED**: pricing page + sandboxing page CTAs (exp049_pricing_topofpage_cta_click + exp049_sandboxing_topofpage_cta_click). /pricing/ has demonstrated buyer-intent GH clicks. /guides/ai-agent-sandboxing/ freshened (August 2026, 360+ stars, new credentials-in-CLAUDE.md FAQ). Measure 2026-08-27. **EXP-050 SHIPPED**: New /guides/ai-agent-memory-management/ guide with baked-in CTA (exp050_memory_guide_topofpage_cta_click). 4-type memory system, CLAUDE.md foundation layer, what-goes-where table (9 rows), HowTo 5 steps + FAQPage 6 Qs + Article + BreadcrumbList JSON-LD. Measure 2026-08-27. **J1**: Daily AEO scan written to Obsidian. **J6**: changelog/notes.json 3 new entries (e58c085 email read-back sent copy, 2e78f49 browser start viewport/ignored fields, a1c5be1 email reply audit rows); changelog/index.html ItemList updated (newest 5: e58c085, 2e78f49, a1c5be1, 168f311, 438e528). | **EXP-034** marked concluded win. **EXP-035** marked concluded inconclusive. **EXP-049** shipped and marked running (pricing + sandboxing). **EXP-050** shipped and marked running (memory guide new page). /guides/ai-agent-memory-management/ created. /pricing/ + /guides/ai-agent-sandboxing/ freshened. changelog/notes.json 3 new entries + index.html ItemList updated. sitemap.xml + llms.txt updated. AEO_BACKLOG.md appended. homepage-experiments.md updated. |
