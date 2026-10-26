# amux UI components

Open **Settings → Style guide** for interactive examples of the production components.
The gallery uses `app.css` and the real confirmation dialog. Its controls only modify
example state. **Preview next theme** steps through every theme; yours comes back when
the guide closes.

amux has one design system: semantic tokens in `app.css`, the shared components below
that read them, and a theme engine that rewrites the color tokens. A component never
names a color, size, radius, shadow or font; it names a token.

## Shared source

- `crates/amux-dashboard/static/app.css`: theme tokens and component classes.
- `crates/amux-dashboard/static/app.js`: `openStyleGuide`, the existing dialog helpers,
  `_modalLayoutCheck`, and `_uiComponentCheck`.
- `e2e/ui-component-guide.spec.ts`: production rendering, focus, keyboard escape,
  theme restoration, example feedback, and a deliberate contrast failure.
- `scripts/lint-design-tokens.mjs`: the token lint (below), run in CI by
  `e2e/design-tokens.test.mjs`.
- `e2e/themes.spec.ts`: every theme reaches every surface, layout never changes with
  a theme, and the create, preview, import and export flows.

Use the existing classes below. Extend a shared component at its source when a
pattern recurs; do not copy a dialog's HTML/CSS and give it another independent theme.
Do not replace the owner's original toolbar symbols with a different icon family.

## Tokens

Defined once, at the top of `app.css` between `── Design tokens ──` and
`── end design tokens ──`. Nothing outside that block writes a literal value.

| Purpose | Tokens | Rule |
| --- | --- | --- |
| Surfaces | `--bg`, `--bg-inset`, `--card`, `--border` | Page, sunken wells, raised surfaces and dividers. Follow the theme. |
| Text | `--text`, `--dim` | Main content and supporting labels. |
| Primary action | `--accent`, `--on-accent` | Always pair these for filled primary buttons. |
| On a status fill | `--on-solid` | Text on a solid green, red or purple fill. |
| Status | `--green`, `--yellow`, `--red`, `--cyan`, `--orange` | Include words or an accessible label; color alone is insufficient. |
| Categorical | `--purple`, `--pink`, plus the status colors | Per-name colors (avatars, worker badges) come from `themeCategorical(hash)`. |
| Terminal and code | `--term-bg`, `--term-text`, `--term-dim`, `--term-accent`, `--term-border`, `--term-green`, `--term-red`, `--term-yellow`, `--term-cyan` | The terminal stays dark in light themes; these are its colors there. xterm.js reads them through `themeColor()`. |
| Shadows and scrims | `--shade`, `--shadow-sm/md/lg/xl` | Elevation by size; a scrim is `color-mix(in srgb, var(--shade) N%, transparent)`. |
| Tints | `color-mix(in srgb, var(--token) N%, transparent)` or `… var(--bg))` | A lighter or darker variant is a mix of tokens, never a new hex. |
| Type | `--fs-4xs` … `--fs-8xl` (0.6 to 3rem), `--font-ui`, `--font-mono` | One scale for every size. `em` and `%` stay relative. |
| Spacing | `--space-0-5` … `--space-20` | A 2px grid: 2, 4, 6, 8, 10, 12, 14, 16, 20, 24, 32, 40, 48, 64, 80 pixels. |
| Corners | `--radius-xs/sm/md/lg/xl/2xl/3xl`, `--radius-pill`, `--radius-round` | 2 to 16 pixels; `--radius-control` (8) and `--radius-dialog` (12) for controls and dialogs. |
| Controls | `--control-height`, `--control-font` | 40 pixels on desktop; 44 pixels and 16-pixel input text at ≤600 pixels. |
| Layer | `--layer-dialog`, `--layer-confirmation` | Standard dialogs cover the app tab strip; confirmation overlays sit above their parent dialog. |
| Focus | `--focus-ring` | Visible focus plus an outline; do not remove keyboard focus indication. |

### Adding a token

Add it to the token block (and to `body.light` when it differs in light mode). A color
token must also be in `THEME_TOKENS` in `app.js`, so a theme can set it:
`e2e/design-tokens.test.mjs` fails on a color token no theme can reach.

## Themes

**Settings → Device → Appearance → Theme** picks a theme; **Themes…** opens the
manager. Built in: Amux Dark (the default, which takes Settings → Branding's color as
its accent), Amux Light, GitHub Dark, GitHub Light, GitHub Dark Dimmed, One Dark Pro,
Dracula, Atom One Light, Ayu Dark, Ayu Mirage and Ayu Light.

The manager previews a theme across the whole app (closing it restores yours), uses it,
copies a built-in to edit, creates a theme with a live preview, imports a `.json`
file, exports any theme, and deletes your own. A theme is validated before it can be
saved or used: the schema and version, the base, hex colors, and contrast (text on
the background and surface and terminal text at 4.5:1 or more; secondary text and
accent below 3:1 are warnings). Themes are kept on the device and on the server
(`ui_themes`, `ui_theme` prefs), so another device gets them; a change made offline
is retried until the server has it. Other open pages (split panes, Mac app tabs)
follow a change at once.

A theme sets colors only. Layout, spacing, type and behavior do not change with a
theme; `e2e/themes.spec.ts` compares every element's box under four themes.

### Theme file (schema `amux-theme`, version 1)

```json
{
  "schema": "amux-theme",
  "version": 1,
  "name": "Dracula",
  "base": "dark",
  "colors": {
    "bg": "#282a36", "card": "#21222c", "border": "#44475a",
    "text": "#f8f8f2", "dim": "#8b93b8", "accent": "#bd93f9",
    "green": "#50fa7b", "red": "#ff5555", "yellow": "#f1fa8c"
  }
}
```

`bg`, `card`, `border`, `text`, `dim` and `accent` are required; every other token in
`THEME_TOKENS` is optional and falls back to the base (`dark` or `light`). A dark
theme's terminal uses its own colors unless it names `term*` ones. Keys are camelCase
(`bgInset` for `--bg-inset`). An unknown key is a warning, not an error. A later
version of the schema needs a newer amux.

## Enforcement

`node scripts/lint-design-tokens.mjs` reports, in `app.css`, `app.js`, `index.html`
and `state/feedback.css`:

- a color literal (hex, `rgb()`, `hsl()`) outside the token block and the built-in
  theme palettes;
- a font size, radius or spacing in px/rem instead of a token;
- a font stack instead of `--font-ui` / `--font-mono`;
- a `var(--name)` that nothing defines (with no fallback).

A value that must be literal carries `design-tokens: allow (<reason>)` on its line:
a hex-only consumer (Excel cells, ink saved in drawings, `<input type=color>`, the
browser's `theme-color`), stored data (the brand color), or a program's own
256-color terminal output. `business/` is a separately built bundle and is out of
scope.

Regressions are caught three ways: the lint, the theme spec's probe (every token set
to an unused color; any element that keeps its old color is reported by name), and the
layout comparison across themes.

## Components and states

| Component | Production classes/helpers | Contract |
| --- | --- | --- |
| Primary button | `.btn.primary` | One primary action per group; contrasting text; meaningful verb. |
| Secondary button | `.btn` | Normal action, Cancel, or Close; wraps with its action group. |
| Switch | `.btn` with `role="switch"` and `aria-checked` (`true`, `false`, `mixed`) | A setting that is on or off. Draws a track and knob from `aria-checked` with On/Off beside it; never a filled `.primary` button. |
| Destructive button | `.btn.danger` | Explicit action label; confirmation when real data would be irreversibly affected. |
| Unavailable/loading | `disabled`, `aria-busy="true"` | Prevent dispatch and explain the state. Do not disguise a failed request as loading forever. |
| Press feedback | automatic: `state/feedback.mjs` (`.press-busy`, `.press-done`, `.press-failed`) | Every press whose handler makes a request is busy (disabled, `aria-busy`, a bar on its bottom edge) until it settles, refuses a second press, then shows the outcome; a failure always reaches a toast. Write plain handlers and do not hand-roll busy code. Opt a rapid-repeat control out with `data-repeatable`. Inventory: `docs/ux/button-feedback-audit.md` (`node scripts/button-audit.mjs`). |
| Input/select/textarea | `.input`, `.ui-field`, `.ui-help` | Persistent label, consistent border/height/type size; native mobile selectors. |
| Invalid field | `aria-invalid="true"`, `.ui-error` | Explain the correction beside the field; connect it with `aria-describedby`. |
| Status | `.status-badge.active/.waiting/.idle` | Shared semantic treatment with a readable label. |
| Icon control | Existing glyph + accessible button label | Keep 🔔 and ⚙ in the toolbar. At least 44×44 pixels on mobile. |
| Confirmation | `showConfirm` / `showAlert` / `showPrompt` | Use the established helpers; surface the outcome and preserve an unsent draft on cancellation. |
| Standard dialog | `.modal-overlay > .modal` | Heading and Close, scrollable `.modal-body`, reachable `.modal-footer`. |
| Menu | Existing menu/trigger pair | Anchor to the trigger, fit the viewport, and remain dismissible. |
| Folder listing | `_feCrumbsHtml`, `_feBuildRow`, `_feParentRow`, `_feEmptyRow`, `_feStatusText`; `.fe-breadcrumb`, `.fe-scroll`, `.fe-col-headers`, `.fe-status` | Every folder view (Files, the Split file panel, MDAI) renders through these, so rows, icons, sort, parent row and status match. Pass `openDir`/`openFile` to `_feBuildRow` for view-specific opening; never build a second table. |

Terminal output, source editors, maps, and media have specialized content layouts.
Their surrounding buttons, labels, menus and dialogs follow this contract. Terminal
ANSI content can retain its dark surface in the light theme; that is a content requirement,
not a second general-purpose theme.

## Verification and diagnostics

Run the guide and existing modal/header regressions on phone and desktop widths,
in both themes. Check loading, empty, long-content, error, disabled, focus, and
keyboard-open states. Confirm actual click/type/dismiss outcomes in the native iOS
Simulator; a dispatched gesture alone does not prove its target received it.

`_uiComponentCheck()` reports `measured`, `n_considered`, and issues for visible shared controls: undersized phone controls, plus contrast
for enabled primary buttons with opaque backgrounds. The dialog observer emits
`ui-component-drift` to `/api/client-debug` when an issue appears. It records identifiers
and counts, never field values. A transparent-background control or a disabled control
is outside that contrast measurement; zero considered is not a full-page contrast audit.
`_modalLayoutCheck()` continues to diagnose clipping, missing dismissal controls, and
selected surface contrast problems.

The gallery is the maintained reference for shared components, not proof that every
historical screen override conforms. Keep surface-specific regression cases when
migrating a custom widget. Use the native modal inventory and retained screenshots to
check the real app rather than relying solely on the gallery.
