-- AF-318: `needsyou` has to name WHO is being asked WHAT, and what ends it.
--
-- Measured on the live board 2026-08-29: 445 cards in `needsyou`, median age
-- 15 days, oldest 58. Classified by title+desc, 24% are decision-shaped, 13%
-- access-shaped, 13% verification-shaped, and 51% (227) match NONE — their
-- titles are ordinary engineering work ("Compute Utilization Audit", "Fix
-- Namespace Pollution", "Batch Workers Blocked").
--
-- The cause is structural, not sloppiness. `needsyou` is the only status that
-- costs a worker nothing and stops the idle nudge, so it collects everything a
-- worker decided to stop doing. The ~20 items that genuinely need Ethan are
-- then indistinguishable inside 445 rows that mostly do not.
--
-- Three columns, not one blob, because the three questions come apart: WHICH
-- KIND of human act is needed (routable), WHAT is being asked (answerable), and
-- WHAT ENDS THE BLOCK (checkable). A free-text note satisfies none of them
-- mechanically, which is what the existing `NEEDS-YOU:` desc marker already is.
ALTER TABLE issues ADD COLUMN ask_type TEXT;
ALTER TABLE issues ADD COLUMN ask_question TEXT;
ALTER TABLE issues ADD COLUMN ask_unblocks TEXT;
