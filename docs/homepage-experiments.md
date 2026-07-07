# amux.io Homepage Experiments

**KPIs we optimize for:**
| KPI | PostHog event | Target |
|-----|--------------|--------|
| GitHub stars | `$pageview` → github.com/mixpeek/amux click | ↑ star rate |
| iOS downloads | click on App Store link | ↑ iOS installs |
| Cloud signups | click on concierge/cloud CTA | ↑ onboarding calls |

**Tracking note:** PostHog project key is `phx_z3rBWTAM8jzH7MYoJt396nNPfmC8CuE5sTn86Yt5k4i4vgmG` (US cloud, us.i.posthog.com). To query KPI clicks use HogQL on `$autocapture` events filtered by `elements_chain` containing the relevant href. Personal API key needed for API queries — set `POSTHOG_PERSONAL_API_KEY` in ~/.amux/server.env.

---

## Running Experiments

### EXP-001 — Hero CTA button copy
- **Status:** `running`
- **Started:** 2026-07-07
- **Change:** "View on GitHub" → "⭐ Star on GitHub" (both hero CTA instances in index.html, `replace_all`)
- **Hypothesis:** More action-oriented, emoji-prefixed star CTA increases GitHub star click-through by 20%+
- **KPI:** GitHub star clicks (PostHog autocapture on github.com/mixpeek/amux link)
- **Measure after:** 2026-07-14 (7 days minimum)

---

## Experiment Backlog (prioritized)

### EXP-001 — Hero CTA button copy
- **Hypothesis:** "View on GitHub" → "⭐ Star on GitHub" increases star click-through by 20%+
- **Page:** `/` (homepage hero)
- **KPI:** GitHub star clicks
- **Status:** `running` — started 2026-07-07
- **Implementation:** Changed both "View on GitHub" instances to "⭐ Star on GitHub" in site/index.html
- **Effort:** XS (1 line edit)

### EXP-002 — iOS CTA placement in nav
- **Hypothesis:** Moving "iOS app" from nav to a sticky mobile-only bottom bar increases iOS taps on mobile by 30%+
- **Page:** All pages (site-header nav)
- **KPI:** iOS downloads
- **Status:** `queued`
- **Implementation:** Add `@media (max-width: 600px)` fixed bottom bar with App Store button; hide from nav on mobile
- **Effort:** S (CSS + site.js)

### EXP-003 — Homepage hero subheadline
- **Hypothesis:** Adding a concrete social proof line ("Join 2,000+ developers running AI teams overnight") under the H1 increases GitHub clicks and concierge signups
- **Page:** `/` (index.html hero)
- **KPI:** GitHub stars + cloud signups
- **Status:** `queued`
- **Implementation:** Add `<p class="social-proof">` line below H1
- **Effort:** XS

### EXP-004 — Concierge CTA urgency
- **Hypothesis:** Adding scarcity to the concierge CTA ("3 onboarding slots open this month") increases cloud signup clicks by 25%+
- **Page:** `/concierge/` + homepage concierge block
- **KPI:** Cloud signups
- **Status:** `queued`
- **Implementation:** Update CTA text + add a small badge/counter
- **Effort:** XS

### EXP-005 — "Star History" social proof on homepage
- **Hypothesis:** Embedding a star-history chart image on the homepage (showing growth trend) increases GitHub clicks from visitors who aren't sure if the project is active
- **Page:** `/` 
- **KPI:** GitHub stars
- **Status:** `queued`
- **Implementation:** Add star-history.com embed image to homepage below the feature table
- **Effort:** S

### EXP-006 — GitHub README → iOS CTA
- **Hypothesis:** Adding an App Store badge to the README (near the top, before the feature table) increases iOS installs from GitHub traffic
- **Page:** README.md
- **KPI:** iOS downloads
- **Status:** `queued`
- **Implementation:** Add `[![App Store](badge)](appstore-link)` to badges row
- **Effort:** XS

### EXP-007 — Compare pages → GitHub CTA
- **Hypothesis:** Compare pages get high-intent "alternative" traffic. Adding a prominent "Try it free — ⭐ on GitHub" CTA box at the top of each compare page (not just the bottom) increases star clicks from comparison traffic
- **Page:** All `/compare/amux-vs-*/` pages
- **KPI:** GitHub stars
- **Status:** `queued`
- **Implementation:** Add a compact top CTA block after the first paragraph on compare pages
- **Effort:** M (bulk edit via Python)

### EXP-008 — Dark/light mode preference → personalization signal
- **Hypothesis:** Users who switch to light mode are more likely to be non-developers (less terminal-native) and may convert better on concierge/cloud vs GitHub. Track theme preference as a PostHog property.
- **Page:** All pages (site.js)
- **KPI:** Cloud signups (segment)
- **Status:** `queued`
- **Implementation:** `posthog.register({ theme_preference: theme })` in site.js
- **Effort:** XS

### EXP-009 — "Indie hackers" and "overnight builders" language in hero subtitle
- **Hypothesis:** Explicitly calling out the indie hacker / overnight builder persona in the homepage hero subtitle increases conversion from that audience (HN, Product Hunt, Indie Hackers forum traffic).
- **Page:** `/` (hero subtitle)
- **KPI:** GitHub stars + concierge signups
- **Status:** `queued`
- **Implementation:** A/B between current subtitle and one that mentions "indie hackers", "solo builders", or "ship overnight"
- **Effort:** XS

### EXP-010 — Feature table reorder: phone-first features at top
- **Hypothesis:** Most visitors who convert on cloud/iOS CTAs are non-enterprise users who are drawn to the phone/mobile angle. Reordering the feature comparison table to lead with "Mobile dashboard (iOS app + PWA)" before "Self-healing watchdog" may increase iOS and cloud CTA clicks.
- **Page:** `/` (feature comparison table)
- **KPI:** iOS downloads + cloud signups
- **Status:** `queued`
- **Implementation:** Reorder table rows in the homepage feature grid
- **Effort:** XS

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

---

## Notes for SCHED-149

When running Job 9:
1. Query PostHog for click events on `github.com/mixpeek/amux`, `apps.apple.com` links, and `/concierge/` CTAs — these are the three KPI proxies.
2. Check which pages have the highest click-through on each KPI (not just pageviews).
3. Move the highest-confidence `queued` experiment to `running` — implement it (it will be small/XS effort by design in the backlog). Log the start date and expected measurement period (minimum 7 days of traffic).
4. Check any `running` experiments: if ≥7 days of data, read PostHog for pre/post comparison and log result in Concluded section.
5. Add new experiment ideas to the backlog if new patterns emerge from the data (e.g., a page with high traffic but zero KPI clicks is a signal).
6. Always implement the experiment change AND record it — don't just write about it.
