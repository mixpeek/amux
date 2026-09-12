# Worker composer draft state

AMUX-4424, 2026-09-11.

The worker card, details composer, fullscreen editor, and embedded grid details
edit one local draft per worker. Browser tabs on the same origin share that
draft through localStorage events. Drafts remain local to the browser/device;
they are not synchronized between separate devices.

Each input edit commits immediately and mirrors into the other mounted editors.
The previous 250ms delay let the card retain a partial copy of the message sent
from details. Acceptance cleared only exact text matches, leaving the prefix on
the card; lifecycle flushing could then save it again.

Submissions capture a draft revision before asynchronous local acceptance. The
receipt clears that revision across views, preserving newer edits, including
deleting and retyping the same text. Failed local acceptance keeps the draft.
The existing durable outbox owns network delivery and retry after acceptance.

Opening, closing, rendering, and backgrounding never harvest stale DOM copies
over the shared draft. Fullscreen, history, saved-message, chip, and autocomplete
edits use the same state updates. Unsent drafts no longer expire after 14 days.
Storage failures retain text in the current page, show a warning, and retry the
pending write on background. They cannot promise persistence through a page kill
while browser storage remains unavailable.

`composer_locally_accepted` and `composer_draft_storage_failed` are emitted to
the server's client-debug endpoint without message text. The former includes
whether a newer draft was preserved; the latter identifies the storage error.

Regression coverage lives in `e2e/composer-draft-state.spec.ts`, alongside the
existing `e2e/lifecycle/composer.spec.ts` and `composer-cards.spec.ts` outbox
tests. Exercise all three on desktop, mobile Chromium, and iPhone WebKit. The
original partial-mirror regression failed on the pre-fix source with the old
prefix still in the card after sending, closing, and backgrounding.

Validation: 36 draft-state browser checks passed across desktop, phone Chromium,
and iPhone WebKit, plus 24 existing delivery checks (eight per browser target).
All 30 dashboard asset contracts and workspace/all-target clippy passed. SPA lint
reported zero errors. Disabling revision ownership made the identical-text
regression fail; the mutation was restored before the final 36-check run.
