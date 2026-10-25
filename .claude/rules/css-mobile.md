---
description: When editing dashboard CSS — mobile and PWA considerations
globs: ["crates/amux-dashboard/static/**"]
---

When editing CSS:
- Check that any new UI fits within the existing `@media (max-width: 600px)` breakpoints
- Touch targets must be at least 44x44 on mobile
- Use `env(safe-area-inset-*)` for anything positioned at screen edges (iOS PWA notch)
- The viewport uses `viewport-fit=cover` — fixed overlays must extend to physical edges
- Test that flex containers don't overflow on 375px-wide screens

**Gut-check every new feature against phone real estate (Ethan, 2026-10-21).**
On a phone a view gets at most ONE row of controls above its content and ONE
below. When a feature adds a control, it joins that row: the row scrolls
sideways (`flex-wrap:nowrap; overflow-x:auto`) or the extras go behind a `⋯`
menu. Never wrap to a second row and never add a new bar. The Messages log had
grown to six rows on top and a two-row pager, leaving the list the bottom
third of a 375px screen; `e2e/messages-mobile-one-row.spec.ts` is the pattern
for pinning it (each control's centre inside the one bar, bar height at most
56px, content at least half the viewport). Before calling a UI change done,
screenshot it at 375px and count the rows.


Use the production component contract in `docs/ui-style-guide.md` and **Settings →
Style guide** before creating or changing a control. Reuse `.btn`, `.input`, the
shared dialog helpers and theme/spacing tokens; fix a recurring pattern in the
shared class instead of adding another independent variant. Preserve the owner's
original toolbar icons. Primary fills pair `--accent` with `--on-accent`; verify
focus, disabled, loading and invalid states in both themes. The gallery must render
production classes, not copied demonstration styles.
