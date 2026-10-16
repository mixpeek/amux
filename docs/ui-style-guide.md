# amux UI components

Open **Settings → Style guide** for interactive examples of the production components.
The gallery uses `app.css` and the real confirmation dialog. Its controls only modify
example state. Theme preview returns to the starting theme when the guide closes.

## Shared source

- `crates/amux-dashboard/static/app.css`: theme tokens and component classes.
- `crates/amux-dashboard/static/app.js`: `openStyleGuide`, the existing dialog helpers,
  `_modalLayoutCheck`, and `_uiComponentCheck`.
- `e2e/ui-component-guide.spec.ts`: production rendering, focus, keyboard escape,
  theme restoration, example feedback, and a deliberate contrast failure.

Use the existing classes below. Extend a shared component at its source when a
pattern recurs; do not copy a dialog's HTML/CSS and give it another independent theme.
Do not replace the owner's original toolbar symbols with a different icon family.

## Tokens

| Purpose | Tokens | Rule |
| --- | --- | --- |
| Surfaces | `--bg`, `--card`, `--border` | Inherit the current theme; no fixed light/dark colors for ordinary surfaces. |
| Text | `--text`, `--dim` | Main content and supporting labels. |
| Primary action | `--accent`, `--on-accent` | Always pair these for filled primary buttons. |
| Status | `--green`, `--yellow`, `--red` | Include words or an accessible label; color alone is insufficient. |
| Spacing | `--space-1/2/3/4/6` | 4 / 8 / 12 / 16 / 24 pixels. |
| Corners | `--radius-control`, `--radius-dialog` | 8 / 12 pixels for standard controls and dialogs. |
| Controls | `--control-height`, `--control-font` | 40 pixels on desktop; 44 pixels and 16-pixel input text at ≤600 pixels. |
| Layer | `--layer-dialog`, `--layer-confirmation` | Standard dialogs cover the app tab strip; confirmation overlays sit above their parent dialog. |
| Focus | `--focus-ring` | Visible focus plus an outline; do not remove keyboard focus indication. |

## Components and states

| Component | Production classes/helpers | Contract |
| --- | --- | --- |
| Primary button | `.btn.primary` | One primary action per group; contrasting text; meaningful verb. |
| Secondary button | `.btn` | Normal action, Cancel, or Close; wraps with its action group. |
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
