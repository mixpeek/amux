//! "You went idle with N uncommitted change(s)" — ported from Python
//! (AMUX-2638), with the one change that stops a known incident recurring.
//!
//! # The requirement, which is the whole reason this is a card
//!
//! Python derived "yours" from DIRTY-TREE MEMBERSHIP. Measured 2026-08-09: it
//! listed 11 files as mine to commit; I had touched NONE of them — they were a
//! peer's in-flight rust migration — while the staged-guard, asked the same
//! question at the same moment about the same files, answered foreign=4,
//! unclaimed=18, MINE=0. Two components, one question, opposite answers, and
//! the WRONG one was the one giving instructions. Followed literally it sweeps
//! a peer's work into your commit, which happened three times in one day
//! (762e06e, 325314d, 4bf767c).
//!
//! So ownership here comes from [`Ownership`], which is the staged-guard's
//! answer (`POST /api/git/staged-guard`, api/git_guard.rs) and nothing else.
//! `git status` supplies the LIST of dirty paths and no opinion about whose
//! they are. That split is enforced by the signature: [`build`] cannot see a
//! repo, so it cannot re-derive ownership even by accident.
//!
//! # Why a foreign file does not merely get filtered out
//!
//! It is named, loudly, with its owner. The recipient is about to run
//! `git add -A`; the useful thing is not silence about the peer's file, it is
//! "do not commit this one, it is theirs". Python learned that the hard way —
//! the branch was missing entirely and two sweeps (~93 and ~85 lines) followed.
//!
//! # Why `shared` is warned about rather than suppressed
//!
//! On a repo where two lanes routinely touch one file, "both edited it" is
//! satisfied almost always, so suppressing on `shared` would silence the nudge
//! permanently. Name the file as contested and say who else is in it; the
//! recipient can then stage per-hunk instead of per-file.

use std::collections::{BTreeMap, BTreeSet};

/// The staged-guard's verdict, transcribed. Every field is a list of paths.
///
/// Deliberately NOT constructible from a working tree: it exists only to carry
/// an answer the guard already gave.
///
/// Sentinel for a foreign path whose OWNER did not resolve — a running cotenant
/// with no transcript. Distinct from an absent entry (which means nobody's edit
/// record claims the path) because the two lead to different actions: one says
/// there is a peer to go and find, the other says there is not (AMUX-3816).
pub(crate) const UNRESOLVED_OWNER: &str = "\u{0}unresolved";

#[derive(Debug, Default, Clone)]
pub struct Ownership {
    /// A peer edited it and this session did NOT. Never commit these.
    ///
    /// The owner may be [`UNRESOLVED_OWNER`]: the path IS a peer's, and the
    /// guard could not put a name to them.
    pub foreign: Vec<(String, String)>, // (path, owner)
    /// Both edited it. Contested, not forbidden.
    pub shared: Vec<(String, String)>, // (path, other owner)
    /// Nobody's edit record claims it. NOT "yours" — see [`build`].
    pub unclaimed: Vec<String>,
    /// The guard could not decide.
    pub undecided: Vec<String>,
    /// The guard is partially blind (a cotenant has no transcript), so an
    /// empty `foreign` does NOT clear their files.
    pub partial: Option<String>,
}

/// The freshness axis (MG-1467): for each dirty path, WHICH DIRECTION it
/// differs from origin/main. Parallel to [`Ownership`], and like it,
/// deliberately NOT constructible from a working tree. It carries an answer the
/// caller computed with [`freshness_from_repo`] so that [`build`] stays
/// repo-blind.
///
/// Only `stale` and `same` change what build does. `new` and `edited` are both
/// ordinary commit-worthy work; they are tracked for completeness and so a
/// caller can inspect the split, but build derives "commit-worthy" as
/// everything not-same and not-stale rather than reading them. That keeps a
/// default (empty) Freshness behaving exactly like it did before this axis.
#[derive(Debug, Default, Clone)]
pub struct Freshness {
    /// origin has commits on the path that local HEAD lacks: the worktree copy
    /// is OLDER. Committing it SILENTLY REVERTS origin. Restore, do not commit.
    pub stale: Vec<String>,
    /// BOTH directions have commits on the path — novel and stale at once.
    /// Neither single-arm remedy is safe: commit reverts origin's side,
    /// restore reverts the local side, and every restore-safety test passes
    /// while doing it (locally-committed content is "reachable from a commit"
    /// too). Live specimen 2026-08-20: mixpeek .githooks/pre-push, origin
    /// carrying 96ea161803 and local HEAD carrying the MG-1483 guard — the
    /// two-bucket classifier filed it STALE and the prescribed restore
    /// silently disarmed a data-loss push guard. Merge, or hand to the owner.
    pub diverged: Vec<String>,
    /// Absent from origin/main. Genuinely new work; LOST if never committed.
    pub new: Vec<String>,
    /// Differs from origin, which has not moved past HEAD on this path. A
    /// plausible in-flight edit.
    pub edited: Vec<String>,
    /// Byte-identical to origin: dirty only because local HEAD is behind.
    /// Suppressed entirely as noise (48 of 321 on the motivating checkout).
    pub same: Vec<String>,
    /// The worktree copy is an OLD COMMITTED REVISION of this path, and HEAD
    /// and origin/main agree on the current one. Committing it silently REVERTS
    /// content both refs hold (AMUX-3695, reported by mixpeek-frustrations).
    ///
    /// Its own bucket rather than folded into `stale`, even though the remedy
    /// is the same restore. `stale` says "origin has commits local HEAD lacks",
    /// which is FALSE here — the refs agree, which is precisely why both
    /// ancestry arms are blind to it. Filing it under a bucket whose
    /// explanation does not hold is the mistake `diverged` exists to correct:
    /// the two-bucket classifier called a diverged path STALE and its
    /// prescribed restore disarmed a live push guard.
    ///
    /// Restore IS safe here, and that is checkable rather than assumed: the
    /// on-disk bytes are reachable from a commit by construction, since that
    /// is how the path was classified.
    pub revived: Vec<String>,
    /// How many paths were NOT put through the revived discriminator because
    /// the run hit its budget (AMUX-3695, measured by mixpeek-frustrations).
    ///
    /// NEVER SILENT. Those paths fall back to `edited`, which is the safe
    /// direction but is also indistinguishable from a path that was checked and
    /// found novel. A reader who cannot tell "checked, it is fine" from "we ran
    /// out of budget" has been told something false by omission, and on the
    /// checkout that motivated this the second case is the majority.
    pub revived_unchecked: usize,
    /// How many paths the discriminator ACTUALLY examined. Coverage, not a
    /// count of findings — and the share of a partial sample must never be
    /// generalised to the checkout without it (mixpeek-frustrations measured
    /// 4 of 59 checked under the default budget on a large repo, i.e. 0.5%
    /// coverage on a 772-path listing).
    pub revived_checked: usize,
    /// Which bound ended the revived sampling: "cap", "clock", or "" when it
    /// finished the list. `revived_checked: 0` is honest about coverage and
    /// silent about cause, and the two causes want different actions
    /// (AMUX-3760).
    pub revived_stopped_by: &'static str,
    /// The subset of `diverged` that the repo itself declares is GENERATED
    /// output, via a `linguist-generated` attribute in its `.gitattributes`
    /// (AF-428).
    ///
    /// The three-way merge this arm prescribes is WRONG for these, and
    /// `conflicts=0` is exactly when it bites: a merge of two generator outputs
    /// is syntactically valid, matches NEITHER source model, was produced by no
    /// code, and is overwritten by the next regen. Restore is equally wrong. The
    /// only correct remedy is to regenerate once the underlying change lands.
    ///
    /// WHY `.gitattributes` AND NOT AN AMUX LIST. The obvious source, the
    /// repo's pre-commit config, declares each hook's INPUT TRIGGER (`files:`)
    /// and not its outputs — mixpeek's generate-openapi names openapi.json only
    /// in free-text `description`, and the real paths live inside the generator
    /// script. So a config scan finds the hook and cannot learn what it writes.
    /// `linguist-generated` already means exactly this, is versioned with the
    /// code it describes, is edited by whoever adds the generator, and is read
    /// with `git check-attr` rather than parsed.
    ///
    /// NOT INFERRED FROM AN ABSENT EDIT RECORD, which was the tempting option
    /// and the dangerous one: "no record from any lane" also describes a lane
    /// whose hooks are not installed (AF-409/AF-410, live in this fleet), and
    /// would label their work as generated.
    pub generated: Vec<String>,
    /// How many diverged paths the attribute probe actually examined.
    ///
    /// PUBLISHED BESIDE `generated` BECAUSE AN EMPTY LIST HAS TWO CAUSES, and
    /// this is the whole answer to the "a declared list goes stale silently"
    /// objection. A repo with no `.gitattributes` yields zero marked paths,
    /// which is indistinguishable from a repo that checked and found none —
    /// unless the denominator is in the same payload (ethos rule 4). "0 of 7
    /// marked" reads as a missing list; "0" alone reads as an assertion.
    pub generated_checked: usize,
    /// "" when the probe ran, "error" when git could not answer. On error
    /// `generated_checked` is 0, so a failed probe can never be read as a
    /// measurement that found nothing.
    pub generated_stopped_by: &'static str,
}

/// Is the worktree copy of `path` an OLD COMMITTED REVISION rather than a new
/// edit? (AMUX-3695.)
///
/// Only meaningful where HEAD and origin/main AGREE on the path, which is the
/// one state both ancestry arms are blind to: there is no "origin has commits
/// HEAD lacks" to find, because the refs hold the same bytes. Committing an old
/// revision from there silently reverts what both agree on.
///
/// TWO QUESTIONS, CHEAPEST FIRST, and the order is the whole cost story:
///
///   1. Is the on-disk blob a known object AT ALL? A genuine new edit produces
///      bytes never committed anywhere, so this is a single hash lookup that
///      settles the common case in ~30ms with no ref walk.
///   2. Only if it is: does any commit on any ref carry that blob at this path?
///
/// Measured on this repo before shipping (141 refs, 4158 commits): step 2's
/// worst case, a full walk finding nothing, is 0.103s. That was the number the
/// card asked for and it is what made this buildable.
///
/// FAILS TOWARD `edited` on every error. A false `revived` would prescribe a
/// restore against work that is genuinely new, which destroys it; a false
/// `edited` merely under-warns, and under-warning is the direction this whole
/// gate already fails in deliberately.
async fn revives_an_old_revision(dir: &str, path: &str) -> bool {
    let hash = match tokio::process::Command::new("git")
        .args(["-C", dir, "hash-object", "--", path])
        .output()
        .await
    {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim().to_string(),
        _ => return false,
    };
    if hash.is_empty() {
        return false;
    }
    // The O(1) gate. Exit != 0 means these bytes are in no commit anywhere, so
    // the path is novel work and no walk is warranted.
    let known = tokio::process::Command::new("git")
        .args(["-C", dir, "cat-file", "-e", &hash])
        .output()
        .await
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !known {
        return false;
    }
    // The blob exists. Now the only question left is whether it was ever the
    // content OF THIS PATH — a blob can be a known object because some other
    // file has the same bytes, and calling that a revert would be wrong.
    tokio::process::Command::new("git")
        .args([
            "-C", dir, "log", "--all", "--oneline", "-1",
            &format!("--find-object={hash}"),
            "--", path,
        ])
        .output()
        .await
        .map(|o| o.status.success() && !String::from_utf8_lossy(&o.stdout).trim().is_empty())
        .unwrap_or(false)
}

/// The nudge, or None when there is nothing honest to say.
///
/// `dirty` is the list of paths `git status` reported under the session's
/// working directory — a LIST, carrying no ownership claim.
///
/// Two axes arrive pre-computed, for one reason: [`build`] is deliberately
/// unable to see a repo, so it cannot re-derive either by accident. `own` is
/// WHOSE a file is (the staged-guard's answer). `fresh` is WHICH DIRECTION it
/// differs from origin/main (MG-1467). They need opposite handling, so one
/// undifferentiated count cannot serve both. It mis-instructed five sessions in
/// three days, telling them to commit files a commit would SILENTLY REVERT.
pub fn build(
    dir: &str,
    dirty: &[String],
    own: &Ownership,
    fresh: &Freshness,
    provenance: &str,
) -> Option<String> {
    // PROVENANCE IS A REQUIRED PARAMETER, NOT A FIELD THE CALLER MAY FORGET
    // (tubescience, 2026-08-11). It shipped first as the second half of a
    // `(Vec<String>, String)` tuple, which a future caller retires with
    // `let (dirty, _) = ...` — one underscore and the warning silently stops
    // saying what it compared against, which is the exact degradation this
    // whole fix exists to make visible.
    //
    // Their formulation, from mechanising it in their own harness: "wherever
    // claims get emitted in a structured way, put the scope in the constructor
    // and make it mandatory. Prose is where scope goes to be optional." A
    // nudge is a claim about someone's uncommitted work; this signature is what
    // makes it impossible to state one without saying what it was measured
    // against. It covers the caller written months from now by someone who read
    // none of this — the population a remembered habit cannot reach.
    if dirty.is_empty() {
        return None;
    }

    // FRESHNESS is the second axis (MG-1467). Ownership answers WHOSE a file is;
    // freshness answers WHICH DIRECTION it differs from origin/main, and two of
    // its four classes flip what build says, in OPPOSITE directions. That is why
    // one count mis-instructed five sessions in three days:
    //
    //   SAME:  worktree is byte-identical to origin, dirty only because local
    //          HEAD is behind (a graft-push artifact). Nothing is wrong, so it
    //          is SUPPRESSED. 48 of 321 paths were this on the checkout that
    //          motivated the card, and they are why the raw count read alarming.
    //   STALE: origin has commits on the path that local HEAD lacks, so the
    //          worktree copy is OLDER. `git add -A` carries it forward and
    //          SILENTLY REVERTS origin (no conflict, the older file just wins,
    //          AMUX-3000). Rendered FIRST, with the RESTORE command, and never
    //          told to commit. This is the inverse of the normal nudge.
    //
    // NEW and EDITED are ordinary commit-worthy work and keep today's messaging.
    // build derives "commit-worthy" as everything NOT same and NOT stale, so a
    // default (empty) Freshness reproduces today's behaviour exactly. That is
    // also the honest state when local HEAD is not behind origin.
    let same: BTreeSet<&str> = fresh.same.iter().map(String::as_str).collect();
    let stale_set: BTreeSet<&str> = fresh.stale.iter().map(String::as_str).collect();
    let diverged_set: BTreeSet<&str> = fresh.diverged.iter().map(String::as_str).collect();
    // AMUX-3695. Classified but not rendered would be the worse bug: the path
    // would silently leave `commit_worthy` and the nudge would stop mentioning
    // it at all, so a silent revert becomes an invisible one. A bucket that
    // nothing reads is the same as no bucket (ethos rule 1).
    let revived_set: BTreeSet<&str> = fresh.revived.iter().map(String::as_str).collect();

    // Drop SAME first. If that empties the set there is nothing honest to say:
    // identical-to-origin is the same non-event as a clean tree.
    let dirty: Vec<&str> =
        dirty.iter().map(String::as_str).filter(|p| !same.contains(*p)).collect();
    if dirty.is_empty() {
        return None;
    }

    let stale_paths: Vec<&str> =
        dirty.iter().copied().filter(|p| stale_set.contains(*p)).collect();
    let diverged_paths: Vec<&str> =
        dirty.iter().copied().filter(|p| diverged_set.contains(*p)).collect();
    let revived_paths: Vec<&str> =
        dirty.iter().copied().filter(|p| revived_set.contains(*p)).collect();
    let commit_worthy: Vec<String> = dirty
        .iter()
        .copied()
        .filter(|p| {
            !stale_set.contains(*p) && !diverged_set.contains(*p) && !revived_set.contains(*p)
        })
        .map(str::to_string)
        .collect();

    // WHOSE files? (MG-1484's other half; mixpeek-general's 3-vs-20.) The
    // STALE and DIVERGED headers used to say "{n} of your dirty file(s)" over
    // sets with NO ownership filter — dirty ∩ stale on a SHARED checkout
    // includes peers' work and generated churn (an SDK regen landing on
    // origin makes its outputs read stale in every checkout behind it). The
    // pronoun was accidentally right when the recipient's touched set
    // coincided (a firing of 3) and wrong when the regen widened it (20, of
    // which 17 the addressed session had never opened — they classified the
    // noise by hand to find their 3). Same axis commit_worthy_body already
    // uses: only a positive attribution is "yours"; the rest is named as
    // such, with the owner where one is known.
    let foreign_owner: BTreeMap<&str, &str> =
        own.foreign.iter().map(|(p, o)| (p.as_str(), o.as_str())).collect();
    let unknown_owner: BTreeSet<&str> = own
        .unclaimed
        .iter()
        .chain(own.undecided.iter())
        .map(String::as_str)
        .collect();
    let not_mine = |p: &str| foreign_owner.contains_key(p) || unknown_owner.contains(p);
    let whose = |paths: &[&str]| {
        let n = paths.len();
        let n_mine = paths.iter().filter(|p| !not_mine(p)).count();
        if n_mine == n {
            "of your dirty file(s)".to_string()
        } else if n_mine == 0 {
            "dirty file(s) in this SHARED checkout (NONE carries your edit record — peers' \
             work or generated churn, e.g. an SDK regen on origin; not yours to reconcile, \
             but do not commit them either)"
                .to_string()
        } else {
            format!(
                "dirty file(s) in this SHARED checkout ({n_mine} with your edit record, {} \
                 without — peers' work or generated churn)",
                n - n_mine
            )
        }
    };
    let tagged_list = |paths: &[&str]| -> String {
        let mut mine: Vec<String> = Vec::new();
        let mut other: Vec<String> = Vec::new();
        for p in paths {
            if let Some(o) = foreign_owner.get(*p) {
                // THREE STATES, THREE STRINGS (AMUX-3816). A foreign path whose
                // owner did not resolve is neither `[<name>'s]` nor
                // `[no edit record of yours]`: the first invents a name, the
                // second says nobody owns it. mixpeek-frustrations hit exactly
                // this — they needed a peer's name to warn them before a
                // destructive commit, and `['s]` could not tell them whether
                // there was anyone to warn.
                //
                // The row states the ACTION (hands off, and you will not get
                // the name here). It deliberately does NOT name a cause: the
                // row has not measured one, and `degraded` already carries the
                // real reason when the guard knows it. Formatting something the
                // row does not know is the defect this fixes.
                if *o == UNRESOLVED_OWNER {
                    other.push(format!("  {p}  [a peer's, name unresolved]\n"));
                } else {
                    other.push(format!("  {p}  [{o}'s]\n"));
                }
            } else if unknown_owner.contains(*p) {
                other.push(format!("  {p}  [no edit record of yours]\n"));
            } else {
                mine.push(format!("  {p}\n"));
            }
        }
        let n = mine.len() + other.len();
        mine.into_iter().chain(other).take(10).collect::<String>()
            + if n > 10 { "  …\n" } else { "" }
    };

    let mut sections: Vec<String> = Vec::new();

    // DIVERGED before everything: both remedies the rest of this message
    // teaches are wrong for these paths, and the reader must hit that before
    // either recipe. The missing cell that disarmed the mixpeek MG-1483 guard
    // (2026-08-20): both directions carry commits, so commit reverts origin's
    // side, restore reverts the local side, and the restore-safety check
    // passes throughout because locally-committed content is
    // reachable-from-a-commit too.
    if !diverged_paths.is_empty() {
        // PARTITION BEFORE RENDERING (AF-428). The paragraph at the bottom of
        // this section has told readers since 05e280fd that the merge recipe is
        // wrong for generated files and left them to work out which ones those
        // are. Where the repo declares it, the arm can now do that itself, and a
        // declared path must not be handed the recipe at all: a caveat under a
        // command is read after the command.
        let gen_set: BTreeSet<&str> = fresh.generated.iter().map(String::as_str).collect();
        let generated_paths: Vec<&str> =
            diverged_paths.iter().copied().filter(|p| gen_set.contains(*p)).collect();
        let hand_paths: Vec<&str> =
            diverged_paths.iter().copied().filter(|p| !gen_set.contains(*p)).collect();
        // SAY WHAT THE PROBE COVERED, in the same payload as its result. An
        // empty `generated` has two causes (the repo marks nothing, or the probe
        // could not run) and they want different actions. See
        // Freshness::generated_checked.
        //
        // THE THREE CLAUSES BELOW COMPOSE, AND THAT IS WHY NONE OF THEM CAN BE
        // DROPPED AS REDUNDANT (mixpeek-frustrations, 2026-09-04, tracing a
        // near-miss backend had within an hour of this shipping). The chain:
        // a stale checkout reads 0 marked, because `check-attr` reads the
        // WORKING TREE and a graft-push checkout is routinely not the repo; the
        // reader concludes the list is missing; the arm hands them a method for
        // building one; and if that method were co-change they mark a generator
        // INPUT and silently delete a real merge warning. Three correct
        // components producing a wrong write, none individually wrong.
        //
        // So the zero says whose tree it is about, the guidance says which
        // evidence to trust, and the generated section says which direction of
        // error is silent. Deleting any one of them re-opens the chain.
        let coverage = match (fresh.generated_stopped_by, generated_paths.len()) {
            ("error", _) => "\n\nGENERATED-FILE CHECK DID NOT RUN: `git check-attr` failed \
                 here, so none of the above is known either way. Treat every path as \
                 hand-written, which is what the recipe above assumes."
                .to_string(),
            (_, 0) => format!(
                "\n\nNONE of these {checked} path(s) is marked generated: `git check-attr \
                 linguist-generated` returned nothing set. That is a statement about \
                 `.gitattributes` in this repo, NOT a finding that none are generated, because \
                 a repo with no such file answers exactly this way. AND IT IS READ FROM THIS \
                 CHECKOUT: on a stale HEAD the file can read as absent while origin already \
                 carries one, so `git show origin/main:.gitattributes` before you write a new \
                 one, or you will regress a better version with a worse one. If one of them \
                 IS generator output the recipe above is wrong for it, and one line fixes it \
                 for every future nudge:\n\
                 \x20 <path> linguist-generated=true",
                checked = fresh.generated_checked
            ),
            (_, g) => format!(
                "\n\n{g} of {checked} diverged path(s) are marked `linguist-generated` in this \
                 repo's `.gitattributes` and are listed under DIVERGED (GENERATED) above; the \
                 rest are treated as hand-written.",
                checked = fresh.generated_checked
            ),
        };
        if !generated_paths.is_empty() {
            let gn = generated_paths.len();
            let glist = tagged_list(&generated_paths);
            sections.push(format!(
                "DIVERGED (GENERATED): {gn} path(s) under {dir} differ in BOTH directions AND \
                 are declared generator output by this repo's `.gitattributes` \
                 (`linguist-generated`):\n{glist}\
                 DO NOT MERGE AND DO NOT RESTORE THESE. A three-way merge of two generator \
                 outputs is syntactically valid, matches NEITHER source model, was produced by \
                 no code, and is overwritten by the next regen. `conflicts=0` is exactly when \
                 that bites, because a clean merge is the result that looks safest. \
                 `git checkout origin/main -- <path>` is equally wrong: it restores an artifact \
                 that does not match the sources now in the tree, and the next commit \
                 regenerates the divergence.\n\
                 The remedy is to REGENERATE FROM SOURCE once the underlying change lands, and \
                 it belongs to the owner of the SOURCE rather than to whoever this nudge \
                 happened to wake.\n\
                 IF A PATH ABOVE IS NOT ACTUALLY GENERATED, its `.gitattributes` entry is \
                 wrong, and that error is the WORSE of the two directions: an unmarked \
                 generated file gets a recipe you can decline, while a marked hand-written \
                 file loses its merge warning silently. The trap is a generator INPUT sitting \
                 beside its outputs — a `generate.sh` or a config next to the tree it writes, \
                 which changes on every regen exactly like the outputs do. Never derive the \
                 list from which paths change together. Ask the PRODUCER, strongest first: \
                 a per-file header (`@generated`, \"DO NOT EDIT\") in the first few lines, \
                 which is a statement about THAT file and so cannot mistake a sibling input \
                 for an output; a generator manifest where one exists \
                 (openapi-generator writes `.openapi-generator/FILES`, kubb and most others \
                 write nothing); a codegen CI gate\'s own path list; and last, a directory \
                 named for it (`__generated__/`), which is the weakest and wants a second \
                 source before you mark on it."
            ));
        }
        if !hand_paths.is_empty() {
            let n = hand_paths.len();
            let list = tagged_list(&hand_paths);
            let whose_d = whose(&hand_paths);
            sections.push(format!(
            "DIVERGED: {n} {whose_d} under {dir} have commits in BOTH directions — \
             origin/main carries commits on them that your HEAD lacks, AND your HEAD carries \
             commits origin lacks. They are novel and stale AT ONCE, so NEITHER standard remedy \
             is safe:\n{list}\
             Committing carries your side forward and SILENTLY REVERTS origin's commits; \
             `git checkout origin/main -- <path>` reverts YOUR landed commits — and the \
             find-object restore-safety check PASSES while it does, because locally-committed \
             content is reachable-from-a-commit too (that is how the mixpeek MG-1483 push guard \
             was silently disarmed, 2026-08-20). Do not clear these with any single-arm command.\n\n\
             THREE-WAY MERGE, per path, non-destructive (AMUX-3718 — this arm used to say \
             \"MERGE the two versions, or hand the path to its owner\" and stop, while every \
             other arm here carries something runnable; of 16 paths one lane was told to merge, \
             they merged none):\n\
             \x20 B=$(git merge-base HEAD origin/main)\n\
             \x20 git show $B:<path>          > /tmp/base\n\
             \x20 git show origin/main:<path> > /tmp/theirs\n\
             \x20 git merge-file -p <path> /tmp/base /tmp/theirs > /tmp/merged; echo \"conflicts=$?\"\n\
             `-p` writes the merge to STDOUT and leaves <path> untouched, which matters on a \
             shared checkout where an in-place merge is a whole-file write over a peer's live \
             edit. The exit status is the conflict COUNT: 0 means git merged it cleanly and you \
             can move /tmp/merged into place; non-zero means real judgment is needed and the \
             markers show exactly where. Handing it to the owner stays the right answer when the \
             conflicts are not yours to resolve — but now you know how many there are before you \
             decide that.\n\n\
             NOT FOR GENERATED FILES, and conflicts=0 is exactly when this bites. A merge of \
             two generator outputs is syntactically valid, matches NEITHER source model, was \
             produced by no code, and is silently overwritten by the next regen — so it is \
             wrong and invisible at once. `git checkout origin/main -- <path>` is equally \
             wrong: it restores a spec that does not match the models now in the tree, and the \
             next commit regenerates the divergence. The only correct remedy for that class is \
             REGENERATE FROM SOURCE once the underlying change lands, which belongs to the \
             source owner rather than to whoever this nudge happened to wake.\n\
             HOW TO TELL, and this arm now does where the repo says so: a `linguist-generated` \
             attribute in `.gitattributes` moves a path out of this list and into DIVERGED \
             (GENERATED) above, with the regenerate remedy instead of the recipe. The repo's \
             pre-commit config is NOT the place to look it up: `files:` there declares the \
             hook's INPUT TRIGGER, not what it writes (mixpeek's generate-openapi names \
             openapi.json only in free-text `description`). Reported by mixpeek-frustrations \
             and backend, who hit this independently in one night on server/openapi.json and \
             docs/api-reference/openapi.json and both declined the recipe above on their own \
             judgement.{coverage}"
        ));
        }
    }

    if !revived_paths.is_empty() {
        let n = revived_paths.len();
        let list = tagged_list(&revived_paths);
        let whose_r = whose(&revived_paths);
        // WHEN THIS IS THE MAJORITY IT IS ONE FINDING, NOT N ALARMS
        // (mixpeek-frustrations measured 7 of 10 on a checkout 6x this repo).
        // A bucket that fires on most of the set reads as noise and gets
        // scrolled past, which is how a real silent-revert warning stops being
        // read at all. Lead with the ratio and name the checkout-level
        // condition, because at that share the story is the checkout rather
        // than any one path.
        // THE SHARE IS OF WHAT WAS CHECKED, AND ONLY GENERALISES IF COVERAGE
        // SUPPORTS IT (mixpeek-frustrations, probes 1 and 2).
        //
        // Two separate errors were possible here and both were live. First, the
        // denominator: dividing by every dirty path counts unchecked ones as
        // not-revived, which understates by exactly the coverage gap. Second and
        // worse, generalising at all — under the default budget their repo
        // checked 4 of 59 paths, so a "75% of your checkout" claim would rest on
        // four files. A checkout-level statement needs checkout-level coverage.
        //
        // A FLOOR ON THE COUNT TOO. 1 of 2 paths is 50% and is not a
        // checkout-wide condition; an existing test caught that by going red.
        let denom = fresh.revived_checked.max(n);
        // SAFE ONLY BECAUSE THE `>= 50` BRANCH BELOW IS THE SOLE RENDER SITE.
        // Integer division can take a small nonzero share to 0 (5 of 501 is 0),
        // and a rendered "0%" beside a nonzero count is not an imprecise number,
        // it is the ZERO THAT MEANS NONE — the same collision that made
        // "0% coverage" read as "nothing was checked". If you ever print
        // `share` outside that branch, give it the `<1%` treatment `cov` has.
        let share = (n * 100).checked_div(denom).unwrap_or(0);
        let coverage = fresh.revived_checked + fresh.revived_unchecked;
        let well_covered = coverage == 0 || fresh.revived_checked * 2 >= coverage;
        // A PERCENTAGE NEEDS ENOUGH OBSERVATIONS TO BE ONE (mixpeek-frustrations).
        //
        // Their granularity floor, and it is not a precision argument, it is a
        // range argument: at n=4 the only values this can produce are 0, 25, 50,
        // 75 and 100. The true share on their checkout is 63%, and that is not
        // in the estimator's range AT ANY CONFIDENCE. A share printed from four
        // observations is not a noisy measurement of the share, it is a
        // different quantity wearing a percent sign.
        //
        // Their arithmetic on how many it would take, at p=0.63 with a finite
        // population correction and the 458ms mean walk they measured:
        //   n=4    +/- 47 points     2s
        //   n=40   +/- 14 points    18s
        //   n=93   +/-  9 points    43s
        // So no cap compatible with a 2s budget supports a proportion, and the
        // answer to "is 40 the right cap" is that there is no right cap. Below
        // 10 the count IS the honest statement, so print the count.
        let enough_for_a_rate = denom >= 10;
        let lede = if share >= 50 && n >= 5 && well_covered && enough_for_a_rate {
            format!(
                "OLD REVISIONS ON DISK — {n} of the {} path(s) examined ({share}%). At this \
                 share the finding is the CHECKOUT, not the individual files: it is broadly \
                 carrying content that was already superseded. Treat this as ONE condition to \
                 resolve deliberately, not {n} separate alarms",
                fresh.revived_checked.max(n)
            )
        } else {
            format!("OLD REVISION ON DISK: {n} {whose_r} under {dir}")
        };
        sections.push(format!(
            "{lede} — content that was ALREADY COMMITTED at some point, while HEAD and \
             origin/main agree on the current version:\n{list}\
             Committing these SILENTLY REVERTS what both refs hold. This is invisible to the \
             usual stale check precisely BECAUSE the refs agree, so there is no \
             'origin is ahead' to find — it was reported as the one population the \
             refs-agree gate did not split (AMUX-3695).\n\
             Confirm with: git log --all --oneline --find-object=$(git hash-object <path>) -- <path>\n\
             A commit printing there IS the old revision. `git checkout origin/main -- <path>` \
             is safe here and loses nothing, because the on-disk bytes are reachable from that \
             commit — which is how the path was classified in the first place. If you PUT the \
             old content there deliberately, commit it and say so; nothing here can tell an \
             intentional revert from an accidental one, and that call is yours."
        ));
    }

    // NO SILENT CAP. On a large repo the discriminator runs out of budget long
    // before the dirty list ends, and those paths land in `edited` — the safe
    // fallback, and also the exact output a path that WAS checked and found
    // novel produces. Saying nothing here would let "we did not look" read as
    // "we looked and it is fine", which is the difference between a bound and a
    // lie. Measured need: 772 paths in one real listing (mixpeek-frustrations).
    if fresh.revived_unchecked > 0 {
        let total = fresh.revived_checked + fresh.revived_unchecked;
        // Coverage is an EXACT quantity (we counted both sides), not an
        // estimate, so a percentage is legitimate here at any n — unlike the
        // share above, which is a sample statistic. Rendered only when it adds
        // something the two counts do not: 4-of-59 is much easier to feel as
        // "6%", and 2-of-6 is not.
        let pct = (fresh.revived_checked * 100).checked_div(total).unwrap_or(0);
        // "<1%", NEVER "0%", when anything was actually examined.
        //
        // Integer division takes 4-of-524 — mixpeek-frustrations' real shape —
        // to exactly 0, and their naming of why that matters is the reusable
        // part: truncation turned a small nonzero into THE ZERO THAT MEANS
        // NONE. Not an imprecise number, a different claim. "0% coverage"
        // beside a sentence saying four paths were examined is two fields
        // contradicting each other, and the reader believes the number.
        //
        // Same family as a can't-tell rendering as a known-good, an unreachable
        // cell rendering as a pass, and an all() over an empty set rendering as
        // success: the value meaning "we did not look" and the value meaning
        // "we looked and found little" must stay distinguishable, and
        // arithmetic collapses them without anyone writing the collision.
        //
        // The counts carry the actionable part either way, which is why they
        // are printed beside it rather than replaced by it: at these ratios the
        // percentage is atmosphere and "4 of 524" is the thing a reader can use.
        let cov = if total < 10 {
            String::new()
        } else if pct == 0 && fresh.revived_checked > 0 {
            " (<1% coverage)".to_string()
        } else {
            format!(" ({pct}% coverage)")
        };
        // NAME THE BOUND THAT BIT (AMUX-3760). "hit its budget" names two knobs
        // and leaves the reader to guess which, and they want opposite actions:
        // a cap hit means raise the cap, a clock hit means the machine is loaded
        // or the walks are slow. Before this, a clock hit at ZERO checked was
        // also how the revived discriminator silently stopped running under load
        // — the budget clock was started before the classification phase and was
        // already spent when the sampling began.
        let why = match fresh.revived_stopped_by {
            "cap" => " — it stopped on the PATH CAP (AMUX_NUDGE_REVIVED_MAX_PATHS); raise that to examine more",
            "clock" => " — it stopped on the CLOCK (AMUX_NUDGE_REVIVED_BUDGET_MS); the walks are slow or the machine is loaded, so raising the cap alone will not help",
            _ => "",
        };
        sections.push(format!(
            "NOT CHECKED FOR OLD-REVISION: {} of {total} candidate path(s) were examined\
             {cov}; the other {} were not, because this run hit its budget{why} \
             (AMUX_NUDGE_REVIVED_BUDGET_MS / AMUX_NUDGE_REVIVED_MAX_PATHS). The unchecked ones \
             are listed above as ordinary edits, which is the SAFE fallback and NOT a finding \
             that they are clean.\n\
             COVERAGE, STATED AS A PERCENTAGE ON PURPOSE: on a large checkout the per-path \
             test costs ~700ms and the clock cuts after about four paths, so a long dirty list \
             gets low single digits. That is an honest bound and it is NOT a sample — do not \
             read the proportions above as representative of the rest. Measured: reporting a \
             share to within 10 points would need ~93 paths and ~43s, so there is no cap \
             compatible with a 2s budget that supports one. The examined paths are allocated \
             across top-level directories in PROPORTION to their size, which removes both the \
             alphabetical bias and the equal-weight-per-directory bias that replacing it \
             introduced — neither of which makes four files a survey.\n\
             To check one by hand:\n  \
             git log --all --oneline --find-object=$(git hash-object <path>) -- <path>",
            fresh.revived_checked, fresh.revived_unchecked
        ));
    }

    // STALE FIRST and most prominently. The opposite instruction to the rest of
    // the nudge: do NOT commit these, restore them.
    //
    // RESTORE-SAFETY DISCRIMINATOR (AMUX-3264, cold-outbound live near-miss
    // 2026-08-17). Once a path is known behind origin, the remaining question is
    // pure-old-copy (safe to `git checkout origin/main -- <path>`) vs
    // carries-novel-mid-edit (a restore DELETES it irreversibly). The advice
    // below prescribes `git log --all --oneline --find-object=<blob> -- <path>`,
    // which answers by REACHABILITY FROM A COMMIT: non-empty means the exact
    // content is in a commit somewhere, empty means it is in none.
    //
    // Both `--all` and the `-- <path>` pathspec look droppable and are NOT:
    //   * `--all` (not HEAD-only): a blob committed on another branch or on
    //     origin reads EMPTY under a HEAD-only search and would be misjudged
    //     novel. `--all` widens what counts as committed so a genuine old copy
    //     living elsewhere is still recognised.
    //   * `-- <path>` (cold-outbound): a blob committed under a DIFFERENT path (a
    //     move, a copied fixture) reads not-found for THIS path.
    // Every residual edge case (other-path, an errored or timed-out command, an
    // empty result) yields a FALSE "novel", which the advice resolves to
    // DO-NOT-RESTORE. That bias is deliberate and MUST NOT be tightened back
    // toward the destructive side: a declined restore is recoverable, a deleted
    // keystroke is not. The unsound recipe this replaces,
    // `git cat-file -e $(git hash-object <path>)`, failed OPEN: `git add` alone
    // writes the blob into the object DB without committing, so it answered EXISTS
    // for a never-committed mid-edit and its `git checkout` remedy deleted it.
    if !stale_paths.is_empty() {
        let n = stale_paths.len();
        let list = tagged_list(&stale_paths);
        let whose_s = whose(&stale_paths);
        sections.push(format!(
            "STALE: {n} {whose_s} under {dir} are behind origin/main — origin has commits on \
             these paths that your local HEAD does not. THAT IS A FACT ABOUT HISTORY, NOT ABOUT \
             YOUR BYTES, and it does not tell you what to do with them:\n\
             {list}\
             RUN THE PER-PATH TEST FIRST. The two remedies are opposites and this label cannot \
             pick between them — measured 2026-09-03 by general-canvas-apps and \
             mixpeek-homepage-claude independently, FOUR OF FOUR paths carrying this label were \
             NOVEL mid-edits a restore would have deleted, i.e. the answer was the opposite one \
             every time. Two lanes reaching that verdict separately is why the test leads here \
             now and the label does not.\n\
             \x20 git log --all --oneline --find-object=$(git hash-object <path>) -- <path>\n\
             PRINTS A COMMIT -> a genuine old copy. RESTORE it (`git checkout origin/main -- \
             <path>`); do NOT commit it, because `git add -A` or `git commit -a` carries the \
             older copy forward and SILENTLY REVERTS origin — no conflict, the older file just \
             wins (AMUX-3000).\n\
             ANYTHING ELSE -> DO NOT RESTORE; COMMIT the path instead. An empty result means \
             this content is in no commit on any ref: a novel mid-edit a restore DELETES \
             irreversibly (AMUX-3172/AMUX-3188; social-media caught 16 such paths whose \
             worktree matched NEITHER local HEAD nor origin). An error, a timeout, or a result \
             you cannot read is also DO-NOT-RESTORE, because a declined restore is recoverable \
             and a deleted keystroke is not.\n\
             The rest of this is why the obvious shortcut does not work. Behind-on-history does \
             not prove the \
             worktree is a pure old copy: a path can be behind origin AND carry NOVEL \
             uncommitted content, which is exactly the four-of-four case above. Do NOT substitute `git cat-file -e $(git hash-object <path>)`: `git add` (and \
             `git hash-object -w`) writes the blob into the object DB WITHOUT committing, so it \
             answers yes for a never-committed mid-edit and cannot separate a committed old copy \
             from novel work. It is strictly weaker than was-this-committed and its remedy here is \
             a delete (cold-outbound, 2026-08-17: server-fast-checks.yml was mid-keystroke and in \
             no commit on any ref, yet that recipe answered yes; restoring would have deleted it)."
        ));
    }

    // The commit-worthy body: NEW/EDITED paths, rendered under the ownership
    // axis exactly as before the freshness split.
    if let Some(body) = commit_worthy_body(dir, &commit_worthy, own) {
        sections.push(body);
    }

    if sections.is_empty() {
        // Everything was SAME (returned above) or the paths left were
        // foreign/unknown with nothing positively ours: no stale warning and
        // nothing to commit.
        return None;
    }
    let mut msg = sections.join("\n\n");
    // THE APPEND-ONLY NOTE BELONGS TO THE WHOLE DIRTY SET, NOT TO ONE ARM
    // (AMUX-3718, near-miss 2026-08-25).
    //
    // It used to be emitted from inside `commit_worthy_body`, which `build`
    // hands `commit_worthy` — defined three lines up as the paths that are NOT
    // stale/diverged/revived. So the archive check was structurally unreachable
    // for a DIVERGED frustrations.md: the ONE state in which this nudge
    // actually prescribes a union-merge was the one state that could not be
    // told how to perform it safely. A lane followed the bare directive
    // verbatim and would have resurrected an entry closed on a 692/692 prod
    // measurement.
    //
    // Its own unit test was green throughout, because it called
    // `commit_worthy_body` directly and pinned a layer the broken path does not
    // flow through (ethos rule 7 / AF-161). Hoisting it here means the note
    // travels with EVERY arm, and `dirty` is the honest input: the note is
    // about the file, not about which remedy the file happens to be under.
    if let Some(note) = append_only_note(&dirty) {
        msg.push_str(&note);
    }
    // SAME SHAPE, ONE INSTANCE OVER (flagged by mixpeek-frustrations while
    // reviewing the fix above; measured with a positive control before acting).
    //
    // `ATTRIBUTION IS PARTIAL` was also emitted only from `commit_worthy_body`,
    // so a nudge whose every path is DIVERGED — commit_worthy empty, that
    // function never called — dropped it. Probe: diverged+partial rendered
    // false, commit-worthy+partial rendered true.
    //
    // It matters most exactly where it went missing. The DIVERGED arm's
    // prescribed exit is "hand the path to its owner", and this caveat is the
    // one that says the ownership axis is unreliable. The arm that most needs
    // to know attribution is blind was the arm that could not be told.
    //
    // The generalisation, worth more than either fix: A SAFETY NOTE ATTACHED TO
    // THE HEALTHY BRANCH OF A CONDITIONAL CANNOT REACH THE UNHEALTHY ONE. When
    // you write a warning, check which states actually receive it. Caveats
    // about the WHOLE dirty set belong here, at the top level, never inside an
    // arm — an arm-scoped emitter silently scopes the warning to that arm.
    if let Some(why) = &own.partial {
        msg.push_str(&format!("\n\nATTRIBUTION IS PARTIAL — {why}"));
    }
    // AF-438, and it belongs HERE for the reason the block above states: it is
    // a fact about the whole path list, so an arm-scoped copy would reach one
    // reader in four. `git status --porcelain` emits root-relative paths, and
    // git PATHSPECS are cwd-relative, so a reader in a subdirectory who follows
    // any remedy verbatim runs it against the wrong path — silently, with every
    // command exiting 0. mvs-pitr hit this: the notice named
    // `.../mixpeek/server/mvs/ME` for a file at `.../mixpeek/ME`.
    msg.push_str(&format!(
        "\n\nPATHS ABOVE ARE REPO-ROOT-RELATIVE, and git pathspecs are CWD-relative — so \
         run every remedy from {dir}, or prefix it: `git -C {dir} <remedy>`. From a \
         subdirectory the same command resolves <path> against your cwd and silently \
         targets a different file, or none, exiting 0 either way."
    ));
    // AF-135 defect 1: the message timestamped origin's tip but never said
    // when it OBSERVED the tree, so a snapshot composed before a commit and
    // delivered at the next turn boundary read as live and named files
    // already committed minutes earlier. Harmless on the commit branch (a
    // no-op); on the STALE branch the same lag prescribes `git checkout
    // origin/main -- <path>` against paths origin does not have, which
    // deletes them. Stamp the observation so the reader compares it against
    // their own last commit in one glance.
    msg.push_str(&format!(
        "\n\n({provenance}; tree observed {}Z — if you committed AFTER that moment this \
         nudge predates it: re-run `git status` before acting on any remedy)",
        chrono::Utc::now().format("%H:%M:%S")
    ));
    Some(msg)
}

/// The commit-worthy body: paths that are neither STALE nor identical to origin,
/// rendered under the ownership axis (mine / unknown / foreign / shared). Returns
/// the message WITHOUT the trailing provenance so [`build`] can stamp it exactly
/// once across a STALE section and this body.
/// An append-only, multi-writer shared file — `frustrations.md` is the canonical
/// one (matched case-insensitively; macOS resolves FRUSTRATIONS.md to the same
/// file). Its whole failure mode is that the two direction remedies this nudge
/// prescribes BOTH lose data on it, so it needs its own directive.
fn is_append_only_shared(path: &str) -> bool {
    std::path::Path::new(path)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(path)
        .eq_ignore_ascii_case("frustrations.md")
}

/// The union-merge directive for any append-only shared file in the dirty set,
/// or None if there is none. Appended to whichever nudge fires (AMUX-3367): an
/// append-only file can be NOVEL AND STALE at once — you appended locally while a
/// peer landed different entries on origin — so committing the path REVERTS the
/// peer's entries and restoring DELETES yours. The direction test cannot separate
/// these because both are true, so name the file and prescribe the only safe
/// operation, which neither generic remedy performs.
///
/// CORRECTED by CD-78 (creative-dna retracting their own AMUX-3367 directive,
/// 2026-08-21): these files are not truly append-only — an entry legitimately
/// LEAVES via a companion archive (FRUSTRATIONS_ARCHIVE.md), so a blind
/// re-append re-injects deliberately archived entries. Measured on the shared
/// mixpeek checkout: 15 of 15 "lost" entries were archive moves, zero were
/// lost work, and the restore/remove cycle had run three times on origin
/// (d89dcf843a "restore 13 entries" -> 1391eed484 "12 archived entries came
/// back"). The general form: a set-difference over ONE file cannot see a
/// move and reports it as a deletion every time — before treating a
/// disappearance as loss, look where it may have legitimately moved to.
fn append_only_note(dirty: &[&str]) -> Option<String> {
    let mut ao: Vec<&str> = dirty.iter().copied().filter(|p| is_append_only_shared(p)).collect();
    ao.sort();
    ao.dedup();
    if ao.is_empty() {
        return None;
    }
    Some(format!(
        "\n\nAPPEND-ONLY SHARED FILE — {} can be NOVEL AND STALE at once (you and a peer \
         appended to different bases), so BOTH remedies above lose data on it: committing the \
         path REVERTS the peer's origin-only entries, `git checkout origin/main -- <path>` \
         DELETES yours. UNION-MERGE instead — `git checkout origin/main -- <path>` to take their \
         version, then re-append ONLY YOUR OWN entries, with the ARCHIVE CHECK first (CD-78 \
         corrected AMUX-3367): an entry can legitimately LEAVE this file by moving to its \
         companion archive (e.g. FRUSTRATIONS_ARCHIVE.md), so before re-appending anything \
         'missing', grep the archive for it — present there means the deletion was a deliberate \
         move, and re-appending it manufactures a duplicate (measured: 15 of 15 'lost' entries \
         were archive moves; the restore/remove cycle ran three times on origin). Only an entry \
         absent from BOTH files is lost work. Never a plain add/commit or a bare restore.",
        ao.join(", ")
    ))
}

/// The delivered-text invariant behind the AMUX-3718 WARN: if an append-only
/// shared file is in the dirty set, the message MUST carry CD-78's archive
/// check. Returns the offending paths when it does not.
///
/// Deliberately checks the RENDERED message rather than re-deriving which arm
/// fired. Re-deriving is how the original defect survived: the note's own test
/// reasoned about `commit_worthy` and was correct about it, while the bytes a
/// lane received had no archive check in them at all.
/// Takes `fresh` so it can SHARE the predicate of the code it describes rather
/// than re-derive it: `build` drops paths byte-identical to origin before it
/// renders anything, so a SAME frustrations.md legitimately produces no note and
/// a checker that missed that would cry wolf on healthy text. A view that
/// disagrees with the mechanism it reports on is wrong in whichever direction it
/// disagrees, and a WARN nobody trusts is worse than no WARN.
fn missing_archive_check(dirty: &[String], fresh: &Freshness, msg: &str) -> Option<String> {
    if msg.contains("ARCHIVE CHECK") {
        return None;
    }
    let same: BTreeSet<&str> = fresh.same.iter().map(String::as_str).collect();
    let bad: Vec<&str> = dirty
        .iter()
        .map(String::as_str)
        .filter(|p| is_append_only_shared(p) && !same.contains(*p))
        .collect();
    if bad.is_empty() {
        return None;
    }
    Some(bad.join(", "))
}

fn commit_worthy_body(dir: &str, dirty: &[String], own: &Ownership) -> Option<String> {
    if dirty.is_empty() {
        return None;
    }
    // MINE means POSITIVELY ATTRIBUTED TO ME, never "not proven to be someone
    // else's" (AMUX-2638, reopened by Ethan 2026-08-10).
    //
    // The first port filtered out `foreign` and treated everything else as
    // mine. That is the same bug in a subtler dress: it told Ethan to "commit
    // completed work now" about CLAUDE.md, which he had never touched — the
    // guard classified it `unclaimed`, meaning NO session has an edit record
    // for it, and specifically he had no claim. Near-certainly a peer's
    // in-flight edit (last commit to that file was amux-homepage doing exactly
    // that work).
    //
    // "Not attributable to a peer" is not evidence that it is yours. Only a
    // positive claim is.
    let foreign_paths: BTreeSet<&str> = own.foreign.iter().map(|(p, _)| p.as_str()).collect();
    let unknown_paths: BTreeSet<&str> = own
        .unclaimed
        .iter()
        .map(String::as_str)
        .chain(own.undecided.iter().map(String::as_str))
        .collect();

    let mine: Vec<&String> = dirty
        .iter()
        .filter(|p| !foreign_paths.contains(p.as_str()) && !unknown_paths.contains(p.as_str()))
        .collect();
    let unknown: Vec<&String> =
        dirty.iter().filter(|p| unknown_paths.contains(p.as_str())).collect();

    if mine.is_empty() {
        // Nothing is positively yours. Saying "commit completed work now" here
        // is the instruction that cost this checkout three sweeps in two days.
        // But silence is also wrong when the tree is dirty and nobody can say
        // whose it is — so report the uncertainty AS uncertainty.
        if unknown.is_empty() {
            return None;
        }
        let n = unknown.len();
        let list: String = unknown.iter().take(10).map(|f| format!("  {f}\n")).collect();
        // Not `mut`: the partial-attribution caveat used to be appended here and
        // now renders once in `build`, so it reaches every arm (AMUX-3718).
        let m = format!(
            "You went idle with {n} uncommitted change(s) under {dir} whose OWNERSHIP IS \
             UNKNOWN — no session has an edit record for {}:\n{list}\n\
             Do NOT assume {} yours. `git add -A` here would commit whatever a peer is \
             mid-edit on — OR a STALE copy origin/main has moved past, which committing would \
             silently REVERT (no conflict; the older file just wins, AMUX-3000). For each, \
             prove the direction with the ANCESTRY test, NOT the diff line-count: \
             worktree-has-more/has-less misclassifies about 2 in 10 (studio-plg measured it on \
             this checkout: files with MORE lines than origin that are nonetheless stale). \
             FIRST compare BLOBS, which is what the classifier gates on and what the commit \
             graph cannot tell you: `git rev-parse HEAD:<path> origin/main:<path>` — EQUAL \
             sha1s mean origin holds your bytes already, so there is nothing to revert and you \
             just commit. GRAFTING is why: a graft-push rebuilds your landed work as a new \
             commit on origin\'s tip, same content, different sha, so it returns as \"a commit \
             your HEAD lacks\". Measured 40.6% false flags on the mixpeek checkout, 737 of 1815 \
             (AF-471). ONLY IF THE BLOBS DIFFER: \
             `git log --oneline HEAD..origin/main -- <path>`: prints a commit = origin has work \
             you lack = STALE, so do not commit; restore with `git checkout origin/main -- <path>` \
             ONLY after `git log --all --find-object=$(git hash-object <path>) -- <path>` prints a \
             commit (empty or errored means the stale copy carries novel mid-edit a restore would \
             DELETE, so commit the path instead) AND the reverse test \
             `git log --oneline origin/main..HEAD -- <path>` prints NOTHING — if BOTH directions \
             print commits the path has DIVERGED (novel and stale at once) and either single-arm \
             remedy destroys one side's landed work: MERGE the versions or hand the path to its \
             owner; \
             prints NOTHING = origin has nothing you lack, so it is current content, and if it \
             is not yours it is a peer's mid-edit: hands off either way. Do NOT use \
             `git cat-file -e $(git hash-object <path>)` for this: blob existence cannot separate \
             an OLD revision from a CURRENT one that is merely unpushed, so on a checkout ahead \
             of origin it calls the whole tree STALE and its remedy reverts it. It also answers \
             YES for a never-committed mid-edit, because `git add` writes the blob into the \
             object DB without committing — and THIS arm is where that matters most, since \
             ownership being unknown is exactly the state a peer's uncommitted work is in \
             (cold-outbound, 2026-08-17: a file mid-keystroke and in no commit on any ref, which \
             that recipe answered yes for; the restore it green-lit would have deleted it). \
             Stage only what you recognise as your work AND whose ancestry test prints nothing.",
            if n == 1 { "it" } else { "them" },
            if n == 1 { "it is" } else { "they are" },
        );
        return Some(m);
    }

    let n = mine.len();
    let sample: String = mine
        .iter()
        .take(10)
        .map(|f| format!("  {f}\n"))
        .collect::<String>()
        + if n > 10 { "  …\n" } else { "" };

    let mut msg = format!(
        "You went idle with {n} uncommitted change(s) under your working directory ({dir}):\n\
         {sample}\n\
         A DIFFERENCE FROM origin/main IS NOT A DIRECTION (AMUX-3000). Each of these differs \
         from origin/main, but the guard does not know which side is NEWER — and on this \
         graft-push checkout the worktree is frequently the OLDER side. A stale worktree copy \
         is a LOADED REVERT: `git add -A` carries the older version forward and silently undoes \
         the newer one on origin (no conflict, the older file just wins). So BEFORE staging, \
         PROVE the direction per path with the ANCESTRY test. Do NOT read the line-count of a \
         `git diff` as the verdict: worktree-has-more/has-less misclassifies about 2 in 10 \
         (studio-plg measured it on this checkout: files with MORE lines than origin that are \
         nonetheless stale, because origin has since moved to a shorter version). Roughly 1 in 4 \
         differing paths here are novel mid-edit a blind `checkout` would DESTROY irreversibly, \
         so the direction test is the headline, not a caveat:\n\
         \u{2022} FIRST compare the BLOBS, which is what the classifier does and what the \
         commit graph cannot tell you: `git rev-parse HEAD:<path> origin/main:<path>`. If the \
         two sha1s are EQUAL, origin holds the same bytes your HEAD does and there is nothing \
         of origin's to revert, whatever the commit graph says. Skip to the ordinary commit. \
         GRAFTING is why this comes first: a graft-push builds a NEW commit onto origin\'s tip, \
         so your own landed work returns as a commit your HEAD lacks under a different sha with \
         identical content. Measured on the mixpeek checkout 2026-09-04 (behind 1681, ahead \
         767) by mixpeek-general, reproduced by amux-frustrations and sampled again by \
         mixpeek-cicd: of 1815 paths the commit test flags, 1080 actually differ. 737 false \
         flags, 40.6%. `--find-object` below survives grafting for the same reason this does — \
         it asks about CONTENT reachability, and grafting changes commit identity while \
         preserving content (AF-471).\n\
         \u{2022} ONLY IF THE BLOBS DIFFER does the commit graph mean anything: \
         `git log --oneline HEAD..origin/main -- <path>`: if it prints ANY commit, \
         origin has work on this path that your HEAD lacks, so the worktree copy is genuinely \
         older (STALE); do not commit. Restore with `git checkout origin/main -- <path>` ONLY \
         after BOTH: `git log --all --find-object=$(git hash-object <path>) -- <path>` prints a \
         commit (empty or errored means the stale copy carries novel mid-edit a restore would \
         DELETE, so commit the path instead), AND the REVERSE test \
         `git log --oneline origin/main..HEAD -- <path>` prints NOTHING.\n\
         \u{2022} if BOTH directions print commits, the path has DIVERGED — novel AND stale at \
         once — and commit and restore each destroy one side's landed work while every test \
         above passes as prescribed. Neither single-arm remedy is safe; three-way merge it, \
         non-destructively (AMUX-3718 — this line used to stop at 'MERGE the two versions' with \
         no procedure): `B=$(git merge-base HEAD origin/main); git show $B:<path> >/tmp/base; \
         git show origin/main:<path> >/tmp/theirs; git merge-file -p <path> /tmp/base \
         /tmp/theirs >/tmp/merged; echo conflicts=$?` — `-p` leaves <path> untouched (an \
         in-place merge is a whole-file write over a peer's live edit) and the exit status is \
         the conflict COUNT, so you know whether it is yours to resolve before you hand it to \
         the owner. Live specimen 2026-08-20: mixpeek \
         .githooks/pre-push, origin carrying 96ea161803 and HEAD carrying the MG-1483 guard \
         chain — the one-direction protocol read it STALE and the prescribed restore silently \
         disarmed a data-loss push guard.\n\
         \u{2022} if it prints NOTHING, origin has nothing you lack: the content is yours to \
         keep, `checkout` would DESTROY it, and the safe action is COMMIT, not restore.\n\
         This is the same predicate the guard itself classifies with, so its verdict and your \
         check cannot disagree. Do NOT substitute \
         `git cat-file -e $(git hash-object <path>)`: blob existence cannot tell an OLD revision \
         from a CURRENT one that is merely unpushed, because both were committed at some point \
         and both answer yes. On a checkout that sits ahead of origin every committed file \
         answers yes, so that test reports STALE for the entire tree and its remedy reverts it \
         (AMUX-3000 follow-up, measured 2026-08-16: five committed-but-unpushed paths, five \
         false STALEs, one of them a whole feature shipped that day).\n\
         When the check is ambiguous or you cannot run it, commit — a redundant commit is \
         recoverable and a restore is not. (A `git diff` still shows WHAT changed; just never \
         read worktree-has-more as newer.)\n\
         Commit only the paths whose ancestry test prints nothing, with a clear message; \
         WIP-commit anything intentionally incomplete and say so. Don't leave the working tree \
         dirty — but don't commit a revert to clear it, either."
    );

    if !unknown.is_empty() {
        let list: Vec<&str> = unknown.iter().take(4).map(|s| s.as_str()).collect();
        msg.push_str(&format!(
            "\n\nOWNERSHIP UNKNOWN — {} also dirty, with no edit record from any session. \
             Not counted above and not necessarily yours; check before staging.",
            list.join(", ")
        ));
    }
    if !own.shared.is_empty() {
        // AF-135 defect 2: "(unknown)" is the NO-PEER placeholder from the
        // server's shared branch (AF-24), not a session name — "also edited
        // by (unknown)" asserts a co-editor who does not exist, on a line
        // whose whole argument is that a NAMED peer has in-flight work. Say
        // the real fact instead.
        type Row<'a> = Vec<&'a (String, String)>;
        let (named, unowned): (Row, Row) =
            own.shared.iter().partition(|(_, w)| w.as_str() != "(unknown)");
        if !named.is_empty() {
            let who: BTreeSet<&str> = named.iter().map(|(_, w)| w.as_str()).collect();
            let paths: Vec<&str> = named.iter().take(4).map(|(p, _)| p.as_str()).collect();
            msg.push_str(&format!(
                "\n\nCONTESTED — {} also edited by {}. Stage per-HUNK (`git add -p`), not per-file: \
                 `git add <file>` takes their in-flight hunks too.",
                paths.join(", "),
                who.into_iter().collect::<Vec<_>>().join("/")
            ));
        }
        if !unowned.is_empty() {
            let paths: Vec<&str> = unowned.iter().take(4).map(|(p, _)| p.as_str()).collect();
            msg.push_str(&format!(
                "\n\nCO-EDIT RECORDS, UNATTRIBUTED — {}: edit records beyond yours exist but \
                 name no session. Not a named co-editor (the no-peer shape, AF-24); stage \
                 per-hunk (`git add -p`) if you are unsure which hunks are currently yours.",
                paths.join(", ")
            ));
        }
    }

    if !own.foreign.is_empty() {
        let who: BTreeSet<&str> = own.foreign.iter().map(|(_, w)| w.as_str()).collect();
        let paths: Vec<&str> = own.foreign.iter().take(4).map(|(p, _)| p.as_str()).collect();
        let (was, it) = if own.foreign.len() == 1 { ("was", "it") } else { ("were", "them") };
        msg.push_str(&format!(
            "\n\nNOT YOURS — {} {} edited by {} and NOT by you. Do not commit {}: \
             `git add -A` or `git commit -a` would sweep a peer's in-flight work into your \
             commit under your name. Stage only the files you touched.",
            paths.join(", "),
            was,
            who.into_iter().collect::<Vec<_>>().join("/"),
            it
        ));
    }
    Some(msg)
}


// ---------------------------------------------------------------------------
// The firing path
// ---------------------------------------------------------------------------

use crate::api::AppState;
use serde_json::{json, Value};

/// Once per session per UTC day. Python's own audit found 87 nudges/day against
/// 75 human sends — each one a full-context turn into a cold-cache idle session,
/// the single largest automated token stream. A reminder that arrives twelve
/// times is not a reminder.
fn cap_key(session: &str, now: f64) -> String {
    let day = (now / 86_400.0).floor() as i64;
    format!("commit_nudge:{session}:{day}")
}


/// Drop tracked paths whose CONTENT already matches `origin/main`.
///
/// Reported by tubescience 2026-08-10, measured rather than argued: 6 of 7
/// paths in one ownership warning were BYTE-IDENTICAL to origin. They were not
/// edits at all.
///
/// The cause is a workflow, not a bug in git: several lanes land work with
/// scripts/graft-push.sh, which pushes a tree built from origin WITHOUT moving
/// the local branch. Local HEAD therefore sits permanently behind origin, and
/// every file anyone has successfully landed reads as modified forever after.
/// `git status` is answering "how does the worktree differ from local HEAD",
/// while the warning asks "what might a peer be mid-edit on" — an instrument
/// answering a question adjacent to the one asked.
///
/// It cost a real decision: a session read this signal on tenant_canary.py,
/// concluded a peer was mid-edit, and declined a three-line fix. The file was
/// identical to origin. And a warning wrong 6 times in 7 gets skimmed, so the
/// ONE real entry — an untracked draft that `git add -A` would genuinely have
/// swept up — arrived already discounted.
///
/// UNTRACKED IS NOT A SYNONYM FOR ABSENT-FROM-ORIGIN, and an earlier version of
/// this comment said it was. Reported by creative-dna and reproduced: a file
/// ADDED on origin after local HEAD reads `??` from `git status`, because the
/// local index predates the commit that added it. Untracked is the same
/// artifact for adds that dirty-status is for edits. So the test is PRESENCE ON
/// ORIGIN, never the porcelain letter — a `??` path that matches origin is a
/// phantom and is dropped, while a `??` path genuinely absent from origin is
/// the real unprotected-WIP case and is always kept.
async fn drop_paths_identical_to_origin(dir: &str, paths: Vec<String>) -> (Vec<String>, String) {
    // REFRESH origin FIRST, and say what we compared against (tubescience).
    //
    // Nothing in this job fetched, so the comparison read whatever the LOCAL
    // origin/main ref happened to be — current only if some session had been
    // fetching for its own reasons. Against a frozen ref every path landed
    // since fails equality, gets kept, and the warning degrades straight back
    // to the noise this filter removes.
    //
    // The danger is not the noise, it is that THE DEGRADED STATE IS
    // INDISTINGUISHABLE FROM THE PRE-FIX STATE: same warning, same phantoms,
    // and nobody files a second report because it looks like the bug already
    // fixed. So the fix goes quiet without anyone learning it went quiet.
    // A filter that can silently stop filtering must say what it filtered
    // against — hence the returned provenance string, which the warning prints.
    let fetched = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        tokio::process::Command::new("git")
            .args(["-C", dir, "fetch", "--quiet", "origin", "main"])
            .output(),
    )
    .await
    .ok()
    .and_then(|r| r.ok())
    .map(|o| o.status.success())
    .unwrap_or(false);

    let ref_age = tokio::process::Command::new("git")
        .args(["-C", dir, "log", "-1", "--format=%cr", "origin/main"])
        .output()
        .await
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty());

    let provenance = match (fetched, ref_age.as_deref()) {
        (true, Some(age)) => format!("compared against origin/main (just fetched; tip {age})"),
        (false, Some(age)) => format!(
            "compared against a STALE origin/main (fetch failed; tip {age}) —              paths landed since then will be listed even if they match origin"
        ),
        (_, None) => "NOT compared against origin (no origin/main ref) — every path listed".into(),
    };
    if ref_age.is_none() {
        // Cannot compare at all: keep everything. Noisy beats silent.
        return (paths, provenance);
    }

    // RESOLVE THE REPO ROOT AND COMPARE FROM THERE (AMUX-2947).
    //
    // `git status --porcelain` emits REPO-ROOT-relative paths, but `dir` is the
    // lane's CC_DIR, which is frequently a SUBDIRECTORY — creative-dna's is
    // /Users/ethan/Dev/mixpeek/gtm/creative-dna. Run from there,
    // `git hash-object -- .github/workflows/x.yml` resolves against the CWD and
    // dies with "could not open 'gtm/.github/workflows/x.yml'". The command
    // fails, the match arm below cannot compare, and its deliberately
    // conservative `_ => false` keeps the path.
    //
    // So the filter degraded to a NO-OP for every lane not sitting at a repo
    // root, and it did so silently while the provenance line kept saying
    // "compared against origin/main (just fetched)". Measured by creative-dna
    // across all 55 paths in one report: 34 byte-identical to origin, i.e. 62%
    // false, on a guard whose own text warns the reader those files may be a
    // peer's mid-edit work. A guard that is wrong most of the time trains
    // people to skim it, and the 19 real ones were in there.
    //
    // `cat-file -e origin/main:<path>` is why it was not obvious: that one
    // addresses the OBJECT DB and is root-relative, so it succeeds from any
    // subdir. Existence checks passed, content checks failed, everything was
    // kept.
    let root = tokio::process::Command::new("git")
        .args(["-C", dir, "rev-parse", "--show-toplevel"])
        .output()
        .await
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| dir.to_string());
    let dir = root.as_str();

    let mut kept = Vec::new();
    for p in paths {
        // Does the path exist on origin at all? THE TRAP tubescience hit and
        // documented: `git rev-parse origin/main:<path>` on a path that does not
        // exist there prints the literal argument to STDOUT rather than failing
        // cleanly, so a naive is-empty check does not fire and an untracked file
        // scores as "differs from origin". Test the exit code via cat-file -e.
        let exists = tokio::process::Command::new("git")
            .args(["-C", dir, "cat-file", "-e", &format!("origin/main:{p}")])
            .output()
            .await
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !exists {
            kept.push(p); // genuinely absent from origin: the real WIP case
            continue;
        }
        let local = tokio::process::Command::new("git")
            .args(["-C", dir, "hash-object", "--", &p])
            .output()
            .await;
        let remote = tokio::process::Command::new("git")
            .args(["-C", dir, "rev-parse", &format!("origin/main:{p}")])
            .output()
            .await;
        // VALIDATE THE SHAPE, DO NOT TRUST THE EXIT CODE (creative-dna,
        // AMUX-2947 follow-up). Two reasons, and the second is the dangerous
        // one:
        //
        //   1. NOT because git's exit status is unreliable — it is. That was
        //      briefly believed here and is now settled: `hash-object` on an
        //      unreadable path exits 128, loudly and consistently. The 0 came
        //      from a piped measurement reading `head`'s status rather than
        //      git's (creative-dna measured it, then re-measured with
        //      PIPESTATUS and corrected the record). Recorded because a comment
        //      hinting that a core git command lies is a hazard note someone
        //      would act on, and it would be wrong.
        //
        //   2. The reason that stands, and it is the serious one: comparing two
        //      unvalidated strings has an inverse failure
        //      the conservative `_ => false` arm does NOT cover: if both sides
        //      came back empty, `"" == ""` is TRUE and the path is DROPPED — a
        //      real uncommitted file silently removed from the warning. Keeping
        //      a phantom is noise; dropping a genuine one loses somebody's work
        //      from the only notice that mentions it.
        //
        // A blob id is exactly 40 lowercase hex characters. Anything else is
        // not an answer, whatever the exit code said.
        let blob = |o: &std::process::Output| -> Option<String> {
            let t = String::from_utf8_lossy(&o.stdout).trim().to_string();
            (t.len() == 40 && t.chars().all(|c| c.is_ascii_hexdigit())).then_some(t)
        };
        let same = match (local, remote) {
            (Ok(l), Ok(r)) => match (blob(&l), blob(&r)) {
                (Some(a), Some(b)) => a == b,
                // One side unreadable -> cannot compare -> keep it. A path we
                // failed to check is not a path we have cleared.
                _ => false,
            },
            _ => false,
        };
        if !same {
            kept.push(p);
        }
    }
    (kept, provenance)
}

/// Most files a lane could sensibly be mid-edit on inside ONE untracked
/// directory. Past this the expansion below stops rather than flooding a nudge.
///
/// Truncating loses a warning about the files past the cap, which is the
/// tolerable direction: the reader is not told to act on them at all. Emitting
/// the DIRECTORY instead would hand them a restore target that deletes every
/// file under it, which is the direction that is not recoverable.
const MAX_UNTRACKED_DIR_EXPANSION: usize = 200;

/// `git status --porcelain` under `dir`, as repo-relative paths — FILES ONLY.
///
/// Supplies the LIST only. Whose they are is the guard's answer, never this
/// function's — see the module docs for what re-deriving it cost.
///
/// WHY THE EXPANSION (ts-gke, 2026-09-01). `--untracked-files=normal` collapses
/// a wholly-untracked directory to ONE entry with a trailing slash, and nothing
/// downstream of here has ever looked for that slash. So a directory flowed the
/// whole length of the pipeline dressed as a file: classified as one path,
/// labelled STALE as one path, and rendered with `git checkout origin/main --
/// <path>` as its remedy.
///
/// Measured on the mixpeek checkout, where two such directories were emitted:
///
///   clients/providers/rtsp/__init__.py                       matches origin
///   tests/unit/clients/providers/rtsp/__init__.py            matches origin
///   clients/providers/rtsp/sync_provider.py                  novel, in NO commit
///   tests/unit/clients/providers/rtsp/test_sync_provider.py  novel, in NO commit
///
/// Two of four needed no action and two were novel mid-edit work. The directory
/// verdict was "behind origin", true of the directory and false of half its
/// contents, and the remedy stated at that granularity deletes the other half.
///
/// The proof recipe the nudge itself prescribes cannot rescue the reader,
/// which is what makes this worth fixing here rather than in the wording:
/// `git hash-object` on a directory ERRORS, and an error is DO-NOT-RESTORE by
/// the nudge's own rule. So a careful reader gets stuck and a hurried one
/// restores the directory. Only a per-file list makes the prescribed check
/// runnable at all.
///
/// `normal` is kept for the listing itself. `all` would walk every untracked
/// tree in the repo on every run to answer a question about the few that are
/// dirty; expanding only the directories actually reported pays for what is
/// used. `--exclude-standard` keeps the expansion to exactly what `git status`
/// would itself have shown, so an ignored tree cannot enter through this door.
/// The repo root for `dir`, or `dir` when it is not in a repo.
///
/// AF-438, reported by mvs-pitr. `git status --porcelain` emits paths relative
/// to the REPO ROOT regardless of the cwd it is run from, and every nudge
/// labelled that list "under {dir}" using the lane's own `CC_DIR`. For a lane
/// whose cwd is a subdirectory the two disagree, and the reader concatenates
/// them into a path that does not exist: their notice named
/// `.../mixpeek/server/mvs/ME` for a file sitting at `.../mixpeek/ME`.
///
/// That is worse than cosmetic, because git PATHSPECS are cwd-relative. An
/// operator following the remedies from the directory the notice named runs
/// `git checkout origin/main -- ME` against `server/mvs/ME`, which either does
/// nothing or hits a different file, while every command appears to succeed.
async fn repo_root_of(dir: &str) -> String {
    tokio::process::Command::new("git")
        .args(["-C", dir, "rev-parse", "--show-toplevel"])
        .output()
        .await
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| dir.to_string())
}

async fn dirty_paths(dir: &str) -> Vec<String> {
    let out = tokio::process::Command::new("git")
        .args(["-C", dir, "status", "--porcelain", "--untracked-files=normal"])
        .output()
        .await;
    let Ok(out) = out else { return Vec::new() };
    if !out.status.success() {
        return Vec::new();
    }
    let raw: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| {
            // "XY path" or "XY old -> new"; take the destination.
            let rest = l.get(3..)?.trim();
            Some(rest.rsplit(" -> ").next().unwrap_or(rest).trim_matches('"').to_string())
        })
        .filter(|p| !p.is_empty())
        .collect();

    let mut paths = Vec::with_capacity(raw.len());
    for p in raw {
        if !p.ends_with('/') {
            paths.push(p);
            continue;
        }
        // A directory is NEVER passed through. When it cannot be enumerated it
        // is DROPPED, losing the warning, because the alternative is emitting a
        // restore target whose blast radius is every file beneath it. That is
        // the same asymmetry the nudge already states to its reader: a declined
        // restore is recoverable and a deleted keystroke is not.
        paths.extend(expand_untracked_dir(dir, &p).await);
    }
    paths
}

/// The untracked, non-ignored files under one directory, repo-relative.
///
/// Empty on any failure — see the caller for why that is the safe direction.
async fn expand_untracked_dir(dir: &str, subdir: &str) -> Vec<String> {
    let out = tokio::process::Command::new("git")
        .args(["-C", dir, "ls-files", "--others", "--exclude-standard", "-z", "--", subdir])
        .output()
        .await;
    let Ok(out) = out else { return Vec::new() };
    if !out.status.success() {
        return Vec::new();
    }
    // `-z` because a path may contain a newline, and this list feeds a remedy
    // the reader runs. Splitting on '\n' would hand them half a filename.
    String::from_utf8_lossy(&out.stdout)
        .split('\0')
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .take(MAX_UNTRACKED_DIR_EXPANSION)
        .map(str::to_string)
        .collect()
}

/// The owner named in one guard entry, or [`UNRESOLVED_OWNER`] (AMUX-3816).
///
/// A free function rather than a closure inside the parse so it has a test that
/// does not need an HTTP call to the guard (ethos rule 7). The bug it fixes was
/// exactly one `unwrap_or` away and shipped to every row of a nudge:
/// `unwrap_or` fires only on a MISSING key, so the guard's `owner: ""` for an
/// unresolvable cotenant sailed through into a possessive template.
fn owner_name(entry: &Value) -> String {
    entry
        .get("owner")
        .and_then(Value::as_str)
        .map(str::trim)
        // `?` was the old fallback and is the same defect one step later:
        // `[?'s]` renders as a name, and looks deliberate while being none.
        .filter(|s| !s.is_empty() && *s != "?")
        .unwrap_or(UNRESOLVED_OWNER)
        .to_string()
}

/// Ownership for `paths`, from the staged-guard ITSELF — called as a function,
/// not reimplemented.
///
/// This is the card's requirement made literal: one implementation of "whose is
/// this", with two consumers. Two implementations diverge and each looks correct
/// alone, which is exactly how the guard said MINE=0 while the nudge said 11
/// files were mine, at the same moment about the same files.
async fn ownership_from_guard(session: &str, dir: &str, paths: &[String]) -> Option<Ownership> {
    let body = json!({ "dir": dir, "session": session, "paths": paths });
    let mut headers = axum::http::HeaderMap::new();
    if let Ok(v) = axum::http::HeaderValue::from_str(session) {
        headers.insert("x-amux-session", v);
    }
    // `None` state: this is an ownership PROBE, not a commit attempt, so it
    // must not fire the owner notifications the HTTP path sends (AMUX-2923).
    let (status, axum::Json(v)) = crate::api::git_guard::staged_guard_inner(
        None,
        headers,
        axum::body::Bytes::from(body.to_string()),
    )
    .await;
    if !status.is_success() || v.get("ok").and_then(Value::as_bool) != Some(true) {
        // Attribution unavailable. Return None and stay SILENT rather than
        // nudging without it — python kept the old over-nudging behaviour here
        // and that is the failure mode, not the safe default.
        return None;
    }
    let pairs = |k: &str| -> Vec<(String, String)> {
        v.get(k)
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|f| {
                        Some((
                            f.get("path")?.as_str()?.to_string(),
                            // EMPTY IS UNRESOLVED, NOT A NAME (AMUX-3816). The
                            // guard reports `owner: ""` for a peer it could not
                            // resolve — a running cotenant with no transcript —
                            // and `unwrap_or` fires only on a MISSING key, so
                            // `Some("")` sailed through and the possessive
                            // template rendered a whole list as `['s]`. `"?"`
                            // was the same defect one step later and worse,
                            // because `[?'s]` looks deliberate.
                            owner_name(f),
                        ))
                    })
                    .collect()
            })
            .unwrap_or_default()
    };
    // "I cannot tell" is a first-class answer here, and the reason this fix is
    // possible now: the guard reports when a cotenant has no transcript, so an
    // empty `foreign` does NOT clear their files.
    // `degraded` is an ARRAY of sentences on the live server, not a string —
    // an `as_str()` read silently dropped it, which would have shipped this fix
    // with its own disclosure permanently off. Handle both shapes.
    let partial = v
        .get("degraded")
        .and_then(|d| match d {
            Value::String(s) if !s.is_empty() => Some(s.clone()),
            Value::Array(a) if !a.is_empty() => {
                let joined: Vec<String> =
                    a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect();
                (!joined.is_empty()).then(|| joined.join("; "))
            }
            _ => None,
        })
        .or_else(|| {
            v.get("reason")
                .and_then(Value::as_str)
                .filter(|s| s.to_lowercase().contains("partial") || s.to_lowercase().contains("invisible"))
                .map(str::to_string)
        });
    let plain = |k: &str| -> Vec<String> {
        v.get(k)
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|f| {
                        f.get("path")
                            .and_then(Value::as_str)
                            .or_else(|| f.as_str())
                            .map(str::to_string)
                    })
                    .collect()
            })
            .unwrap_or_default()
    };
    let mut own = Ownership {
        foreign: pairs("foreign"),
        shared: pairs("shared"),
        undecided: plain("undecided"),
        partial,
        unclaimed: plain("unclaimed"),
    };
    // SETTLED-MINE + DIRTY-THEIRS IS NOT A CONTEST (AMUX-3436). `shared` keys
    // on edit records inside the window, which cannot tell edited-and-committed
    // from edited-and-dirty: a session whose every hunk already landed in HEAD
    // was told to stage per-hunk over a file where all dirty bytes were a
    // peer's in-flight work. The discriminator exists — owner_committed_since,
    // the same check the victim notice runs — so ask it: a shared path whose
    // OWN edit is strictly settled by a newer own commit demotes to foreign
    // (NOT YOURS, the peer named). Unsettled, unknown-peer, or unanswerable
    // rows keep today's CONTESTED, the safe direction.
    let mut settled: BTreeSet<String> = BTreeSet::new();
    if let Some(rows) = v.get("shared").and_then(Value::as_array) {
        for row in rows {
            let Some(path) = row.get("path").and_then(Value::as_str) else { continue };
            let peer = row.get("owner").and_then(Value::as_str).unwrap_or("(unknown)");
            let Some(mine_age) = row.get("mine_age_secs").and_then(Value::as_i64) else {
                continue;
            };
            if peer == "(unknown)" || session.is_empty() {
                continue;
            }
            if crate::api::git_guard::owner_committed_since(dir, path, session, mine_age)
                .await
                .is_some()
            {
                settled.insert(path.to_string());
            }
        }
    }
    demote_settled_shared(&mut own, &settled);
    Some(own)
}

/// Pure half of the AMUX-3436 demotion: move settled shared rows to foreign,
/// keeping the peer as the named owner.
fn demote_settled_shared(own: &mut Ownership, settled: &BTreeSet<String>) {
    if settled.is_empty() {
        return;
    }
    let (moved, kept): (Vec<_>, Vec<_>) =
        own.shared.drain(..).partition(|(p, _)| settled.contains(p));
    own.shared = kept;
    own.foreign.extend(moved);
}

/// Per-path freshness for `paths`, computed against origin/main (MG-1467).
///
/// The second axis the guard never had. [`ownership_from_guard`] answers WHOSE a
/// file is; this answers WHICH DIRECTION it differs from origin. Both hazards are
/// real and need OPPOSITE handling (committing a peer's file vs committing a
/// stale copy that reverts origin), so one count cannot serve both.
///
/// Caller-side exactly like ownership, because it needs the repo and [`build`] is
/// deliberately repo-blind. It reads the origin/main ref that
/// [`drop_paths_identical_to_origin`] refreshed earlier in the same tick, so it
/// MUST run after that filter. It does not fetch again.
///
/// How many lines the WORKTREE copy of `p` has that `origin/main`'s copy does
/// not. `None` when either side is unreadable — absence of an answer, never
/// zero, because zero is the permission to prescribe a destructive restore.
///
/// Line-set rather than diff-hunk: the paths this guards are append-only ledgers
/// (`frustrations.md` and friends), where entries are appended and occasionally
/// MOVED to a companion archive. A hunk-based comparison reports a move as a
/// change on both sides; a set comparison asks the only question that matters
/// here, which is whether any content would be DESTROYED by taking origin's copy.
///
/// Blank and whitespace-only lines are dropped so that reflowing cannot
/// manufacture novelty, and the comparison is over a set so that reordering
/// cannot either. Both directions of that choice are conservative: they can only
/// make the difference SMALLER, and a smaller difference is what unlocks the
/// downgrade — so the caller pairs this with `Some(0)` only, never with a
/// threshold.
/// How many line INSTANCES of `want` are not covered by `have`.
///
/// ONE PRODUCER FOR BOTH DIRECTIONS, deliberately. The two callers below ask
/// mirror questions and must not answer them by different rules: a downgrade is
/// only sound if "nothing is at risk" means the same thing in each arm. They
/// were two hand-written loops for two days and had already drifted in the way
/// that matters, because the second was written by copying the first.
///
/// MULTISET, NOT SET (gtm-media-assets, 2026-08-26, reviewing 90eaa6dc). Set
/// membership ignores multiplicity, so dropping ONE of a repeated line scores
/// zero: origin `x = 1 / } / }` against worktree `x = 1 / } / novel` reported
/// nothing missing while a closing brace was being deleted. Counting instances
/// keeps the property the set was chosen for — a line MOVED within the file is
/// not a loss — and drops the one it was not chosen for.
///
/// RAW LINES, NOT TRIMMED, same report. `str::trim` makes indentation
/// invisible, and in Python or YAML an indent change IS a semantic change, so
/// origin re-indenting a block scored zero and the owner was told that
/// committing reverts nothing. Whitespace-only lines are still skipped: a lost
/// blank line is not lost work, and `str::lines` already strips the `\r` of a
/// CRLF ending, so trimming was buying nothing else.
///
/// BOTH CHANGES TIGHTEN, which is why they are safe to make together on an arm
/// that has shipped since 2026-08-24. Each caller downgrades only on a zero, so
/// counting more losses can only mean FEWER downgrades and more of the loud
/// verdict. The failure mode stays "warned too loudly" rather than "prescribed a
/// remedy that ate committed work".
fn missing_line_instances(have: &str, want: &str) -> usize {
    // `trim_end`, not `trim` (AMUX-3786). The two ends of a line are treated
    // differently here, and the justification is a COST COMPARISON, not a fact
    // about languages. The first version of this comment claimed the latter and
    // was wrong; a premise in a codebase outlives the decision it justified, so
    // it is written as the trade it actually is.
    //
    // LEADING whitespace is content wherever indentation carries meaning
    // (Python, YAML), and erasing it was the second half of the bug this
    // function was rewritten for. That one is not a trade: an indent change is a
    // behaviour change, so it stays significant.
    //
    // TRAILING whitespace IS content too. Markdown's hard line break is two
    // trailing spaces, and Markdown is the most common file type this nudge
    // touches. Measured by gtm-media-assets while reviewing this change, and
    // reproduced 2026-08-26. The two commands report different UNITS, so both
    // are given: quoting one and citing the other's number makes a reader who
    // runs it think the comment is stale.
    //
    //     git grep -IP  '\S  +$' -- '*.md'   # LINES:  amux 3,  mixpeek 1986
    //     git grep -IlP '\S  +$' -- '*.md'   # FILES:  amux 1,  mixpeek 294
    //
    // We drop it anyway, because the two failures are not the same size. Losing
    // it costs a line break in rendered output. Treating it as content holds
    // DIVERGED open every time an editor strips trailing whitespace on save,
    // across those 294 files — the noisier failure, and the one this card was
    // filed for. Special-casing two trailing spaces in .md is more machinery
    // than a rendering nit deserves.
    //
    // REVISIT IF this ever runs against files where trailing whitespace is
    // load-bearing rather than cosmetic: a .patch fixture, a golden file, a
    // snapshot test. That is what would change the answer.
    let mut pool: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for l in have.lines().map(str::trim_end).filter(|l| !l.is_empty()) {
        *pool.entry(l).or_default() += 1;
    }
    let mut missing = 0usize;
    for l in want.lines().map(str::trim_end).filter(|l| !l.is_empty()) {
        match pool.get_mut(l) {
            Some(n) if *n > 0 => *n -= 1,
            _ => missing += 1,
        }
    }
    missing
}

async fn read_origin_and_worktree(dir: &str, p: &str) -> Option<(String, String)> {
    let origin = tokio::process::Command::new("git")
        .args(["-C", dir, "show", &format!("origin/main:{p}")])
        .output()
        .await
        .ok()
        .filter(|o| o.status.success())?;
    let origin = String::from_utf8_lossy(&origin.stdout).into_owned();
    let mine = tokio::fs::read_to_string(std::path::Path::new(dir).join(p)).await.ok()?;
    Some((origin, mine))
}

async fn worktree_lines_absent_from_origin(dir: &str, p: &str) -> Option<usize> {
    let (origin, mine) = read_origin_and_worktree(dir, p).await?;
    Some(missing_line_instances(&origin, &mine))
}

/// The MIRROR of [`worktree_lines_absent_from_origin`], and the half the
/// DIVERGED verdict never asked (gtm-media-assets, 2026-08-26).
///
/// DIVERGED tells the owner that both single-arm remedies destroy landed work,
/// so they must hand-merge. That claim has TWO independent halves, and only one
/// of them was ever measured:
///
///   restore risk  the worktree holds lines origin lacks, so
///                 `git checkout origin/main -- <path>` destroys them.
///                 Measured, by the function above.
///   commit  risk  origin holds lines the worktree lacks, so committing the
///                 worktree REVERTS them.  NEVER MEASURED — inferred from sha
///                 ancestry, which is exactly what a graft-push replay breaks.
///
/// A path can fail the first test and pass the second, and on a fleet that
/// graft-pushes that is the COMMON case rather than a corner: graft-push lands
/// the work under a new sha and leaves the local commit object behind, so
/// `origin/main..HEAD` prints commits for a path whose content origin already
/// has. Both arms fire, and the verdict is "novel and stale at once".
///
/// THE LIVE SPECIMEN, server/mvs/shard-rs/src/grpc.rs on the mixpeek checkout:
/// both ancestry arms print commits, and `git diff --numstat origin/main` reads
/// 42 added, ZERO deleted. The worktree is a strict SUPERSET of origin — it
/// already contains origin's newest commit on that path. Nothing to merge,
/// nothing of origin's at risk, and the correct action was the ordinary commit
/// that DIVERGED told them not to make.
///
/// The cost of being wrong in that direction is what makes this worth a second
/// subprocess: a false DIVERGED tells the owner both remedies are destructive
/// and sends them to a manual reconciliation, which is the operation most
/// likely to lose the work the warning was protecting.
///
/// Shares [`missing_line_instances`] with its mirror, with the arguments the
/// other way round. Order ignored, multiplicity counted, indentation
/// significant.
async fn origin_lines_absent_from_worktree(dir: &str, p: &str) -> Option<usize> {
    let (origin, mine) = read_origin_and_worktree(dir, p).await?;
    Some(missing_line_instances(&mine, &origin))
}

/// When it cannot classify a path (git error, no origin/main ref) it treats the
/// path as ordinary work, never STALE, so a failure degrades to today's
/// behaviour rather than inventing a revert warning.
/// Wall-clock the revived discriminator may spend across ONE nudge run.
///
/// MEASURED ON A REPO 6x THIS ONE (mixpeek-frustrations, 2026-08-25): 594 refs,
/// 26,190 commits, 43,837 tracked files. A single `--find-object` walk costs
/// 691ms there against 103ms here, and — the part that actually breaks the
/// design — the O(1) prefilter INVERTS. Of 59 dirty paths sampled, 58 had their
/// blob already in the object database, so 98% paid the full walk instead of the
/// 30ms lookup. Projected 40s for that sample, and their idle nudge that morning
/// listed 772 paths.
///
/// The premise this was built on ("a genuine new edit produces bytes committed
/// nowhere") is not wrong, it is just not load-bearing on a checkout whose dirty
/// set is genuinely MOSTLY old revisions — which is what their classification
/// showed: 7 of 10 revived. The prefilter stays because it costs 107ms against a
/// 691ms walk and it does save everything on repos shaped like this one; it is
/// simply not the thing that bounds the cost.
///
/// A NUDGE MUST NOT COST MINUTES OF GIT SUBPROCESSES. This runs on a shared
/// checkout, so the cost is paid in the same resource the sessions need, which
/// is the shape ethos.md warns about: a detector that makes the thing it watches
/// worse the harder it looks.
fn revived_budget_ms() -> u128 {
    std::env::var("AMUX_NUDGE_REVIVED_BUDGET_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2_000)
}

/// Hard cap on paths put through the discriminator, independent of the clock.
/// Belt and braces: a budget alone still starts one walk per path, and on a
/// 772-path listing the first check of the clock happens after the first walk.
fn revived_max_paths() -> usize {
    std::env::var("AMUX_NUDGE_REVIVED_MAX_PATHS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(40)
}

async fn freshness_from_repo(dir: &str, paths: &[String]) -> Freshness {
    let mut fresh = Freshness::default();
    if paths.is_empty() {
        return fresh;
    }
    let mut pending_revived: Vec<String> = Vec::new();

    // Resolve the repo root and address paths from there. `git status` emits
    // repo-root-relative paths, but a lane's CC_DIR is often a SUBDIRECTORY; run
    // from there, `git hash-object -- <path>` resolves against the CWD and fails
    // (AMUX-2947). Same trap drop_paths_identical_to_origin documents.
    let root = tokio::process::Command::new("git")
        .args(["-C", dir, "rev-parse", "--show-toplevel"])
        .output()
        .await
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| dir.to_string());
    let dir = root.as_str();

    // A blob id is exactly 40 lowercase hex characters. Anything else, an error
    // message or an empty string, is NOT an answer whatever the exit code said.
    // This is the creative-dna trap: `"" == ""` is true, so two empty stdouts
    // would score as SAME and silently drop a real change.
    let blob = |o: &std::process::Output| -> Option<String> {
        let t = String::from_utf8_lossy(&o.stdout).trim().to_string();
        (t.len() == 40 && t.chars().all(|c| c.is_ascii_hexdigit())).then_some(t)
    };

    for p in paths {
        // 1. NEW: absent from origin. TEST THE EXIT CODE, never stdout emptiness.
        //    Plain `git rev-parse` ECHOES an unresolvable revspec to stdout and
        //    still exits nonzero, so an is-empty check reports every path as
        //    present. Three sessions shipped that bug 2026-08-10. `--verify -q`
        //    keeps stdout clean; `.status.success()` is the only signal read.
        let present = tokio::process::Command::new("git")
            .args(["-C", dir, "rev-parse", "--verify", "-q", &format!("origin/main:{p}")])
            .output()
            .await
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !present {
            fresh.new.push(p.clone());
            continue;
        }

        // 2. SAME: worktree byte-identical to origin. Dirty only because local
        //    HEAD is behind; nothing is wrong, so build suppresses it.
        let local = tokio::process::Command::new("git")
            .args(["-C", dir, "hash-object", "--", p])
            .output()
            .await;
        let remote = tokio::process::Command::new("git")
            .args(["-C", dir, "rev-parse", &format!("origin/main:{p}")])
            .output()
            .await;
        let identical = match (local, remote) {
            (Ok(l), Ok(r)) => match (blob(&l), blob(&r)) {
                (Some(a), Some(b)) => a == b,
                // One side unreadable: cannot call it SAME, fall through.
                _ => false,
            },
            _ => false,
        };
        if identical {
            fresh.same.push(p.clone());
            continue;
        }

        // 2b. THE REFS ALREADY AGREE, so neither STALE nor DIVERGED can be true
        //     (mixpeek-frustrations, 2026-08-24 — their discriminator, and it is
        //     better than the one it replaces).
        //
        //     Step 2 asks whether the WORKTREE matches origin, via
        //     `hash-object -- <path>`, which reads the file on disk. When the
        //     file is DELETED locally that command fails, `identical` is false,
        //     and the path falls through to the ancestry arms below. On a
        //     graft-pushed checkout both arms print commits, so a path whose
        //     content is byte-identical at HEAD, at origin/main and at both
        //     graft twins was reported DIVERGED: "novel and stale at once,
        //     NEITHER standard remedy is safe". Their live specimen:
        //     research/extractors/HYPERSPECTRAL-RASTER-EXTRACTOR-GAP.md, four
        //     blobs all 1e435f5c9fba, arms 5.5h apart by author date.
        //
        //     This compares the two REFS instead. It needs no worktree file, so
        //     a deletion cannot defeat it, and identical bytes at both refs mean
        //     there is nothing to merge and nothing at risk WHATEVER the two
        //     ancestries say. It also subsumes the graft-twin case reported
        //     first, without anyone having to reason about twins.
        //
        //     It does not weaken real STALE either, which is the check worth
        //     doing before accepting this. STALE means committing the worktree
        //     would silently REVERT origin — and if HEAD and origin hold the
        //     same bytes, origin's commits on this path did not change its
        //     content relative to HEAD, so there is nothing for a commit to
        //     revert. The honest classification is then the worktree's own
        //     story: an ordinary edit, or a local deletion, which is the
        //     OWNER's call rather than something a prescribed restore should
        //     decide for them.
        let head_blob = tokio::process::Command::new("git")
            .args(["-C", dir, "rev-parse", "--verify", "-q", &format!("HEAD:{p}")])
            .output()
            .await;
        let origin_blob = tokio::process::Command::new("git")
            .args(["-C", dir, "rev-parse", "--verify", "-q", &format!("origin/main:{p}")])
            .output()
            .await;
        let refs_agree = match (head_blob, origin_blob) {
            (Ok(h), Ok(o)) => match (blob(&h), blob(&o)) {
                (Some(a), Some(b)) => a == b,
                // Same rule as step 2: one side unreadable is not agreement.
                // Absence is not evidence, and calling it agreement here would
                // suppress a genuine STALE.
                _ => false,
            },
            _ => false,
        };
        //     ONE POPULATION THIS DOES NOT SPLIT, and it is a real silent
        //     revert (mixpeek-frustrations, reviewing the above). 2b is reached
        //     only when the worktree ALREADY differs from origin, so the state
        //     here is: worktree != origin, HEAD == origin. That holds two cases:
        //
        //       (a) a genuine new edit          -> `edited` is exactly right
        //       (b) an OLD committed revision on disk -> committing REVERTS the
        //           content both refs agree on
        //
        //     (b) is invisible to both ancestry arms PRECISELY BECAUSE the refs
        //     agree, so it is the AMUX-3000 shape reached from a direction the
        //     arms cannot see. It is not an argument against this gate: `edited`
        //     prescribes no destructive remedy, so the failure is UNDER-warning
        //     rather than a bad prescription, which is the right way to fail.
        //
        //     The discriminator, if anyone wants it, is one command and is
        //     already in this file's toolkit — a commit printing here means the
        //     on-disk copy is an old revision:
        //
        //       git log --all --oneline --find-object=$(git hash-object <path>) -- <path>
        //
        //     MEASURED, then shipped (AMUX-3695). The cost objection did not
        //     survive the numbers: on this repo (141 refs, 4158 commits) a full
        //     `--all --find-object` walk that finds NOTHING — the worst case,
        //     and the common one — costs 0.103s with a pathspec.
        //
        //     And it is not even paid usually, because of the prefilter below.
        //     A genuine new edit produces bytes that were never committed
        //     anywhere, so its blob is not in the object database at all, and
        //     `git cat-file -e` answers that with a single hash lookup in
        //     0.030s and no walk. Only a path whose on-disk content IS a known
        //     object pays the walk, and that is exactly the (b) candidate.
        //
        //     So the ordering is the optimisation: ask the cheap question that
        //     can only be answered one way, and reach for the expensive one
        //     solely when the cheap answer is inconclusive.
        if refs_agree {
            // DEFERRED, NOT DECIDED HERE (mixpeek-frustrations, probe 2). This
            // loop runs in `git status` order, which is alphabetical, so
            // checking inline meant the budget always sampled the alphabetically
            // first paths. On their repo that is 11 canvas/apps and 6
            // .github/workflows out of a population dominated by 342 SDK
            // packages, and the two ends disagree wildly: head-of-status reads
            // 35% revived, a random sample of the same set at the same moment
            // reads 75%. Same repo, opposite sides of the share threshold,
            // purely from path ordering.
            //
            // So the candidates are collected here and CHECKED in stratified
            // order below.
            pending_revived.push(p.clone());
            continue;
        }

        // 3. STALE: origin has commits on this path that local HEAD lacks, so
        //    the worktree copy is older and committing it would REVERT origin.
        //    Here stdout emptiness IS the right test: this is `git log` output,
        //    not a revspec echo. Gated on the command succeeding.
        let stale = tokio::process::Command::new("git")
            .args(["-C", dir, "log", "--oneline", "HEAD..origin/main", "--", p])
            .output()
            .await
            .map(|o| o.status.success() && !String::from_utf8_lossy(&o.stdout).trim().is_empty())
            .unwrap_or(false);
        if stale {
            // 3b. DIVERGED: local HEAD ALSO has commits origin lacks on this
            //     path. Filing it STALE prescribes a restore that reverts the
            //     local side's landed work — and the restore-safety check
            //     passes while it happens, because locally-committed content
            //     is reachable-from-a-commit too (the mixpeek .githooks
            //     disarm, 2026-08-20). The missing cell, not a tighter test.
            let local_ahead = tokio::process::Command::new("git")
                .args(["-C", dir, "log", "--oneline", "origin/main..HEAD", "--", p])
                .output()
                .await
                .map(|o| {
                    o.status.success() && !String::from_utf8_lossy(&o.stdout).trim().is_empty()
                })
                .unwrap_or(false);
            // 3c. …but `git log origin/main..HEAD` counts commits BY SHA, and a
            //     commit already upstream under a DIFFERENT sha (cherry-pick,
            //     rebase, graft-push replay) sits in that range permanently —
            //     the duplicate-sha case the Deploy section of CLAUDE.md
            //     documents for `origin/main..main`. On a graft-push checkout
            //     EVERY path reads local_ahead, so DIVERGED fired for paths that
            //     were merely STALE and the safe restore was withheld
            //     (reported by mixpeek-frustrations, 2026-08-24).
            //
            //     The discriminator has to be CONTENT, because sha identity is
            //     exactly what a replay destroys. The remedy under discussion
            //     (`git checkout origin/main -- <path>`) overwrites the
            //     WORKTREE, so restore-safety is precisely "does the worktree
            //     hold lines origin does not". Zero means there is nothing here
            //     to lose whatever the sha arithmetic said.
            //
            //     ONE-SIDED ON PURPOSE. It can only ever DOWNGRADE diverged to
            //     stale, and only on a positive finding (a readable pair, and an
            //     empty difference). Any error, unreadable side or non-empty
            //     difference leaves the DIVERGED verdict standing, so the
            //     failure mode stays "warned too loudly" rather than "prescribed
            //     a restore that ate committed work" — which is the incident
            //     this whole cell exists for.
            if local_ahead {
                match worktree_lines_absent_from_origin(dir, p).await {
                    Some(0) => {
                        // Downgrade, and SAY SO. A verdict that silently changes
                        // class is the one nobody can audit later: STALE-because-
                        // downgraded and STALE-outright would otherwise be
                        // byte-identical in the log, and this is the arm that
                        // prescribes a destructive remedy.
                        tracing::info!(
                            target: "autofix",
                            path = %p,
                            dir = %dir,
                            "commit-nudge: DIVERGED downgraded to STALE — local commits exist \
                             on this path but contribute no lines origin lacks (replayed or \
                             cherry-picked upstream), so the restore is safe"
                        );
                        fresh.stale.push(p.clone());
                    }
                    other => {
                        // 3d. THE OTHER HALF OF THE CLAIM, which nothing here
                        //     had ever measured (gtm-media-assets, 2026-08-26).
                        //     Reaching this arm proves only that the worktree
                        //     holds lines origin lacks — the RESTORE is unsafe.
                        //     DIVERGED additionally asserts that COMMITTING is
                        //     unsafe, and that half was inferred from sha
                        //     ancestry, which a graft-push replay breaks for
                        //     every path on the checkout.
                        //
                        //     If origin holds no line the worktree lacks, the
                        //     worktree is a strict SUPERSET of origin: the
                        //     commit reverts nothing, so the honest class is an
                        //     ordinary edit and the honest advice is to commit.
                        //     Not STALE — STALE prescribes the restore that
                        //     would eat the local lines counted just above.
                        //
                        //     SAME ONE-SIDED SHAPE as 3c: it can only downgrade,
                        //     only on a readable positive zero, and any error or
                        //     unreadable side leaves DIVERGED standing.
                        match origin_lines_absent_from_worktree(dir, p).await {
                            Some(0) => {
                                tracing::info!(
                                    target: "autofix",
                                    path = %p,
                                    dir = %dir,
                                    novel_lines = ?other,
                                    // LOG THE QUANTITY THIS BRANCH DECIDED ON.
                                    // It is 0 by construction here, which is
                                    // exactly why it was omitted and exactly why
                                    // it has to be present: the STANDS arm logs
                                    // origin_lines_at_risk and this one did not,
                                    // so the field's ABSENCE on a downgrade was a
                                    // property of the logging rather than of the
                                    // predicate. mixpeek-general asked whether the
                                    // downgrade ever fires at zero, reached for
                                    // this log, and found a question their
                                    // instrument could not answer either way
                                    // (2026-09-04). A decision whose own input is
                                    // missing from its record cannot be audited
                                    // from the record (ethos rule 4).
                                    origin_lines_at_risk = 0,
                                    "commit-nudge: DIVERGED downgraded to EDITED — the worktree \
                                     is a strict superset of origin, so committing reverts \
                                     nothing and there is nothing to hand-merge"
                                );
                                fresh.edited.push(p.clone());
                            }
                            lost => {
                                tracing::info!(
                                    target: "autofix",
                                    path = %p,
                                    dir = %dir,
                                    novel_lines = ?other,
                                    origin_lines_at_risk = ?lost,
                                    "commit-nudge: DIVERGED stands — content differs in both \
                                     directions"
                                );
                                fresh.diverged.push(p.clone());
                            }
                        }
                    }
                }
            } else {
                fresh.stale.push(p.clone());
            }
            continue;
        }

        // 4. EDITED: differs, origin has not moved past HEAD on this path. A
        //    plausible in-flight edit; commit-worthy like today.
        fresh.edited.push(p.clone());
    }
    drain_revived(dir, &pending_revived, &mut fresh).await;
    mark_generated(dir, &mut fresh).await;

    fresh
}

/// Ask the REPO which of its diverged paths are generator output (AF-428).
///
/// `git check-attr` is the reader because the declaration is a git attribute;
/// there is nothing to parse and no second place for the answer to live. `-z`
/// rather than the default line format: the default is `<path>: <attr>: <value>`
/// and a path containing a colon splits wrong, silently, in the direction of
/// under-reporting.
///
/// FAILS TO "NOT MEASURED", NEVER TO "NONE ARE GENERATED". A git error leaves
/// `generated` empty AND sets `generated_checked` to 0 with `stopped_by` =
/// "error", so the renderer cannot print a coverage line that reads like a
/// finding. Every path stays in `diverged` and gets today's merge recipe, which
/// is the conservative direction: the reader is told to think, not told the
/// wrong thing.
async fn mark_generated(dir: &str, fresh: &mut Freshness) {
    if fresh.diverged.is_empty() {
        return;
    }
    let out = tokio::process::Command::new("git")
        .args(["-C", dir, "check-attr", "-z", "linguist-generated", "--"])
        .args(fresh.diverged.iter())
        .output()
        .await;
    let out = match out {
        Ok(o) if o.status.success() => o,
        _ => {
            fresh.generated_checked = 0;
            fresh.generated_stopped_by = "error";
            return;
        }
    };
    fresh.generated_checked = fresh.diverged.len();
    fresh.generated_stopped_by = "";
    let text = String::from_utf8_lossy(&out.stdout);
    // -z emits a flat NUL-separated stream of (path, attr, value) triples, not
    // one record per line. A trailing NUL yields an empty final element, so the
    // chunking has to tolerate a short tail rather than assume len % 3 == 0.
    let fields: Vec<&str> = text.split('\0').collect();
    for tri in fields.chunks(3) {
        if tri.len() < 3 {
            continue;
        }
        // BOTH SPELLINGS COUNT, and my first version took only "set" (AF-428).
        // git reports a BARE `linguist-generated` as "set" and an explicit
        // `linguist-generated=true` as "true" — and =true is the form GitHub
        // documents, so the only spelling anyone is likely to write was the one
        // that did not match. Caught by the cell that runs the real git rather
        // than by reading the code; nothing about `set` looks wrong on its own.
        // "unset", "false" and "unspecified" all correctly fall through.
        if tri[2] == "set" || tri[2] == "true" {
            fresh.generated.push(tri[0].to_string());
        }
    }
}

/// One sweep: nudge idle lanes that have uncommitted work OF THEIR OWN.
///
/// Delivery is `steer_enqueue`, never a direct send — the existing loop applies
/// the turn-boundary gate, so a nudge cannot land mid-turn (the AMUX-2642 rule
/// this repo already paid for once).
pub async fn nudge_tick(state: &AppState, lanes: &[(String, String)], now: f64) -> usize {
    let mut sent = 0usize;
    for (session, dir) in lanes {
        if session.is_empty() || dir.is_empty() {
            continue;
        }
        // AN ISOLATED WORKER IS A RAW AGENT — DO NOT STEER THE HARNESS INTO IT
        // (Ethan, 2026-08-26: "we have an isolated worker but it still has amux
        // shit", naming gtm-research, which had CC_ISOLATED=1 and was receiving
        // this nudge).
        //
        // `session_is_isolated`'s own doc calls itself "the single source of
        // truth every isolation decision consults" and lists seven consumers:
        // spawn-env suppression, --mcp-config, board auto-capture, the peer
        // fleet list, the fleet roster, the peer-send guard, and the
        // status/rate-limit sweep. Every one of those is about what the worker
        // is TOLD ABOUT or DISCOVERABLE BY. None of them cover what gets typed
        // INTO its pane, and measured on 2026-08-26 ZERO of the 15
        // runtime_jobs consulted it at all — while three of them steer.
        //
        // So the designation promised "the amux harness stripped" and delivered
        // env suppression: the worker still got commit nudges. That is ethos
        // rule 1's exemption question — when you exempt something from a loop,
        // name what still reaches it — answered the wrong way for two months.
        //
        // The owner's own peek/send still work; that is the documented boundary
        // and it is untouched here. What stops is amux typing at a lane whose
        // whole point is to run untouched.
        if crate::api::session_verbs::session_is_isolated(session) {
            continue;
        }
        // Filtered against ORIGIN, not local HEAD — see
        // drop_paths_identical_to_origin for why `git status` alone answers the
        // wrong question on a graft-push checkout.
        let (dirty, provenance) =
            drop_paths_identical_to_origin(dir, dirty_paths(dir).await).await;
        if dirty.is_empty() {
            continue;
        }
        let Some(own) = ownership_from_guard(session, dir, &dirty).await else {
            continue;
        };
        // The freshness axis (MG-1467): WHICH DIRECTION each path differs from
        // origin. Computed here, caller-side, exactly as ownership is; build
        // stays repo-blind. Runs after drop_paths_identical_to_origin so it
        // reads the origin/main ref that filter just refreshed.
        let fresh = freshness_from_repo(dir, &dirty).await;
        // Provenance rides on EVERY nudge, not only the degraded ones. It is the
        // healthy line that makes the stale line legible: with a stamp present
        // in both states, "compared against a STALE origin/main" reads as a
        // difference. Printed only when degraded, it reads as ordinary prose and
        // gets skimmed exactly like the phantom paths it is warning about.
        // The ROOT for the label, not `dir` (AF-438). `dir` still keys the guard
        // and every git call above, because `guard_verdicts` is keyed per-`dir`
        // and re-keying it would orphan every checkout's history and the
        // invariant that reads it. Only the human-facing path label moves.
        let label = repo_root_of(dir).await;
        let Some(msg) = build(&label, &dirty, &own, &fresh, &provenance) else { continue };

        // Cap AFTER deciding there is something to say, so a suppressed-by-cap
        // day does not also consume the "nothing to say" path.
        let key = cap_key(session, now);
        let already = state
            .store
            .read()
            .ok()
            .and_then(|c| {
                c.query_row("SELECT 1 FROM prefs WHERE key=?1", rusqlite::params![key], |_| Ok(()))
                    .ok()
            })
            .is_some();
        if already {
            continue;
        }
        let k2 = key.clone();
        let _ = state
            .store
            .write_async(move |conn| {
                conn.execute(
                    "INSERT INTO prefs (key, value) VALUES (?1, '1') \
                     ON CONFLICT(key) DO NOTHING",
                    rusqlite::params![k2],
                )?;
                Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
            })
            .await;
        // SELF-CHECK ON THE TEXT WE ARE ABOUT TO SEND (AMUX-3718, second fix).
        //
        // The bug this catches shipped for weeks with a green unit test, because
        // the test pinned a layer the broken path did not flow through. A CI
        // cell that asserts the right property in the wrong place is
        // indistinguishable from one in the right place, so the durable
        // instrument is a check on the ACTUAL delivered bytes: if an append-only
        // shared file is in the set, the message MUST carry the archive check,
        // whichever arm rendered it. Without it the nudge prescribes only the
        // half that loses data.
        //
        // WARN, not a suppression: a nudge naming a real divergence is still
        // worth more than silence, and swallowing it would trade a loud bug for
        // a quiet one. This is the line a log sweep finds without anyone
        // knowing to look for it.
        if let Some(bad) = missing_archive_check(&dirty, &fresh, &msg) {
            tracing::warn!(
                session = %session,
                paths = %bad,
                "commit-nudge: append-only shared file prescribed a remedy WITHOUT the archive \
                 check — the union-merge directive lost its safety half and this nudge can \
                 resurrect archived entries (AMUX-3718 regression)"
            );
        }
        let _ = crate::api::session_verbs::steer_enqueue(state, session, &msg, "commit-nudge", "").await;
        sent += 1;
    }
    sent
}

/// Spawn the sweep. LEVEL-triggered like board_drive's pickup, for the reason
/// stated there: this process re-execs on every deploy, so an edge-triggered
/// "went idle" is lost and an already-idle lane waits for a transition that
/// never comes.
pub fn spawn(state: AppState) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let every = std::env::var("AMUX_COMMIT_NUDGE_SECS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(600);
        // 0 disables. The knob is here because a nudge loop is exactly the kind
        // of automation that should be switchable off by config rather than by
        // a code change (D4's lesson about policy living in constants).
        if every == 0 {
            tracing::info!("commit-nudge: disabled (AMUX_COMMIT_NUDGE_SECS=0)");
            return;
        }
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(every)).await;
            // One call so the tick and the cadence it was paced at cannot be
            // recorded separately — `every` is resolved in here, so this is
            // the only place that knows it. Surfaces on /api/system-jobs.
            super::registry::tick_every(
                super::registry::ids::COMMIT_NUDGE,
                std::time::Duration::from_secs(every),
            );
            let lanes = idle_lanes_with_dirs(&state);
            if lanes.is_empty() {
                continue;
            }
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs_f64())
                .unwrap_or(0.0);
            let n = nudge_tick(&state, &lanes, now).await;
            if n > 0 {
                tracing::info!(nudged = n, lanes = lanes.len(), "commit-nudge swept");
            }
        }
    })
}

/// Lanes that are IDLE and have a working directory.
///
/// Idle comes from the session's own report (`prefs.session_reports`, the D1
/// exit) rather than from a pane scrape — nudging a lane mid-turn is the thing
/// the steering boundary exists to prevent, and asking the harness is cheaper
/// and more truthful than inferring.
fn idle_lanes_with_dirs(state: &AppState) -> Vec<(String, String)> {
    let reports: Value = state
        .store
        .read()
        .ok()
        .and_then(|c| {
            c.query_row("SELECT value FROM prefs WHERE key='session_reports'", [], |r| {
                r.get::<_, String>(0)
            })
            .ok()
        })
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| json!({}));
    let home = crate::api::groups::amux_home();
    let Ok(rd) = std::fs::read_dir(home.join("sessions")) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for e in rd.flatten() {
        let p = e.path();
        if p.extension().and_then(|x| x.to_str()) != Some("env") {
            continue;
        }
        let Some(name) = p.file_stem().and_then(|s| s.to_str()) else { continue };
        if reports.get(name).and_then(|r| r["state"].as_str()) != Some("idle") {
            continue;
        }
        let env = crate::config::parse_env_file(&p);
        if env.get("CC_ARCHIVED").map(|v| v == "1").unwrap_or(false) {
            continue;
        }
        if let Some(dir) = env.get("CC_DIR").filter(|d| !d.is_empty()) {
            out.push((name.to_string(), dir.clone()));
        }
    }
    out
}

/// The revived pass, as its OWN function, so the budget structurally measures
/// only the walks it governs (AMUX-3760).
///
/// It used to be inline in `freshness_from_repo`, with `started` set at the top
/// of that function — before the entire per-path classification phase. The
/// revived budget was therefore charged for git calls it does not bound, and
/// under I/O contention (measured: a concurrent release build) the 2000ms was
/// already spent when the loop began. It broke on the FIRST iteration, checked
/// nothing, and every candidate fell through to `edited`.
///
/// EXTRACTED RATHER THAN JUST MOVING THE LINE, and that is the point. Moving it
/// fixes today; a function boundary fixes tomorrow, because `started` cannot be
/// hoisted above work it should not measure without being hoisted out of the
/// function entirely. It is verified by SCOPE, which matters here specifically
/// because it is NOT verified by a test: the cell that would catch a
/// mispositioned clock has to pin the budget high to stop the two bounds
/// racing, which is what made this test flaky in the first place. A test that
/// discriminates the clock's position would have to be load-dependent, i.e. the
/// exact property being removed. Said plainly rather than papered over.
async fn drain_revived(dir: &str, pending_revived: &[String], fresh: &mut Freshness) {
    if pending_revived.is_empty() {
        return;
    }

    // ---- the revived pass, in STRATIFIED order --------------------------
    //
    // Round-robin across top-level directories rather than taking the first N
    // (mixpeek-frustrations, probe 2). `git status` is alphabetical, so first-N
    // samples one end of the repo: on their checkout the first 20 dirty paths
    // are 11 canvas/apps and 6 .github/workflows, while the population is 342
    // SDK packages full of regen churn. Head-of-status read 35% revived and a
    // random sample of the SAME set at the SAME moment read 75% — opposite
    // sides of the share threshold, decided by path ordering alone.
    //
    // Round-robin rather than a shuffle: deterministic, so two runs on an
    // unchanged tree agree, and a nudge that reported a different verdict each
    // firing would be worse than a biased one.
    let mut by_dir: std::collections::BTreeMap<&str, Vec<&String>> = Default::default();
    for p in pending_revived {
        by_dir.entry(p.split('/').next().unwrap_or("")).or_default().push(p);
    }
    // PROPORTIONAL, NOT ONE-PER-GROUP (mixpeek-frustrations, third probe).
    //
    // The first version took one path per directory per round, which fixes the
    // alphabetical bias and introduces a worse one when the groups are unequal:
    // it weights GROUPS equally. On their checkout that is 524 files in 41
    // groups, 17 of them singleton root-level .md files. So the singletons take
    // 41% of the round-robin weight while being 3% of the population, and the
    // three directories holding 63% of the files take 7% of it. The
    // deterministic first four came out as three root .md files and one
    // workflow, and ZERO of the 332 SDK files — which are the population the
    // whole check is about there.
    //
    // Each item gets position (i + offset_g) / n_g within its own group, so a
    // group of 132 lays 132 marks evenly across [0,1) and sorting by the key
    // makes any PREFIX proportional, not merely the whole list.
    //
    // THE PREFIX IS THE ONLY PART THAT EVER RUNS, which is what makes the
    // offset load-bearing rather than a flourish. The first draft used the
    // midpoint, (2i+1)/2n, and that is proportional in aggregate and badly
    // skewed in the prefix: every singleton group lands on exactly 0.5, so with
    // one group of 20 and eight singletons the first TEN picks were all from
    // the big group and not one singleton appeared. A budget that stops at four
    // would have seen a single directory. A test caught it.
    //
    // `offset_g` is a stable hash of the group name folded into [0,1), which
    // scatters the singletons across the interval instead of stacking them at
    // the midpoint. Deterministic — FNV-1a written out rather than DefaultHasher,
    // whose output Rust does not promise to keep stable across releases — so two
    // runs over an unchanged tree still agree, which is the property that ruled
    // out a shuffle in the first place.
    let offset = |name: &str| -> f64 {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for b in name.as_bytes() {
            h ^= *b as u64;
            h = h.wrapping_mul(0x1000_0000_01b3);
        }
        (h >> 11) as f64 / (1u64 << 53) as f64
    };
    let mut keyed: Vec<(f64, &String)> = Vec::with_capacity(pending_revived.len());
    for (name, v) in &by_dir {
        let (n, off) = (v.len() as f64, offset(name));
        for (i, p) in v.iter().enumerate() {
            keyed.push(((i as f64 + off) / n, p));
        }
    }
    // Tie-break on the path so equal keys cannot reorder between runs.
    keyed.sort_by(|a, b| a.0.total_cmp(&b.0).then_with(|| a.1.cmp(b.1)));
    let order: Vec<&String> = keyed.into_iter().map(|(_, p)| p).collect();
    let (budget, cap) = (revived_budget_ms(), revived_max_paths());
    // THE CLOCK STARTS HERE, NOT AT THE TOP OF THE FUNCTION (AMUX-3760).
    //
    // `started` had exactly one reader — the check below — and was set before
    // the whole per-path classification phase, so the revived budget was being
    // charged for git calls it does not govern. Under I/O contention (measured:
    // a concurrent release build) the 2000ms was already spent by the time this
    // loop began, it broke on the FIRST iteration, and every candidate fell
    // through to `edited` with `revived_checked: 0`.
    //
    // That is a live behaviour bug, not just a flaky test: the busier the
    // checkout, the more likely the discriminator silently does nothing — and
    // busy is exactly when a shared checkout has revived paths worth catching.
    // A detector that stops working under the load that produces its subject
    // matter is the shape ethos.md warns about.
    //
    // THE TRADE, stated rather than hidden: the function's total wall-clock can
    // now exceed what it was before by up to one budget, because the
    // classification phase no longer eats into this one. Nothing is less
    // bounded than it was — the earlier phase was never bounded by this clock,
    // it was only stealing from it.
    let started = std::time::Instant::now();
    let mut checked: std::collections::BTreeSet<&str> = Default::default();
    // WHICH BOUND STOPPED US. "0 of 8 examined" is honest about coverage and
    // silent about cause, and the two causes want different actions: a cap hit
    // means raise AMUX_NUDGE_REVIVED_MAX_PATHS, a clock hit means the machine
    // is loaded or the walks are slow. Same distinction the counts themselves
    // draw between "we did not look" and "we looked and found little".
    let mut stopped_by = "";
    for p in order {
        // The clock is checked BETWEEN paths, so a run can overshoot by up to
        // one walk. Measured at 2374ms against a 2000ms budget on a repo whose
        // walks cost ~458ms, which is exactly one walk of overshoot. Stated
        // rather than tuned away: bounding it properly needs a timeout on the
        // git child, and the overshoot is one walk either way.
        if checked.len() >= cap {
            stopped_by = "cap";
            break;
        }
        if started.elapsed().as_millis() >= budget {
            stopped_by = "clock";
            break;
        }
        checked.insert(p.as_str());
        if revives_an_old_revision(dir, p).await {
            fresh.revived.push(p.clone());
        } else {
            fresh.edited.push(p.clone());
        }
    }
    for p in pending_revived {
        if !checked.contains(p.as_str()) {
            // UNCHECKED -> `edited`, the safe fallback, and COUNTED. The
            // fallback output is byte-identical to a path that WAS checked and
            // found novel, so without the count "we did not look" reads as "we
            // looked and it is fine".
            fresh.revived_unchecked += 1;
            fresh.edited.push(p.clone());
        }
    }
    fresh.revived_checked = checked.len();
    fresh.revived_stopped_by = stopped_by;
}

#[cfg(test)]
mod tests {
    /// A nudge may not exist without stating what it was measured against —
    /// asserted on EVERY branch, because the signature alone only proves the
    /// caller supplied a scope, not that the reader ever sees it. Both exits
    /// from `build` are exercised: the positively-mine path and the
    /// ownership-unknown path.
    #[test]
    fn every_nudge_states_what_it_compared_against() {
        let dirty = vec!["a.rs".to_string()];

        // `mine` is DERIVED — not foreign, not unknown — so a default
        // Ownership over a dirty path is exactly the positively-mine branch.
        let m = build("/repo", &dirty, &Ownership::default(), &Freshness::default(), "SCOPE-MARKER")
            .expect("mine branch");
        assert!(m.contains("SCOPE-MARKER"), "mine branch dropped its scope: {m}");

        let unknown = Ownership { unclaimed: vec!["a.rs".to_string()], ..Default::default() };
        let u = build("/repo", &dirty, &unknown, &Freshness::default(), "SCOPE-MARKER")
            .expect("unknown branch");
        assert!(u.contains("SCOPE-MARKER"), "unknown branch dropped its scope: {u}");
    }

    /// The advice a nudge PRINTS must use the same predicate the guard
    /// CLASSIFIES with, or the reader's own check contradicts the verdict.
    ///
    /// This shipped broken: the direction-unknown branches told the reader to
    /// run `git cat-file -e $(git hash-object <path>)` and treat "object
    /// exists" as STALE, while `freshness_from_repo` classifies with
    /// `git log HEAD..origin/main -- <path>`. Blob existence cannot separate an
    /// OLD revision from a CURRENT one that is merely unpushed, since both were
    /// committed at some point. On this checkout, which sits ~44 commits ahead
    /// of origin, EVERY committed file answered "exists", so the printed recipe
    /// reported the whole tree STALE and its remedy was a revert. Measured
    /// 2026-08-16: five committed-but-unpushed paths, five false STALEs, one of
    /// them a feature shipped that day.
    ///
    /// Asserted on the message TEXT because the text is the thing that was
    /// wrong; the classifier was right the whole time.
    #[test]
    fn printed_direction_test_matches_the_classifier() {
        let dirty = vec!["a.rs".to_string()];
        let unknown = Ownership { unclaimed: vec!["a.rs".to_string()], ..Default::default() };
        let msgs = [
            build("/repo", &dirty, &Ownership::default(), &Freshness::default(), "S")
                .expect("mine branch"),
            build("/repo", &dirty, &unknown, &Freshness::default(), "S")
                .expect("unknown branch"),
        ];
        for m in &msgs {
            assert!(
                m.contains("HEAD..origin/main"),
                "nudge must prescribe the ancestry test it classifies with: {m}"
            );
            // AF-471: and it must prescribe the BLOB comparison FIRST, because
            // that is what the classifier does first — step 2b computes
            // `refs_agree` from HEAD:<path> vs origin/main:<path> and never
            // reaches the commit test when they agree.
            //
            // The text used to lead with the commit test, which on a
            // graft-pushed checkout is wrong 40.6% of the time (1815 flagged,
            // 1080 actually differing, on ~/Dev/mixpeek 2026-09-04). The
            // classifier was right the whole time and the printed recipe was
            // not — the SECOND time this exact desync has been fixed in this
            // function, which is why the assertion is ordered and not just
            // presence-based.
            let blob_at = m.find("rev-parse HEAD:<path>");
            let graph_at = m.find("HEAD..origin/main");
            assert!(
                blob_at.is_some(),
                "nudge must prescribe the blob comparison the classifier gates on: {m}"
            );
            assert!(
                blob_at < graph_at,
                "the blob comparison must come BEFORE the commit test, as it does in \
                 the classifier — leading with the graph is the AF-471 defect: {m}"
            );
            // The old recipe may still appear as an explicit warning NOT to use
            // it, but never as the prescribed check.
            let prescribes_blob = m.contains("`git cat-file -e $(git hash-object <path>) 2>/dev/null`: object EXISTS")
                || m.contains("prove the direction with the OBJECT-EXISTENCE test");
            assert!(
                !prescribes_blob,
                "nudge prescribes blob-existence as the DIRECTION test; it reports STALE for \
                 every committed-but-unpushed file and its remedy reverts them: {m}"
            );
        }
    }

    /// The DIVERGED cell must render as its own section that forbids BOTH
    /// single-arm remedies (the mixpeek MG-1483 disarm, 2026-08-20): a path in
    /// `diverged` must never be listed as commit-worthy or STALE, and the text
    /// must name the merge as the only exit.
    #[test]
    fn diverged_paths_get_their_own_section_and_leave_both_recipes() {
        let dirty = vec!["hooks/pre-push".to_string()];
        let fresh =
            Freshness { diverged: vec!["hooks/pre-push".to_string()], ..Default::default() };
        let m = build("/repo", &dirty, &Ownership::default(), &fresh, "S")
            .expect("diverged section must render");
        assert!(m.contains("DIVERGED:"), "the cell must have its own section: {m}");
        assert!(
            m.contains("reachable-from-a-commit too"),
            "must say WHY the restore-safety check cannot catch this: {m}"
        );
        // AMUX-3718. This used to assert the phrase "MERGE the two versions", and a
        // directive is not a procedure: mixpeek-frustrations was told to merge 16
        // paths and merged none, because every other arm of this nudge carries
        // something runnable and this one carried a sentence.
        //
        // The phrase assertion could not even see the change that fixed it. The
        // replacement text QUOTES the old directive while describing it as former
        // behaviour, so `contains("MERGE the two versions")` stayed green across a
        // rewrite that removed the prescription entirely. A check that a quotation
        // satisfies is pinning the words, not the property (ethos rule 7).
        assert!(
            m.contains("git merge-file -p"),
            "the merge must be RUNNABLE, not just prescribed: {m}"
        );
        assert!(
            m.contains("merge-base HEAD origin/main"),
            "a three-way merge needs its base named or the recipe cannot be run: {m}"
        );
        assert!(
            m.contains("conflicts=$?"),
            "the reader must learn the conflict COUNT — that is what decides whether \
             this is theirs to resolve or the owner's: {m}"
        );
        assert!(
            !m.contains("STALE:"),
            "a diverged path must not also render the STALE restore recipe: {m}"
        );
    }

    /// THE SAME BARE DIRECTIVE EXISTED TWICE, and fixing one arm would have left
    /// the other (AMUX-3718, mixpeek-frustrations 2026-08-31).
    ///
    /// `commit_worthy_body`'s decision protocol walks the reader through both
    /// direction tests and then, on the DIVERGED branch, used to stop at "MERGE
    /// the two versions (or hand the path to its owner)" — a verdict with no
    /// procedure, in a block whose every other bullet ends in a runnable command.
    /// The DIVERGED section and this protocol are rendered by different functions,
    /// so the section's cell could not see this one.
    /// The DIVERGED merge recipe is right for hand-written files and WRONG for
    /// generator output, and conflicts=0 is exactly when it bites.
    ///
    /// Reported by mixpeek-frustrations 2026-09-02: the nudge listed
    /// server/openapi.json and docs/api-reference/openapi.json as DIVERGED with
    /// "no edit record of yours" — true, and true for every lane, because a
    /// pre-commit hook regenerates them as a side effect and that write carries
    /// no record by construction. Merging two generator outputs yields a spec
    /// matching neither source model: valid, produced by no code, and
    /// overwritten by the next regen. Two lanes hit it in one night and both
    /// declined the recipe on their own judgement; a lane that followed it
    /// lands a spec no generator produced, with nothing downstream to report it.
    #[test]
    fn the_diverged_merge_recipe_carves_out_generated_files() {
        let dirty = vec!["server/openapi.json".to_string()];
        let fresh =
            Freshness { diverged: vec!["server/openapi.json".to_string()], ..Default::default() };
        let m = build("/repo", &dirty, &Ownership::default(), &fresh, "S")
            .expect("diverged section must render");
        assert!(m.contains("DIVERGED:"), "premise: the diverged arm must be firing: {m}");
        assert!(
            m.contains("merge-file -p"),
            "premise: the merge recipe must still be there — this is a carve-out, not a removal: {m}"
        );
        assert!(
            m.contains("NOT FOR GENERATED FILES"),
            "the recipe must name the class it is wrong for: {m}"
        );
        assert!(
            m.contains("REGENERATE FROM SOURCE"),
            "and name the correct remedy, not just the wrong ones: {m}"
        );
        // The reader has to be able to TELL, or the caveat is a worry rather
        // than an instruction. WHAT "TELL" MEANS CHANGED IN AF-428 and this
        // assertion moved with it: the arm used to hand over a signature to
        // check by eye ("no edit record from ANY lane"), which was the best it
        // could do and also described a lane whose hooks are not installed. It
        // now reads the repo's own declaration, so the thing to name is the
        // declaration and how to add one.
        assert!(
            m.contains("linguist-generated") && m.contains(".gitattributes"),
            "name the declaration that moves a path out of this list: {m}"
        );
        // AND the restore path must be named as equally wrong. Naming only the
        // merge would push the reader onto `git checkout origin/main --`, which
        // regenerates the divergence on the next commit.
        assert!(
            m.contains("equally wrong"),
            "restoring from origin is wrong for this class too, and must say so: {m}"
        );
    }

    /// A DECLARED generated path must not be handed the merge recipe at all
    /// (AF-428). The caveat shipped in 05e280fd sits UNDER the recipe, and a
    /// caveat under a command is read after the command.
    #[test]
    fn a_declared_generated_path_gets_the_regenerate_remedy_not_the_recipe() {
        let dirty = vec!["server/openapi.json".to_string()];
        let fresh = Freshness {
            diverged: vec!["server/openapi.json".to_string()],
            generated: vec!["server/openapi.json".to_string()],
            generated_checked: 1,
            ..Default::default()
        };
        let m = build("/repo", &dirty, &Ownership::default(), &fresh, "S")
            .expect("a diverged generated path must still produce a nudge");
        assert!(
            m.contains("DIVERGED (GENERATED):"),
            "a declared generated path needs its own arm: {m}"
        );
        assert!(
            m.contains("REGENERATE FROM SOURCE"),
            "and the remedy that is actually correct for it: {m}"
        );
        // THE POINT OF THE WHOLE CARD. With every diverged path declared, the
        // recipe must not appear anywhere in the message — not lower down, not
        // as a caveat. If this passes only because the string moved, the next
        // assertion catches it.
        assert!(
            !m.contains("merge-file -p"),
            "the merge recipe must not be offered for a declared generated path: {m}"
        );
        assert!(!m.contains("DIVERGED:"), "the hand-written arm must not render at all: {m}");
        // THE ASYMMETRY, named where it is actionable (mixpeek-frustrations,
        // 2026-09-04, from turning this on for real). Over-marking is the worse
        // error: an unmarked generated file gets a recipe the reader can
        // decline, and a marked hand-written file loses its merge warning with
        // nothing said. Their specimen is the one my own candidate list would
        // have got wrong — `packages/python-sdk/setup.py` IS generated and
        // `packages/python-sdk/generate.sh` is not, and both change on every
        // regen, so the firing-frequency signal cannot separate them.
        assert!(
            m.contains("loses its merge warning silently"),
            "name the direction of error that is silent, not just that errors happen: {m}"
        );
        // RANKED, AND NOT MANIFEST-FIRST (mixpeek-frustrations, 2026-09-04).
        // My first wording pointed at the generator's manifest as THE source
        // that can answer. They measured it: 1 of their 4 generated trees emits
        // one, and the 3 without hold 12,000 of 16,000 generated files. A reader
        // in a manifest-less repo is then handed a method that returns nothing
        // and falls back to the co-change signal the same paragraph just told
        // them misleads, which is worse than saying nothing.
        //
        // The per-file HEADER leads instead. It has the property that makes a
        // manifest good — a statement by the PRODUCER about THAT file, so a
        // sibling input cannot be swept in — without requiring the generator to
        // emit a separate artifact, and it is what actually covered their trees.
        assert!(
            m.contains("@generated") && m.contains(".openapi-generator/FILES"),
            "rank the producer's own evidence, do not pin one source: {m}"
        );
        assert!(
            m.contains("Never derive the list from which paths change together"),
            "and rule out the signal that cannot separate an input from an output: {m}"
        );
    }

    /// THE OTHER ARM, which the card named as the check that can fail: a path
    /// the repo does NOT declare must still get the merge recipe. Without this,
    /// a carve-out that swallowed the recipe entirely would pass the cell above.
    #[test]
    fn an_undeclared_diverged_path_still_gets_the_merge_recipe() {
        let dirty = vec!["hooks/pre-push".to_string(), "server/openapi.json".to_string()];
        let fresh = Freshness {
            diverged: vec!["hooks/pre-push".to_string(), "server/openapi.json".to_string()],
            generated: vec!["server/openapi.json".to_string()],
            generated_checked: 2,
            ..Default::default()
        };
        let m = build("/repo", &dirty, &Ownership::default(), &fresh, "S")
            .expect("a mixed diverged set must produce a nudge");
        assert!(m.contains("DIVERGED (GENERATED):"), "the declared half must render: {m}");
        assert!(m.contains("DIVERGED:"), "the hand-written half must render too: {m}");
        assert!(
            m.contains("merge-file -p"),
            "the hand-written path must keep its recipe: {m}"
        );
        assert!(
            m.contains("1 of 2 diverged path(s) are marked"),
            "say how many of how many, so a partial list is visible: {m}"
        );
    }

    /// AN EMPTY RESULT HAS TWO CAUSES AND THE MESSAGE MUST SEPARATE THEM
    /// (ethos rule 4). A repo with no `.gitattributes` yields zero marked paths,
    /// which is not a finding that none are generated. This is the whole answer
    /// to "a declared list goes stale silently", so it is asserted rather than
    /// left to the prose.
    #[test]
    fn nothing_marked_is_reported_as_a_statement_about_gitattributes() {
        let dirty = vec!["server/openapi.json".to_string()];
        let fresh = Freshness {
            diverged: vec!["server/openapi.json".to_string()],
            generated_checked: 1,
            ..Default::default()
        };
        let m = build("/repo", &dirty, &Ownership::default(), &fresh, "S")
            .expect("diverged section must render");
        assert!(
            m.contains("NONE of these 1 path(s) is marked generated"),
            "publish the denominator beside the zero: {m}"
        );
        assert!(
            m.contains("NOT a finding that none are generated"),
            "an absent list must not read as a measurement: {m}"
        );
        assert!(
            m.contains("linguist-generated=true"),
            "hand the reader the line that fixes it, or the capability reaches nobody: {m}"
        );
        // WHOSE .gitattributes THE ZERO IS ABOUT (backend, 2026-09-04). They hit
        // this within an hour of the feature shipping: their working tree is a
        // stale graft-push checkout whose HEAD predates mixpeek's
        // .gitattributes, so the file read as ABSENT locally and they started
        // writing a worse one over origin's 38-line version. `check-attr` reads
        // the CHECKOUT, so this arm's zero is a statement about the local tree,
        // and the honest instruction is to check origin's CONTENT rather than
        // whether the path differs.
        assert!(
            m.contains("git show origin/main:.gitattributes"),
            "the zero is about THIS checkout; say so before someone regresses origin's: {m}"
        );
        assert!(m.contains("merge-file -p"), "and the recipe still applies here: {m}");
    }

    /// A PROBE THAT COULD NOT RUN MUST NOT READ AS ONE THAT FOUND NOTHING.
    /// `generated` is empty in both states; only `generated_stopped_by`
    /// separates them, and if the renderer ignores it the two collapse into the
    /// same reassuring sentence.
    #[test]
    fn a_failed_attribute_probe_says_so_instead_of_none_generated() {
        let dirty = vec!["server/openapi.json".to_string()];
        let fresh = Freshness {
            diverged: vec!["server/openapi.json".to_string()],
            generated_checked: 0,
            generated_stopped_by: "error",
            ..Default::default()
        };
        let m = build("/repo", &dirty, &Ownership::default(), &fresh, "S")
            .expect("diverged section must render");
        assert!(
            m.contains("GENERATED-FILE CHECK DID NOT RUN"),
            "an unrunnable probe must say so: {m}"
        );
        assert!(
            !m.contains("NONE of these"),
            "and must not borrow the wording of a probe that ran: {m}"
        );
    }

    /// THE PARSE, AGAINST REAL `git check-attr` OUTPUT (AF-428).
    ///
    /// `-z` emits a flat NUL-separated stream of (path, attr, value) triples,
    /// not one record per line, and the default format is `<path>: <attr>:
    /// <value>` which a path containing a colon splits wrong. Both of those are
    /// beliefs about a tool, and a belief about a tool is exactly what a unit
    /// test over a hand-written fixture cannot check. This runs the real git.
    #[tokio::test]
    async fn the_generated_probe_reads_a_real_gitattributes() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        let sh = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(root)
                .output()
                .expect("git")
        };
        sh(&["init", "-q", "--initial-branch=main"]);
        sh(&["config", "user.email", "t@t"]);
        sh(&["config", "user.name", "t"]);
        // ALL FOUR VALUES GIT CAN REPORT, because the vocabulary is the part a
        // reader gets wrong. `=true` (the form GitHub documents) reports "true";
        // a BARE attribute reports "set"; `=false` reports "false"; an unlisted
        // path reports "unspecified". Taking only "set", which my first version
        // did, matched the one spelling nobody writes and read as correct.
        std::fs::write(
            root.join(".gitattributes"),
            "gen/*.json linguist-generated=true\n\
             bare.json linguist-generated\n\
             notgen.json linguist-generated=false\n",
        )
        .unwrap();
        std::fs::create_dir_all(root.join("gen")).unwrap();
        std::fs::write(root.join("gen/openapi.json"), "{}\n").unwrap();
        std::fs::write(root.join("bare.json"), "{}\n").unwrap();
        std::fs::write(root.join("notgen.json"), "{}\n").unwrap();
        std::fs::write(root.join("hand.rs"), "fn main() {}\n").unwrap();
        sh(&["add", "-A"]);
        sh(&["commit", "-qm", "base"]);

        let mut fresh = Freshness {
            diverged: vec![
                "gen/openapi.json".to_string(),
                "bare.json".to_string(),
                "notgen.json".to_string(),
                "hand.rs".to_string(),
            ],
            ..Default::default()
        };
        mark_generated(&root.to_string_lossy(), &mut fresh).await;
        assert_eq!(
            fresh.generated,
            vec!["gen/openapi.json".to_string(), "bare.json".to_string()],
            "=true and a bare attribute both mark; =false and unspecified must not"
        );
        assert_eq!(fresh.generated_checked, 4, "coverage is the whole diverged set");
        assert_eq!(fresh.generated_stopped_by, "", "the probe ran");

        // CONTROL: the same call against a repo that declares NOTHING must come
        // back empty WITH coverage, not empty-and-unmeasured. Without it, a
        // probe that always returned nothing would pass the assertions above by
        // way of the first repo's single match being a coincidence of parsing.
        let bare = tempfile::tempdir().expect("tempdir");
        std::process::Command::new("git")
            .args(["init", "-q", "--initial-branch=main"])
            .current_dir(bare.path())
            .output()
            .expect("git");
        let mut none = Freshness {
            diverged: vec!["gen/openapi.json".to_string()],
            ..Default::default()
        };
        mark_generated(&bare.path().to_string_lossy(), &mut none).await;
        assert!(none.generated.is_empty(), "no .gitattributes means nothing is marked");
        assert_eq!(none.generated_checked, 1, "but the probe DID run, and must say so");
        assert_eq!(none.generated_stopped_by, "", "an empty result is not an error");
    }

    #[test]
    fn the_protocols_diverged_bullet_carries_the_merge_it_prescribes() {
        let dirty = vec!["src/thing.rs".to_string()];
        // Commit-worthy: in `dirty` and in none of the other buckets, which is
        // what makes `commit_worthy_body` render at all.
        let fresh = Freshness::default();
        let m = build("/repo", &dirty, &Ownership::default(), &fresh, "S")
            .expect("a commit-worthy path must render the protocol");
        assert!(
            m.contains("if BOTH directions print commits"),
            "premise: the decision protocol must be the block under test: {m}"
        );
        assert!(
            m.contains("git merge-file -p"),
            "the protocol's DIVERGED branch must carry the merge, not just name it: {m}"
        );
        assert!(
            m.contains("conflicts=$?"),
            "and the conflict count, which is what decides whose problem it is: {m}"
        );
    }

    /// The STALE section's RESTORE-SAFETY check must be the reachable-from-a-commit
    /// test, never blob existence (AMUX-3264, cold-outbound near-miss 2026-08-17).
    ///
    /// 5b923db fixed the two DIRECTION branches but DELIBERATELY left this section
    /// using `git cat-file -e $(git hash-object <path>)`, reasoning it answered
    /// pure-old-copy vs mid-edit correctly. It does not: `git add` alone writes the
    /// blob into the object DB without committing, so the recipe returns EXISTS for
    /// a never-committed mid-edit and its `git checkout` remedy DELETES it. This
    /// asserts on the message TEXT so a reintroduction of the recipe fails here.
    #[test]
    fn stale_section_prescribes_find_object_never_blob_existence() {
        let dirty = vec!["a.rs".to_string()];
        let fresh = Freshness { stale: vec!["a.rs".to_string()], ..Default::default() };
        let m = build("/repo", &dirty, &Ownership::default(), &fresh, "S")
            .expect("stale section");
        assert!(m.contains("STALE:"), "premise: the stale section must render: {m}");
        // The correct restore-safety discriminator must be prescribed. A revert to
        // the old recipe removes this substring and fails the test.
        assert!(
            m.contains("--find-object=$(git hash-object <path>)"),
            "STALE section must prescribe find-object as the restore-safety check: {m}"
        );
        // The exact old prescription tied "object EXISTS" to "committed before" to
        // "safely restores". Blob existence may only appear inside a do-not warning.
        assert!(
            !m.contains("object EXISTS, this exact content was committed before"),
            "STALE section reintroduced blob-existence as the safe-to-restore check; git add \
             writes the blob uncommitted, so its restore deletes mid-edits: {m}"
        );
        assert!(
            m.contains("Do NOT substitute `git cat-file -e $(git hash-object <path>)`"),
            "STALE section must name blob-existence as the recipe NOT to use: {m}"
        );
    }

    /// cold-outbound's proof at the DISCRIMINATOR level (AMUX-3264, live 4-minute
    /// near-miss on .github/workflows/server-fast-checks.yml, 2026-08-17), run
    /// against a REAL repo because the whole defect is a disagreement between what
    /// the object DB holds and what any commit holds.
    ///
    /// The restore-safety check must separate a pure old copy (safe to
    /// `git checkout origin/main -- <path>`) from a NOVEL mid-edit (whose restore
    /// is an irreversible delete), and it must FAIL CLOSED: ANY inconclusive answer
    /// resolves to DO-NOT-RESTORE, never to safe. This asserts the PROPERTY across
    /// states, not one case, because failing OPEN on some third state is exactly
    /// how `git cat-file -e $(git hash-object <path>)` got here.
    #[test]
    fn find_object_restore_check_fails_closed_on_every_inconclusive_state() {
        let root = std::env::temp_dir().join(format!("amux-nudge-disc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let run = |args: &[&str]| -> std::process::Output {
            std::process::Command::new("git").arg("-C").arg(&root).args(args).output().unwrap()
        };
        let must = |args: &[&str]| {
            let out = run(args);
            assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        };
        let blob_of = |name: &str| {
            String::from_utf8(run(&["hash-object", name]).stdout).unwrap().trim().to_string()
        };
        // The exact discriminator the advice prescribes, applied through the
        // fail-closed rule the advice states: RESTORE only on a non-empty print
        // from a command that SUCCEEDED; every other outcome (empty, error,
        // timeout) is DO-NOT-RESTORE.
        let safe_to_restore = |blob: &str, path: &str| -> bool {
            let out = run(&["log", "--all", "--oneline", &format!("--find-object={blob}"), "--", path]);
            out.status.success() && !String::from_utf8_lossy(&out.stdout).trim().is_empty()
        };

        must(&["init", "-b", "main"]);
        must(&["config", "user.email", "t@t"]);
        must(&["config", "user.name", "t"]);

        // (1) A committed baseline at f.yml: reachable from a commit => SAFE.
        std::fs::write(root.join("f.yml"), "name: ci\njobs:\n").unwrap();
        must(&["add", "f.yml"]);
        must(&["commit", "-m", "base"]);
        let committed_blob = blob_of("f.yml");
        assert!(
            safe_to_restore(&committed_blob, "f.yml"),
            "a genuine committed old copy must be classified safe to restore"
        );

        // (2) cold-outbound's specimen: a NEW line typed in, `git add`-ed, NEVER
        // committed. The blob is now in the object DB, so the OLD recipe answers
        // EXISTS, but it is in NO commit, so find-object is empty => DO-NOT-RESTORE.
        std::fs::write(root.join("f.yml"), "name: ci\njobs:\n  build: {}\n").unwrap();
        must(&["add", "f.yml"]);
        let staged_blob = blob_of("f.yml");
        assert_ne!(staged_blob, committed_blob, "premise: the mid-edit differs from the committed copy");
        assert!(
            run(&["cat-file", "-e", &staged_blob]).status.success(),
            "premise: git add wrote the blob, so the UNSOUND recipe cat-file -e reports EXISTS"
        );
        assert!(
            !safe_to_restore(&staged_blob, "f.yml"),
            "a staged-but-never-committed mid-edit must be DO-NOT-RESTORE (its restore is a delete)"
        );

        // (3) A blob committed under a DIFFERENT path, queried under THIS path with
        // the `-- <path>` pathspec: not-found here => DO-NOT-RESTORE. A false novel,
        // deliberately so: the pathspec biases toward the safe side.
        std::fs::write(root.join("moved.yml"), "shared: fixture\n").unwrap();
        must(&["add", "moved.yml"]);
        must(&["commit", "-m", "moved"]);
        let moved_blob = blob_of("moved.yml");
        assert!(
            !safe_to_restore(&moved_blob, "f.yml"),
            "content committed only under another path must be DO-NOT-RESTORE for THIS path"
        );

        // (4) A blob committed only on ANOTHER branch: a HEAD-only search reads it
        // as a false novel, which is why the advice uses `--all`. With `--all` it
        // is recognised as a genuine old copy => SAFE.
        must(&["checkout", "-b", "other"]);
        std::fs::write(root.join("f.yml"), "name: ci\njobs:\n  test: {}\n").unwrap();
        must(&["add", "f.yml"]);
        must(&["commit", "-m", "on other"]);
        let other_branch_blob = blob_of("f.yml");
        must(&["checkout", "main"]);
        let head_only = run(&["log", "--oneline", &format!("--find-object={other_branch_blob}"), "--", "f.yml"]);
        assert!(
            String::from_utf8_lossy(&head_only.stdout).trim().is_empty(),
            "premise: a HEAD-only search misses an other-branch blob, which is why --all is required"
        );
        assert!(
            safe_to_restore(&other_branch_blob, "f.yml"),
            "--all must recognise a blob committed on another branch as a genuine old copy"
        );

        // (5) A malformed object id: the command cannot answer => DO-NOT-RESTORE.
        // Fail closed on error, never fall through to safe.
        assert!(
            !safe_to_restore("zzz", "f.yml"),
            "an errored or inconclusive discriminator must resolve to DO-NOT-RESTORE"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// The reported case, run against a REAL repo because the whole defect is a
    /// disagreement between what `git status` says and what origin holds — a
    /// mocked porcelain string cannot express it (creative-dna, 2026-08-10).
    ///
    /// A file added on origin AFTER local HEAD reads `??` from `git status`,
    /// because the local index predates the commit that added it. The earlier
    /// comment here asserted untracked paths "cannot match origin by
    /// definition" and this test is what refutes it: the assertion below fails
    /// the moment anyone "corrects" the code to trust the porcelain letter.
    #[tokio::test]
    async fn untracked_but_present_on_origin_is_a_phantom() {
        let root = std::env::temp_dir().join(format!("amux-nudge-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let (bare, work) = (root.join("origin.git"), root.join("work"));
        std::fs::create_dir_all(&work).unwrap();
        let git = |dir: &std::path::Path, args: &[&str]| {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(args)
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        };
        std::process::Command::new("git")
            .args(["init", "--bare", "-b", "main"])
            .arg(&bare)
            .output()
            .unwrap();
        git(&work, &["init", "-b", "main"]);
        git(&work, &["config", "user.email", "t@t"]);
        git(&work, &["config", "user.name", "t"]);
        git(&work, &["remote", "add", "origin", bare.to_str().unwrap()]);
        std::fs::write(work.join("base.txt"), "base\n").unwrap();
        git(&work, &["add", "-A"]);
        git(&work, &["commit", "-m", "base"]);
        git(&work, &["push", "-q", "origin", "main"]);

        // A peer lands a file on origin; we fetch the ref but never merge, so
        // local HEAD does not contain it.
        let peer = root.join("peer");
        std::process::Command::new("git")
            .args(["clone", "-q", bare.to_str().unwrap()])
            .arg(&peer)
            .output()
            .unwrap();
        git(&peer, &["config", "user.email", "t@t"]);
        git(&peer, &["config", "user.name", "t"]);
        std::fs::write(peer.join("landed.txt"), "from peer\n").unwrap();
        git(&peer, &["add", "-A"]);
        git(&peer, &["commit", "-m", "peer"]);
        git(&peer, &["push", "-q", "origin", "main"]);

        // Same bytes appear locally. `git status` calls this UNTRACKED.
        std::fs::write(work.join("landed.txt"), "from peer\n").unwrap();
        // ...and genuine unprotected WIP, which must survive the filter.
        std::fs::write(work.join("mywip.txt"), "mine\n").unwrap();

        let dir = work.to_str().unwrap();
        let porcelain = String::from_utf8(
            std::process::Command::new("git")
                .args(["-C", dir, "status", "--porcelain"])
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap();
        assert!(porcelain.contains("?? landed.txt"), "premise: {porcelain}");

        let (kept, provenance) = drop_paths_identical_to_origin(
            dir,
            vec!["landed.txt".into(), "mywip.txt".into()],
        )
        .await;

        assert!(!kept.contains(&"landed.txt".to_string()), "untracked-but-on-origin is a phantom");
        assert!(kept.contains(&"mywip.txt".to_string()), "real WIP must never be dropped");
        assert!(provenance.contains("origin/main"), "must state what it compared against");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A WHOLLY-UNTRACKED DIRECTORY MUST NEVER REACH A READER AS ONE PATH.
    ///
    /// `--untracked-files=normal` collapses it to one entry with a trailing
    /// slash, and no consumer downstream of `dirty_paths` looks for that slash,
    /// so it was classified, labelled and rendered as though it were a file --
    /// with `git checkout origin/main -- <path>` as the remedy. Aimed at a
    /// directory that deletes every file beneath it.
    ///
    /// The fixture is the incident: an rtsp provider directory where one file
    /// matches origin exactly and one carries novel uncommitted work. That mix
    /// is the whole point. A directory-level verdict can only be right about
    /// one of them, and the remedy it licenses destroys the other.
    ///
    /// This asserts the PREMISE first. Without it the test passes trivially the
    /// day git stops collapsing directories, and a green would mean nothing.
    #[tokio::test]
    async fn an_untracked_directory_is_expanded_to_files() {
        let root =
            std::env::temp_dir().join(format!("amux-nudge-dir-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let (bare, work) = (root.join("origin.git"), root.join("work"));
        std::fs::create_dir_all(&work).unwrap();
        let git = |dir: &std::path::Path, args: &[&str]| {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(args)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        std::process::Command::new("git")
            .args(["init", "--bare", "-b", "main"])
            .arg(&bare)
            .output()
            .unwrap();
        git(&work, &["init", "-b", "main"]);
        git(&work, &["config", "user.email", "t@t"]);
        git(&work, &["config", "user.name", "t"]);
        git(&work, &["remote", "add", "origin", bare.to_str().unwrap()]);
        std::fs::write(work.join("base.txt"), "base\n").unwrap();
        git(&work, &["add", "-A"]);
        git(&work, &["commit", "-m", "base"]);
        git(&work, &["push", "-q", "origin", "main"]);

        // A peer lands the directory on origin. Local HEAD never gets it, which
        // is why git calls the whole directory untracked below.
        let peer = root.join("peer");
        std::process::Command::new("git")
            .args(["clone", "-q", bare.to_str().unwrap()])
            .arg(&peer)
            .output()
            .unwrap();
        git(&peer, &["config", "user.email", "t@t"]);
        git(&peer, &["config", "user.name", "t"]);
        std::fs::create_dir_all(peer.join("rtsp")).unwrap();
        std::fs::write(peer.join("rtsp/__init__.py"), "shipped\n").unwrap();
        std::fs::write(peer.join("rtsp/sync_provider.py"), "shipped\n").unwrap();
        git(&peer, &["add", "-A"]);
        git(&peer, &["commit", "-m", "rtsp"]);
        git(&peer, &["push", "-q", "origin", "main"]);
        git(&work, &["fetch", "-q", "origin", "main"]);

        std::fs::create_dir_all(work.join("rtsp")).unwrap();
        std::fs::write(work.join("rtsp/__init__.py"), "shipped\n").unwrap();
        // ...and one file mid-edit, in no commit on any ref. A restore aimed at
        // the DIRECTORY deletes exactly this.
        std::fs::write(work.join("rtsp/sync_provider.py"), "my novel wip\n").unwrap();

        let dir = work.to_str().unwrap();
        let porcelain = String::from_utf8(
            std::process::Command::new("git")
                .args(["-C", dir, "status", "--porcelain", "--untracked-files=normal"])
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap();
        assert!(
            porcelain.contains("?? rtsp/"),
            "PREMISE: git must still collapse the directory, else this test proves nothing: {porcelain}"
        );

        let paths = dirty_paths(dir).await;

        assert!(
            !paths.iter().any(|p| p.ends_with('/')),
            "a directory reached the caller as a restore target: {paths:?}"
        );
        assert!(
            paths.contains(&"rtsp/sync_provider.py".to_string()),
            "the novel mid-edit file must be named, or nobody is warned about it: {paths:?}"
        );
        assert!(
            paths.contains(&"rtsp/__init__.py".to_string()),
            "expansion must list every file, not a guess at which ones matter: {paths:?}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    /// TWO EMPTY ANSWERS ARE NOT A MATCH (creative-dna's follow-up).
    ///
    /// The comparison used to be `stdout == stdout` on two unvalidated strings,
    /// gated on exit status. The conservative arm covers "one side failed" —
    /// but NOT the case where both come back empty, where `"" == ""` is true
    /// and the path is DROPPED. Keeping a phantom is noise; dropping a real
    /// uncommitted file removes somebody's work from the only notice that
    /// mentions it, which is the failure worth engineering against.
    ///
    /// Asserts the shape rule directly, because provoking git into returning
    /// two empty stdouts on demand is not something a test should stage — the
    /// rule is what the fix rests on, so the rule is what is pinned.
    #[test]
    fn an_empty_or_malformed_blob_id_is_never_a_match() {
        let looks_like_blob = |t: &str| t.len() == 40 && t.chars().all(|c| c.is_ascii_hexdigit());
        assert!(!looks_like_blob(""), "empty must not pass as a blob id");
        assert!(!looks_like_blob("fatal: could not open"), "an error message must not pass");
        assert!(!looks_like_blob("c98e8388"), "a short hash must not pass");
        assert!(
            !looks_like_blob("z98e83889239f9ef1482196115f0edee24442b74"),
            "40 chars of non-hex must not pass"
        );
        assert!(
            looks_like_blob("c98e83889239f9ef1482196115f0edee24442b74"),
            "a real blob id must pass, or the filter drops everything and the guard goes silent"
        );
    }

    /// THE PHANTOM FILTER MUST WORK FROM A SUBDIRECTORY (AMUX-2947).
    ///
    /// `git status --porcelain` emits repo-root-relative paths, but a lane's
    /// CC_DIR is often a subdir. Run from there, `git hash-object -- <path>`
    /// resolves against the CWD and fails, the comparison cannot be made, and
    /// the conservative "keep it" arm keeps EVERYTHING — so the filter became a
    /// no-op while its provenance line still said "compared against
    /// origin/main". creative-dna measured 34 of 55 reported paths as
    /// byte-identical to origin: 62% false.
    ///
    /// Drives the real function against a real git repo, because the bug is
    /// entirely in how git resolves a path relative to a process CWD — a mocked
    /// git would have reproduced nothing.
    /// AF-471. A GRAFT TWIN is the same CONTENT under a different commit, and
    /// the commit-graph direction test cannot see the difference.
    ///
    /// Measured on ~/Dev/mixpeek 2026-09-04 (behind=1681, ahead=767), reported by
    /// mixpeek-general, reproduced independently here and sampled again by
    /// mixpeek-cicd: of 1815 paths flagged by `git log HEAD..origin/main`, 1080
    /// actually differed. 737 false flags, 40.6%. Their specimen was
    /// origin/main:scripts/commit-retry.sh and f4cc71ed5a:scripts/commit-retry.sh
    /// holding blob 596724c016 under two shas.
    ///
    /// Constructed rather than sampled BECAUSE ~/Dev/amux cannot reproduce it:
    /// behind=0 here, so the flagged set is empty and the rate is 0/0. A defect
    /// that needs a diverged checkout still needs a test on one that is not.
    #[tokio::test]
    async fn a_graft_twin_is_not_stale_however_the_commit_graph_reads() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        let sh = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(root)
                .output()
                .expect("git")
        };
        sh(&["init", "-q", "--initial-branch=main"]);
        sh(&["config", "user.email", "t@t"]);
        sh(&["config", "user.name", "t"]);
        std::fs::write(root.join("twin.txt"), "shared content\n").unwrap();
        std::fs::write(root.join("real.txt"), "mine\n").unwrap();
        sh(&["add", "-A"]);
        sh(&["commit", "-qm", "base"]);
        let base = String::from_utf8_lossy(&sh(&["rev-parse", "HEAD"]).stdout).trim().to_string();

        // ORIGIN gets a commit HEAD lacks. twin.txt keeps the SAME bytes (the
        // graft twin); real.txt genuinely changes (the control).
        sh(&["checkout", "-q", "-b", "origin-side"]);
        std::fs::write(root.join("real.txt"), "theirs\n").unwrap();
        sh(&["add", "-A"]);
        sh(&["commit", "-qm", "origin work"]);
        // Make origin's history TOUCH twin.txt and land back on the same blob.
        // Writing it to itself commits nothing (git stages no change), so the
        // path has to leave and return — which is what a graft's rebuilt commits
        // do to a file whose end state is unchanged.
        std::fs::write(root.join("twin.txt"), "shared content\nintermediate\n").unwrap();
        sh(&["commit", "-qam", "origin touches twin.txt"]);
        std::fs::write(root.join("twin.txt"), "shared content\n").unwrap();
        sh(&["commit", "-qam", "graft: twin.txt back to the original blob"]);
        let origin = String::from_utf8_lossy(&sh(&["rev-parse", "HEAD"]).stdout).trim().to_string();
        sh(&["update-ref", "refs/remotes/origin/main", &origin]);
        sh(&["checkout", "-q", &base]);
        sh(&["checkout", "-q", "-B", "main", &base]);

        // CONTROL 1: the commit-graph test must actually flag twin.txt, or this
        // test proves nothing about the guard.
        let flagged = String::from_utf8_lossy(
            &sh(&["log", "--oneline", "HEAD..origin/main", "--", "twin.txt"]).stdout).trim().to_string();
        assert!(!flagged.is_empty(),
            "the commit test must flag the twin, else the fixture is not a graft twin");
        // CONTROL 2: and the blobs must genuinely be equal.
        let a = String::from_utf8_lossy(&sh(&["rev-parse", "HEAD:twin.txt"]).stdout).trim().to_string();
        let b = String::from_utf8_lossy(&sh(&["rev-parse", "origin/main:twin.txt"]).stdout).trim().to_string();
        assert_eq!(a, b, "the fixture's twin must be the same blob under two commits");

        // Dirty both so the classifier considers them.
        std::fs::write(root.join("twin.txt"), "shared content\nlocal edit\n").unwrap();
        std::fs::write(root.join("real.txt"), "mine\nlocal edit\n").unwrap();

        let paths = vec!["twin.txt".to_string(), "real.txt".to_string()];
        let fresh = freshness_from_repo(root.to_str().unwrap(), &paths).await;

        assert!(!fresh.stale.iter().any(|p| p == "twin.txt"),
            "a graft twin must NOT be STALE: origin holds the same blob, so there is \
             nothing of origin's to revert. stale={:?}", fresh.stale);
        // THE OTHER SIDE. real.txt genuinely differs, so the guard must NOT have
        // swallowed the real signal — without this the test passes on a classifier
        // that calls everything fresh.
        assert!(
            fresh.stale.iter().any(|p| p == "real.txt")
                || fresh.diverged.iter().any(|p| p == "real.txt"),
            "a path whose content really differs must still be flagged; the guard \
             must clear graft twins ONLY. stale={:?} diverged={:?}",
            fresh.stale, fresh.diverged
        );
    }

    #[tokio::test]
    async fn phantom_filter_still_works_when_the_lane_dir_is_a_subdirectory() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        let sh = |args: &[&str], cwd: &std::path::Path| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(cwd)
                .output()
                .expect("git")
        };
        sh(&["init", "-q", "--initial-branch=main"], root);
        sh(&["config", "user.email", "t@t"], root);
        sh(&["config", "user.name", "t"], root);
        std::fs::create_dir_all(root.join("sub/lane")).unwrap();
        std::fs::write(root.join("tracked.txt"), "original\n").unwrap();
        sh(&["add", "-A"], root);
        sh(&["commit", "-qm", "seed"], root);
        // A local "origin/main" pointing at the same commit: the file is
        // therefore IDENTICAL to origin and must be filtered out.
        sh(&["update-ref", "refs/remotes/origin/main", "HEAD"], root);
        // Dirty the index only — worktree content still matches origin, which
        // is the phantom shape (a stale index on a graft-push checkout).
        sh(&["update-index", "--assume-unchanged", "tracked.txt"], root);
        std::fs::write(root.join("tracked.txt"), "original\n").unwrap();

        let subdir = root.join("sub/lane");
        let (kept, prov) = drop_paths_identical_to_origin(
            subdir.to_str().unwrap(),
            vec!["tracked.txt".to_string()],
        )
        .await;
        assert!(
            prov.contains("origin/main"),
            "provenance must still describe the comparison: {prov}"
        );
        assert!(
            kept.is_empty(),
            "a worktree file identical to origin must be dropped even when the lane dir is a \
             SUBDIRECTORY — got {kept:?}. Before the fix this kept everything, because \
             hash-object resolved the root-relative path against the subdir and failed."
        );
    }

    /// THE INCIDENT, rebuilt from its own numbers. 11 dirty files, none mine.
    /// Python nudged anyway and named them as work to commit; three sweeps
    /// followed. The honest output is silence.
    #[test]
    fn when_every_dirty_file_is_a_peers_there_is_no_nudge() {
        let dirty = s(&[
            "crates/a.rs", "crates/b.rs", "crates/c.rs", "crates/d.rs",
            "crates/e.rs", "crates/f.rs", "crates/g.rs", "crates/h.rs",
            "crates/i.rs", "crates/j.rs", "crates/k.rs",
        ]);
        let own = Ownership {
            foreign: dirty.iter().map(|p| (p.clone(), "amux-rust".into())).collect(),
            ..Default::default()
        };
        assert!(
            build("/repo", &dirty, &own, &Freshness::default(), "test-provenance").is_none(),
            "a session with no work of its own must not be told to commit"
        );
    }

    /// The mirror: one of mine among a peer's must still nudge, and must NOT
    /// count theirs. Python's suppression branch got this wrong by comparing a
    /// pre-filter count to a post-filter one, and ate legitimate nudges.
    #[test]
    fn one_own_file_among_foreign_ones_nudges_and_counts_only_mine() {
        let dirty = s(&["mine.rs", "theirs_a.rs", "theirs_b.rs"]);
        let own = Ownership {
            foreign: vec![
                ("theirs_a.rs".into(), "peer".into()),
                ("theirs_b.rs".into(), "peer".into()),
            ],
            ..Default::default()
        };
        let msg = build("/repo", &dirty, &own, &Freshness::default(), "test-provenance")
            .expect("should nudge");
        assert!(msg.contains("1 uncommitted change(s)"), "must count MINE only: {msg}");
        assert!(msg.contains("mine.rs"));
        assert!(!msg.contains("  theirs_a.rs\n"), "a peer's file is not work to commit: {msg}");
    }

    /// A foreign file is NAMED, not silently filtered. The recipient is about
    /// to run `git add -A`; silence about the peer's file is not the useful
    /// output.
    #[test]
    fn foreign_files_are_named_with_their_owner_and_a_do_not_commit_warning() {
        let dirty = s(&["mine.rs", "theirs.rs"]);
        let own = Ownership {
            foreign: vec![("theirs.rs".into(), "amux-cloud".into())],
            ..Default::default()
        };
        let msg = build("/repo", &dirty, &own, &Freshness::default(), "test-provenance").unwrap();
        assert!(msg.contains("NOT YOURS"), "{msg}");
        assert!(msg.contains("theirs.rs") && msg.contains("amux-cloud"), "{msg}");
        assert!(msg.contains("Do not commit it"), "{msg}");
        assert!(msg.contains("git add -A"), "name the command that causes the sweep: {msg}");
    }

    /// `shared` must NOT suppress. On a repo where two lanes touch one file
    /// routinely, suppressing would silence the nudge permanently — the
    /// opposite over-correction, and Python's comment says so explicitly.
    #[test]
    fn a_contested_file_warns_but_never_suppresses() {
        let dirty = s(&["hot.rs"]);
        let own = Ownership {
            shared: vec![("hot.rs".into(), "peer".into())],
            ..Default::default()
        };
        let msg = build("/repo", &dirty, &own, &Freshness::default(), "test-provenance")
            .expect("shared must not suppress");
        assert!(msg.contains("CONTESTED") && msg.contains("peer"), "{msg}");
        assert!(msg.contains("git add -p"), "per-hunk is the actionable advice: {msg}");
    }

    /// THE REOPEN (Ethan, 2026-08-10). This test previously asserted the
    /// OPPOSITE — that unclaimed counts as mine — and that wrong belief is
    /// exactly what shipped: the nudge told him to "commit completed work now"
    /// about CLAUDE.md, a file he had never touched, which the guard classified
    /// `unclaimed`.
    ///
    /// "No session has an edit record for it" is not "it is yours". Only a
    /// POSITIVE claim is. The honest output is the uncertainty itself.
    #[test]
    fn an_unclaimed_file_is_reported_as_unknown_never_as_yours() {
        let dirty = s(&["CLAUDE.md"]);
        let own = Ownership { unclaimed: s(&["CLAUDE.md"]), ..Default::default() };
        let msg = build("/repo", &dirty, &own, &Freshness::default(), "test-provenance")
            .expect("a dirty tree is still worth reporting");
        assert!(msg.contains("OWNERSHIP IS UNKNOWN"), "{msg}");
        assert!(
            !msg.contains("Commit completed work"),
            "must NOT instruct a commit of work that is not provably yours: {msg}"
        );
        assert!(msg.contains("git add -A"), "name the command that would sweep it: {msg}");
    }

    /// Unknowns alongside real work: the count must cover MINE only, and the
    /// unknown must be disclosed rather than folded in.
    #[test]
    fn unknown_files_are_disclosed_but_not_counted_as_mine() {
        let dirty = s(&["mine.rs", "CLAUDE.md"]);
        let own = Ownership { unclaimed: s(&["CLAUDE.md"]), ..Default::default() };
        let msg = build("/repo", &dirty, &own, &Freshness::default(), "test-provenance").unwrap();
        assert!(msg.contains("1 uncommitted change(s)"), "count MINE only: {msg}");
        assert!(msg.contains("OWNERSHIP UNKNOWN") && msg.contains("CLAUDE.md"), "{msg}");
    }

    /// A blind guard must say so. An empty `foreign` from a partially-blind
    /// scan does NOT clear a peer's files, and the nudge has to pass that on.
    #[test]
    fn partial_attribution_is_disclosed() {
        let dirty = s(&["mine.rs"]);
        let own = Ownership {
            partial: Some("no transcript for cotenant amux-helper".into()),
            ..Default::default()
        };
        let msg = build("/repo", &dirty, &own, &Freshness::default(), "test-provenance").unwrap();
        assert!(msg.contains("ATTRIBUTION IS PARTIAL"), "{msg}");
        assert!(msg.contains("amux-helper"), "name who is invisible: {msg}");
    }

    #[test]
    fn a_clean_tree_says_nothing() {
        assert!(
            build("/repo", &[], &Ownership::default(), &Freshness::default(), "test-provenance")
                .is_none()
        );
    }

    /// Ten shown, the rest elided — a nudge that pastes 82 paths is a nudge
    /// nobody reads.
    #[test]
    fn a_long_list_is_capped_but_the_count_is_honest() {
        let dirty: Vec<String> = (0..25).map(|i| format!("f{i}.rs")).collect();
        let msg = build("/repo", &dirty, &Ownership::default(), &Freshness::default(), "test-provenance")
            .unwrap();
        assert!(msg.contains("25 uncommitted change(s)"), "count must be the TRUE total: {msg}");
        assert!(msg.contains('…'), "the list must show it was truncated: {msg}");
        assert!(!msg.contains("f24.rs"), "only the first ten are listed: {msg}");
    }

    // -----------------------------------------------------------------------
    // FRESHNESS AXIS (MG-1467). build() renders it, and it is repo-blind, so
    // every case here is driven purely by the Freshness passed in. The two legs
    // that must both hold: STALE warns-not-commits, and SAME vanishes. A build
    // that renders everything fails the SAME legs; one that suppresses
    // everything fails the STALE legs.
    // -----------------------------------------------------------------------

    /// A STALE file is the INVERSE of the normal nudge. It is OLDER than origin,
    /// so committing it reverts origin. It must be rendered with the RESTORE
    /// command and NOT told to commit, and a stale-only tree carries no
    /// commit-worthy work at all.
    #[test]
    fn a_stale_file_is_told_to_restore_and_never_to_commit() {
        let dirty = s(&["stale.rs"]);
        let fresh = Freshness { stale: s(&["stale.rs"]), ..Default::default() };
        let msg = build("/repo", &dirty, &Ownership::default(), &fresh, "test-provenance")
            .expect("a stale file is worth warning about");
        assert!(msg.contains("git checkout origin/main -- "), "must give the RESTORE command: {msg}");
        // AF-336: "do not commit" is now CONDITIONAL, and that is the fix rather
        // than a rewording. This arm used to OPEN with "DO NOT COMMIT them",
        // while its own do-not-restore branch said "commit the path instead" —
        // two opposite directives in one message, with the reader left to know
        // which applied. Measured 2026-09-03 by general-canvas-apps and
        // mixpeek-homepage-claude independently: FOUR OF FOUR paths carrying
        // this label were novel mid-edits, i.e. the leading directive was the
        // wrong one every time. So the don't-commit instruction now sits inside
        // the branch where it is true.
        assert!(
            msg.contains("do NOT commit it") && msg.contains("SILENTLY REVERTS origin"),
            "the don't-commit warning must survive, attached to the genuinely-stale branch: {msg}"
        );
        assert!(
            msg.contains("COMMIT the path instead"),
            "and the opposite branch must be present, since 4 of 4 measured paths needed it: {msg}"
        );
        assert!(msg.contains("stale.rs"), "must name the stale path: {msg}");
        assert!(
            !msg.contains("uncommitted change(s)"),
            "a stale-only tree has NO commit-worthy work; it must not render the commit nudge: {msg}"
        );
    }

    /// AMUX-3188 (social-media): behind-on-history does NOT prove the worktree is
    /// a pure old copy. A stale path can carry novel mid-edit content, and an
    /// unconditional `git checkout origin/main -- <path>` DELETES it irreversibly
    /// (social-media caught 16 such paths). AMUX-3264 (cold-outbound) then showed
    /// the guard this section USED to prescribe, blob existence, is itself unsound:
    /// `git add` writes the blob uncommitted, so it licenses the same destructive
    /// restore for a never-committed mid-edit. The section must prescribe the
    /// reachable-from-a-commit test and gate the restore on it, or it prescribes
    /// data loss.
    #[test]
    fn a_stale_file_warns_before_a_blind_restore_destroys_mid_edit_work() {
        let dirty = s(&["stale.rs"]);
        let fresh = Freshness { stale: s(&["stale.rs"]), ..Default::default() };
        let msg = build("/repo", &dirty, &Ownership::default(), &fresh, "test-provenance")
            .expect("a stale file is worth warning about");
        // AF-336: the guard is no longer a caveat AFTER a directive, it is the
        // FIRST instruction. "The label is the trap, the per-path test is the
        // protocol" — so assert the ORDER, which is the property that changed.
        let test_at = msg.find("--find-object=").expect("the per-path test must be present");
        let restore_at = msg.find("git checkout origin/main -- ").expect("restore command present");
        assert!(
            test_at < restore_at,
            "the per-path test must come BEFORE the restore command it gates: {msg}"
        );
        assert!(
            msg.contains("RUN THE PER-PATH TEST FIRST"),
            "and say so, since the label alone reads as an instruction: {msg}"
        );
        assert!(msg.contains("mid-edit"), "must name the mid-edit case: {msg}");
        // The prove-it-first guard must be the reachable-from-a-commit test, and
        // the restore must be explicitly conditional on it printing a commit.
        assert!(
            msg.contains("--find-object=$(git hash-object <path>)"),
            "must give the reachable-from-a-commit guard, not blob existence: {msg}"
        );
        // Same property, re-pinned in the restructured form: the restore is
        // reachable ONLY inside the prints-a-commit branch, and the opposite
        // branch explicitly refuses it. Asserting the gate's SHAPE rather than
        // the old sentence, so a future rewording cannot pass this while
        // detaching the remedy from its condition.
        let prints_at = msg.find("PRINTS A COMMIT").expect("the positive branch must be labelled");
        assert!(
            prints_at < restore_at,
            "the restore must sit INSIDE the prints-a-commit branch, never before it: {msg}"
        );
        assert!(
            msg.contains("ANYTHING ELSE -> DO NOT RESTORE"),
            "and every other outcome must refuse the restore explicitly: {msg}"
        );
        // The unsound recipe (AMUX-3264) must never be the prescribed check.
        assert!(
            !msg.contains("object EXISTS, this exact content was committed before"),
            "must not tie blob existence to a safe restore: {msg}"
        );
    }

    /// A STALE file among genuine work: the stale warning comes FIRST, and the
    /// commit count covers only the commit-worthy paths, never the stale one.
    #[test]
    fn stale_is_rendered_first_and_excluded_from_the_commit_count() {
        let dirty = s(&["stale.rs", "new.rs"]);
        let fresh = Freshness { stale: s(&["stale.rs"]), new: s(&["new.rs"]), ..Default::default() };
        let msg = build("/repo", &dirty, &Ownership::default(), &fresh, "test-provenance").unwrap();
        let stale_at = msg.find("are behind origin/main").expect("stale block present");
        let commit_at = msg.find("uncommitted change(s)").expect("commit block present");
        assert!(stale_at < commit_at, "STALE must be rendered FIRST: {msg}");
        assert!(msg.contains("1 uncommitted change(s)"), "count MUST exclude the stale path: {msg}");
        assert!(msg.contains("new.rs"), "the commit-worthy path must be listed: {msg}");
    }

    /// AMUX-3816, the PARSE half. The render is only half the fix; if an empty
    /// owner still reaches `foreign` as a name, every consumer has to re-handle
    /// it. Tested on the shipped function rather than a paraphrase of it.
    #[test]
    fn an_owner_the_guard_could_not_resolve_never_becomes_a_name() {
        assert_eq!(owner_name(&json!({"owner": ""})), UNRESOLVED_OWNER, "empty string");
        assert_eq!(owner_name(&json!({"owner": "   "})), UNRESOLVED_OWNER, "whitespace");
        assert_eq!(owner_name(&json!({"owner": "?"})), UNRESOLVED_OWNER, "the old fallback");
        assert_eq!(owner_name(&json!({})), UNRESOLVED_OWNER, "absent key");
        assert_eq!(owner_name(&json!({"owner": null})), UNRESOLVED_OWNER, "explicit null");
        // THE CONTROL: a real name must survive untouched, trimmed. A
        // normaliser that swallowed every owner would pass all five assertions
        // above and delete the field's entire purpose.
        assert_eq!(owner_name(&json!({"owner": "ts-onboard"})), "ts-onboard");
        assert_eq!(owner_name(&json!({"owner": " peer-lane "})), "peer-lane");
    }

    /// MG-1484 (mixpeek-general's 3-vs-20): the STALE header said "of your
    /// dirty file(s)" over a set with no ownership filter, so a regen bot's
    /// churn on a shared checkout was addressed to whoever the nudge reached —
    /// 17 of 20 paths the recipient had never opened, classified by hand to
    /// find their 3. Only a positive attribution may say "your"; the rest is
    /// named as not-yours, with the owner where one is known.
    #[test]
    fn stale_paths_without_your_edit_record_are_not_called_yours() {
        let dirty = s(&["mine.rs", "sdk/model_a.py", "sdk/model_b.py"]);
        let fresh = Freshness {
            stale: s(&["mine.rs", "sdk/model_a.py", "sdk/model_b.py"]),
            ..Default::default()
        };
        let own = Ownership {
            unclaimed: s(&["sdk/model_a.py"]),
            foreign: vec![("sdk/model_b.py".into(), "peer-lane".into())],
            ..Default::default()
        };
        let msg = build("/repo", &dirty, &own, &fresh, "test-provenance").unwrap();
        assert!(
            msg.contains("1 with your edit record, 2 without"),
            "the header must split the claim instead of calling all three yours: {msg}"
        );
        assert!(
            !msg.contains("of your dirty file(s)"),
            "a mixed set must not carry the unqualified pronoun: {msg}"
        );
        assert!(msg.contains("[no edit record of yours]"), "{msg}");
        assert!(msg.contains("[peer-lane's]"), "the known owner must be named: {msg}");

        // AMUX-3816 — THE THIRD STATE. A foreign path whose owner did not
        // resolve rendered as a bare possessive with an empty name: `['s]`,
        // on every row of a whole nudge. The guard reports `owner: ""` for a
        // running cotenant with no transcript, and `unwrap_or` fires only on a
        // MISSING key, so `Some("")` reached the possessive template.
        //
        // Reported by mixpeek-frustrations with the case that makes it matter:
        // they needed a peer's name to warn them before a destructive commit,
        // and `['s]` could not tell them whether there was anyone to warn.
        let own_unresolved = Ownership {
            foreign: vec![("sdk/model_b.py".into(), UNRESOLVED_OWNER.into())],
            ..Default::default()
        };
        let msg = build("/repo", &dirty, &own_unresolved, &fresh, "test-provenance").unwrap();
        assert!(
            msg.contains("[a peer's, name unresolved]"),
            "an unresolved owner is a peer WITHOUT a name, not a nameless possessive: {msg}"
        );
        // THE CONTROLS, one per way this can go wrong. The first is the bug
        // itself; the second is the render that would replace it with a
        // different lie (nobody owns this); the third is the `?` spelling,
        // which is worse than the empty one because it looks deliberate.
        assert!(!msg.contains("['s]"), "the empty possessive must be gone: {msg}");
        assert!(
            !msg.contains("[no edit record of yours]"),
            "an unresolved OWNER is not an ABSENT owner — that is a different action: {msg}"
        );
        assert!(!msg.contains("[?'s]"), "{msg}");
        // The all-bot firing (the 20-shape with zero of yours): the header
        // must say NONE carries the recipient's record.
        let own_none = Ownership {
            unclaimed: s(&["mine.rs", "sdk/model_a.py", "sdk/model_b.py"]),
            ..Default::default()
        };
        let msg = build("/repo", &dirty, &own_none, &fresh, "test-provenance").unwrap();
        assert!(
            msg.contains("NONE carries your edit record"),
            "the zero-yours firing must say so: {msg}"
        );
    }

    /// AMUX-3436: a shared path whose OWN edit is already settled in HEAD
    /// demotes to foreign — the nudge then says NOT YOURS (with the peer
    /// named) instead of prescribing per-hunk staging over a file where the
    /// recipient owns zero dirty hunks. An unsettled shared path keeps
    /// CONTESTED, the safe direction.
    #[test]
    fn a_settled_shared_path_renders_not_yours_instead_of_contested() {
        let mut own = Ownership {
            shared: vec![
                ("app.js".into(), "desktop".into()),
                ("still-mine.rs".into(), "desktop".into()),
            ],
            ..Default::default()
        };
        let settled: BTreeSet<String> = ["app.js".to_string()].into_iter().collect();
        demote_settled_shared(&mut own, &settled);
        assert_eq!(own.shared, vec![("still-mine.rs".to_string(), "desktop".to_string())]);
        assert_eq!(own.foreign, vec![("app.js".to_string(), "desktop".to_string())]);

        let dirty = s(&["app.js", "still-mine.rs"]);
        let msg =
            build("/repo", &dirty, &own, &Freshness::default(), "test-provenance").unwrap();
        let contested =
            msg.split("CONTESTED — ").nth(1).expect("CONTESTED section: {msg}").split("\n\n").next().unwrap();
        assert!(contested.contains("still-mine.rs"), "{msg}");
        assert!(!contested.contains("app.js"), "the settled path must not read contested: {msg}");
        let notyours =
            msg.split("NOT YOURS — ").nth(1).expect("NOT YOURS section").split("\n\n").next().unwrap();
        assert!(notyours.contains("app.js"), "{msg}");
        assert!(notyours.contains("desktop"), "the peer must be named: {msg}");
    }

    /// SAME files are byte-identical to origin, dirty only because local HEAD is
    /// behind. They are pure noise and must be suppressed entirely, not counted.
    #[test]
    fn a_same_as_origin_file_is_suppressed_and_not_counted() {
        let dirty = s(&["real.rs", "same.rs"]);
        let fresh = Freshness { same: s(&["same.rs"]), ..Default::default() };
        let msg = build("/repo", &dirty, &Ownership::default(), &fresh, "test-provenance").unwrap();
        assert!(msg.contains("1 uncommitted change(s)"), "SAME must not be counted: {msg}");
        assert!(msg.contains("real.rs"), "the real change must remain: {msg}");
        assert!(!msg.contains("same.rs"), "a file identical to origin must be absent: {msg}");
    }

    /// If EVERY dirty path is identical to origin there is nothing to say, the
    /// same silence as a clean tree. 48 of 321 paths were this on the checkout
    /// that motivated MG-1467, and they are why the raw count read alarming.
    #[test]
    fn an_all_same_tree_says_nothing() {
        let dirty = s(&["a.rs", "b.rs"]);
        let fresh = Freshness { same: s(&["a.rs", "b.rs"]), ..Default::default() };
        assert!(
            build("/repo", &dirty, &Ownership::default(), &fresh, "test-provenance").is_none(),
            "an all-SAME tree is noise and must produce no nudge"
        );
    }

    /// NEW and EDITED are ordinary commit-worthy work: they keep today's
    /// messaging and are both counted. This is the "local HEAD not behind
    /// origin" world where nothing is STALE or SAME and the nudge reads exactly
    /// as it always did.
    #[test]
    fn new_and_edited_render_as_ordinary_commit_worthy_work() {
        let dirty = s(&["added.rs", "changed.rs"]);
        let fresh =
            Freshness { new: s(&["added.rs"]), edited: s(&["changed.rs"]), ..Default::default() };
        let msg = build("/repo", &dirty, &Ownership::default(), &fresh, "test-provenance").unwrap();
        assert!(msg.contains("2 uncommitted change(s)"), "both NEW and EDITED are commit-worthy: {msg}");
        assert!(msg.contains("added.rs") && msg.contains("changed.rs"), "{msg}");
    }

    /// AMUX-3367: an append-only multi-writer file (frustrations.md) can diverge
    /// BOTH ways at once, so the nudge's "commit the path" / "restore" advice both
    /// lose data on it. A dirty frustrations.md must therefore get an explicit
    /// UNION-MERGE directive, and a dirty set without one must not.
    #[test]
    fn an_append_only_shared_file_gets_a_union_merge_directive() {
        // Case-insensitive basename, any directory, ONLY frustrations.md.
        assert!(is_append_only_shared("frustrations.md"));
        assert!(is_append_only_shared("FRUSTRATIONS.md")); // macOS resolves to the same file
        assert!(is_append_only_shared("deep/path/frustrations.md"));
        assert!(!is_append_only_shared("frustrations.md.bak"));
        assert!(!is_append_only_shared("src/app.js"));

        // THROUGH `build`, NOT `commit_worthy_body` (AMUX-3718). This cell used
        // to call the inner function directly and was green for the entire time
        // the note was unreachable from the DIVERGED arm — a real property
        // asserted at a layer the broken path does not flow through.
        let dirty = s(&["frustrations.md", "app.js"]);
        let msg =
            build("/repo", &dirty, &Ownership::default(), &Freshness::default(), "S").unwrap();
        assert!(msg.contains("APPEND-ONLY SHARED FILE"), "{msg}");
        assert!(msg.contains("UNION-MERGE"), "{msg}");
        assert!(msg.contains("frustrations.md"), "{msg}");
        // CD-78's correction is protection-losing if it regresses: without the
        // archive check the directive re-injects deliberately archived entries
        // (measured 15/15 on the mixpeek checkout, three restore/remove cycles
        // on origin), so its presence is pinned like the directive itself.
        assert!(msg.contains("ARCHIVE CHECK"), "{msg}");
        assert!(msg.contains("absent from BOTH files"), "{msg}");

        // No append-only file in the set -> no such block.
        let plain = s(&["app.js", "main.rs"]);
        let msg2 =
            build("/repo", &plain, &Ownership::default(), &Freshness::default(), "S").unwrap();
        assert!(!msg2.contains("APPEND-ONLY SHARED FILE"), "{msg2}");
    }

    /// THE ARCHIVE CHECK MUST REACH THE **DIVERGED** ARM — the one state in which
    /// the nudge actually prescribes a union-merge (AMUX-3718, near-miss by
    /// mixpeek-frustrations 2026-08-25).
    ///
    /// The test above is green and pins the wrong layer (ethos rule 7 / AF-161).
    /// It calls `commit_worthy_body` directly, and `build` hands that function
    /// `commit_worthy`, which is *defined* as the dirty paths that are NOT
    /// stale/diverged/revived. So a DIVERGED frustrations.md is structurally
    /// excluded from the only code that emits the archive check, while the
    /// DIVERGED section itself says "union-merge" with the safety half behind a
    /// citation. The reader who is being told to union-merge is precisely the
    /// reader who cannot be shown how to do it safely.
    ///
    /// That is not a hypothetical: a lane followed the bare directive verbatim
    /// tonight and would have resurrected an entry closed on a 692/692 prod
    /// measurement.
    #[test]
    fn a_diverged_append_only_file_still_gets_the_archive_check() {
        let dirty = s(&["FRUSTRATIONS.md"]);
        let fresh = Freshness { diverged: s(&["FRUSTRATIONS.md"]), ..Default::default() };
        let m = build("/repo", &dirty, &Ownership::default(), &fresh, "S")
            .expect("a diverged append-only path must still produce a nudge");

        // PREMISE: we are in the arm that prescribes the merge. Without this the
        // assertions below could pass from some other section and prove nothing.
        assert!(m.contains("DIVERGED:"), "premise: the diverged arm must be the one firing: {m}");
        assert!(m.contains("MERGE the two versions"), "premise: a merge must be prescribed: {m}");

        assert!(
            m.contains("ARCHIVE CHECK"),
            "the arm that prescribes a union-merge must carry CD-78's archive check, or it \
             prescribes the destructive half alone: {m}"
        );
        assert!(m.contains("absent from BOTH files"), "{m}");
    }

    /// THE DELIVERY-TIME WARN MUST DISCRIMINATE (AMUX-3718, second fix).
    ///
    /// A check that cannot fire is theatre and a check that always fires is
    /// noise, so both directions are pinned — and the positive case is built
    /// from the ACTUAL pre-fix text, not from a convenient string. The specimen
    /// below is the DIVERGED section as it shipped, which is the message a lane
    /// really received on 2026-08-25.
    #[test]
    fn the_archive_check_warn_fires_on_the_real_specimen_and_stays_quiet_on_healthy_text() {
        let dirty = s(&["FRUSTRATIONS.md"]);
        let fresh = Freshness::default();

        // The pre-fix bytes: prescribes the merge, carries no archive check.
        let broken = "DIVERGED: ... MERGE the two versions (for append-only files, union-merge \
                      per .claude/rules/frustrations.md), or hand the path to its owner.";
        assert_eq!(
            missing_archive_check(&dirty, &fresh, broken).as_deref(),
            Some("FRUSTRATIONS.md"),
            "the WARN must fire on the exact text that shipped, or it certifies the bug"
        );

        // The shipped text, end to end, in EVERY arm — including DIVERGED, the
        // arm the bug lived in. Pinning only the default arm would leave the
        // runtime instrument green against the exact regression it exists to
        // catch, which is the wrong-layer failure one level out.
        for f in [
            Freshness::default(),
            Freshness { diverged: s(&["FRUSTRATIONS.md"]), ..Default::default() },
            Freshness { stale: s(&["FRUSTRATIONS.md"]), ..Default::default() },
            Freshness { revived: s(&["FRUSTRATIONS.md"]), ..Default::default() },
        ] {
            let live = build("/repo", &dirty, &Ownership::default(), &f, "S").unwrap();
            assert!(
                missing_archive_check(&dirty, &f, &live).is_none(),
                "the fixed nudge must not trip its own WARN: {live}"
            );
        }

        // No append-only file in the set -> silent, however the message reads.
        let plain = s(&["src/app.js"]);
        assert!(missing_archive_check(&plain, &fresh, broken).is_none());

        // SAME is dropped by `build` before rendering, so a byte-identical
        // frustrations.md legitimately produces no note and must not warn.
        let same_fresh = Freshness { same: s(&["FRUSTRATIONS.md"]), ..Default::default() };
        assert!(
            missing_archive_check(&dirty, &same_fresh, broken).is_none(),
            "the checker must share `build`'s same-filter, not re-derive it"
        );
    }

    /// A CAVEAT ABOUT THE WHOLE DIRTY SET MUST REACH EVERY ARM (AMUX-3718,
    /// second instance, flagged by mixpeek-frustrations).
    ///
    /// The archive check and the partial-attribution disclosure are both
    /// properties of the SET, not of whichever remedy the set happens to be
    /// under. Both were emitted from `commit_worthy_body`, which only ever sees
    /// `commit_worthy` — the paths that are NOT stale/diverged/revived — so both
    /// vanished from exactly the arms that most needed them. Measured before
    /// fixing, with a control: diverged+partial rendered false, ordinary
    /// commit-worthy+partial rendered true.
    ///
    /// This cell exists so the SHAPE cannot come back quietly. It is a matrix,
    /// not a case: a third caveat added to the wrong function fails here the
    /// moment someone adds its arm to the list.
    #[test]
    fn set_wide_caveats_render_in_every_arm_not_only_the_commit_worthy_one() {
        let dirty = s(&["FRUSTRATIONS.md"]);
        let own = Ownership {
            partial: Some("no transcript for cotenant amux-helper".into()),
            ..Default::default()
        };
        // Every arm `build` can render, including the default (commit-worthy)
        // one, which is the control: if IT ever goes false the test is broken
        // rather than the code.
        let arms: [(&str, Freshness); 4] = [
            ("commit-worthy", Freshness::default()),
            ("diverged", Freshness { diverged: s(&["FRUSTRATIONS.md"]), ..Default::default() }),
            ("stale", Freshness { stale: s(&["FRUSTRATIONS.md"]), ..Default::default() }),
            ("revived", Freshness { revived: s(&["FRUSTRATIONS.md"]), ..Default::default() }),
        ];
        for (arm, fresh) in arms {
            let m = build("/repo", &dirty, &own, &fresh, "S")
                .unwrap_or_else(|| panic!("{arm}: must render"));
            assert!(
                m.contains("ATTRIBUTION IS PARTIAL"),
                "{arm}: a blind guard must be disclosed whichever remedy is prescribed — the \
                 DIVERGED arm says 'hand the path to its owner' and this is the line saying the \
                 ownership axis cannot be trusted: {m}"
            );
            assert!(m.contains("amux-helper"), "{arm}: name who is invisible: {m}");
            assert!(
                m.contains("ARCHIVE CHECK"),
                "{arm}: the archive check is a property of the file, not of the arm: {m}"
            );
        }
    }

    /// AF-438, reported by mvs-pitr: the notice named a directory the paths are
    /// not relative to.
    ///
    /// `git status --porcelain` emits ROOT-relative paths whatever cwd it runs
    /// from, and the label was the lane's own `CC_DIR`. For a lane working in a
    /// subdirectory the two disagree and the reader concatenates them into a
    /// path that does not exist — theirs read `.../mixpeek/server/mvs/ME` for a
    /// file at `.../mixpeek/ME`.
    ///
    /// Worse than cosmetic, because git pathspecs ARE cwd-relative: an operator
    /// following the remedies from the named directory runs
    /// `git checkout origin/main -- ME` against `server/mvs/ME`, hitting a
    /// different file or none, with every command exiting 0.
    #[test]
    fn the_notice_says_what_its_paths_are_relative_to() {
        let dirty = vec!["ME".to_string(), "server/mvs/shard.rs".to_string()];
        for (label, own) in [
            ("commit-worthy", Ownership::default()),
            (
                "unknown-ownership",
                Ownership { unclaimed: dirty.clone(), ..Ownership::default() },
            ),
        ] {
            let m = build("/repo/root", &dirty, &own, &Freshness::default(), "P")
                .unwrap_or_else(|| panic!("{label} arm produced nothing"));
            assert!(
                m.contains("REPO-ROOT-RELATIVE"),
                "{label}: a reader cannot resolve these paths without knowing that: {m}"
            );
            assert!(
                m.contains("git -C /repo/root"),
                "{label}: the runnable form must name the root, or the remedy is \
                 cwd-relative and silently wrong: {m}"
            );
        }
    }

    /// THE WIRING, which is the half the other two cells cannot reach.
    ///
    /// `the_notice_says_what_its_paths_are_relative_to` calls `build` with a
    /// root it hands over itself, and `repo_root_of_resolves_a_subdirectory_to_
    /// the_root` exercises the resolver alone. Both passed while `nudge_tick`
    /// still passed the lane's `dir` — mutating the call site back to the
    /// reported bug survived all 46 tests. A test per component and none over
    /// the seam is the same shape as AF-429's producer and consumer.
    ///
    /// Reads the SOURCE, bounded to `nudge_tick`'s body, because the alternative
    /// is standing up a lane, a repo and the guard API to observe one argument.
    /// The bound matters: an unbounded `include_str!` search would be satisfied
    /// by the `repo_root_of` definition itself, several hundred lines away, and
    /// could not fail.
    #[test]
    fn the_sweep_labels_the_notice_with_the_repo_root_not_the_lane_directory() {
        const SRC: &str = include_str!("commit_nudge.rs");
        let start = SRC.find("pub async fn nudge_tick(").expect("nudge_tick is gone");
        // To the next column-0 item, so the window is this function and no more.
        let rest = &SRC[start..];
        let end = rest[1..]
            .find("\n}\n")
            .map(|i| i + 3)
            .expect("nudge_tick has no closing brace");
        let body = &rest[..end];

        assert!(
            body.contains("repo_root_of(dir)"),
            "nudge_tick must RESOLVE the root; it is the only place that knows the lane dir"
        );
        assert!(
            body.contains("build(&label,"),
            "nudge_tick must pass the resolved label to build, not the lane dir (AF-438)"
        );
        assert!(
            !body.contains("build(dir,"),
            "nudge_tick passes the lane directory to build — that is the reported bug"
        );
        // The control: the window really is nudge_tick and not the whole file,
        // or every assertion above is satisfied by unrelated code elsewhere.
        assert!(
            body.len() < SRC.len() / 4,
            "the window is {} of {} bytes — too wide to be one function",
            body.len(),
            SRC.len()
        );
        assert!(
            !body.contains("async fn repo_root_of"),
            "the window has swallowed the resolver's definition, so it cannot fail"
        );
    }

    /// The CONTROL for the resolver, and the half that would otherwise be a
    /// claim: `repo_root_of` must return the ROOT for a subdirectory, not the
    /// subdirectory. Without this the fix above is a nicer sentence attached to
    /// the same wrong path.
    #[tokio::test]
    async fn repo_root_of_resolves_a_subdirectory_to_the_root() {
        let t = std::env::temp_dir().join(format!("af438-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&t);
        std::fs::create_dir_all(t.join("server/mvs")).unwrap();
        let run = |args: &[&str]| {
            std::process::Command::new("git")
                .args(["-C", t.to_str().unwrap()])
                .args(args)
                .output()
                .unwrap()
        };
        run(&["init", "-q"]);
        let sub = t.join("server/mvs");
        let root = super::repo_root_of(sub.to_str().unwrap()).await;
        // macOS temp dirs are symlinked (/var -> /private/var), so compare the
        // resolved forms rather than the strings git and std happen to print.
        assert_eq!(
            std::fs::canonicalize(&root).unwrap(),
            std::fs::canonicalize(&t).unwrap(),
            "a subdirectory must resolve to the repo root, not to itself"
        );
        // And the fallback: a path in no repo at all returns itself rather than
        // an empty string, or the label would silently become "".
        let outside = std::env::temp_dir().join(format!("af438-norepo-{}", std::process::id()));
        std::fs::create_dir_all(&outside).unwrap();
        assert_eq!(
            super::repo_root_of(outside.to_str().unwrap()).await,
            outside.to_str().unwrap(),
            "outside a repo the label must fall back to the directory given"
        );
        let _ = std::fs::remove_dir_all(&t);
        let _ = std::fs::remove_dir_all(&outside);
    }

    /// THE DIFFERENTIAL FORM, WHICH NEEDS NO REGISTRY (AMUX-3718;
    /// mixpeek-frustrations, reviewing the matrix above).
    ///
    /// The matrix is a registry of strings I already know about. A third
    /// set-wide caveat pushed inside `commit_worthy_body` next month is
    /// invisible to it until someone remembers to add a row — and "someone must
    /// remember" is precisely the step that failed twice, once for the archive
    /// check and once for partial attribution.
    ///
    /// This compares two RENDERINGS of the same dirty set instead: every
    /// `\n\n`-delimited block the commit-worthy arm emits must also appear in
    /// the DIVERGED arm, minus the blocks that are legitimately arm-specific.
    /// A caveat nobody registered is caught the day it is added, by this test,
    /// without anyone touching it.
    ///
    /// It fails closed in the useful direction: adding a genuinely
    /// arm-specific note to `commit_worthy_body` also trips it, which is a
    /// prompt to name it in ARM_SPECIFIC and say why, rather than a false
    /// alarm. That list is short and every entry is a claim someone had to
    /// make deliberately.
    #[test]
    fn every_caveat_the_commit_worthy_arm_emits_also_reaches_the_diverged_arm() {
        // Blocks that belong to the commit-worthy arm BY DESIGN: they describe
        // staging work you are being told to commit, which the DIVERGED arm
        // explicitly forbids. Anything not listed here is presumed set-wide.
        const ARM_SPECIFIC: &[&str] = &[
            "uncommitted change(s)",  // the commit-worthy headline itself
            "OWNERSHIP IS UNKNOWN",   // "stage only what you recognise" advice
            "NOT YOURS",              // do-not-commit warning about staging
            "also edited by",         // shared-path staging caveat
            // The ancestry direction protocol. Flagged by this test on its first
            // run, and the judgement is that it is genuinely arm-specific: it
            // exists to decide commit-vs-restore for paths whose DIRECTION is
            // unknown. In the DIVERGED arm the direction is already known to be
            // "both", and that arm forbids both remedies outright, so running
            // the test there only leads the reader back to the verdict they were
            // already given. The safety point it carries is not lost — the
            // DIVERGED section states the same danger in its own terms,
            // including that the find-object restore-safety check PASSES while
            // reverting your landed commits.
            "IS NOT A DIRECTION",
        ];

        let dirty = s(&["FRUSTRATIONS.md", "src/app.js"]);
        let own = Ownership {
            partial: Some("no transcript for cotenant amux-helper".into()),
            ..Default::default()
        };

        let worthy = build("/repo", &dirty, &own, &Freshness::default(), "S").unwrap();
        let diverged = build(
            "/repo",
            &dirty,
            &own,
            &Freshness { diverged: s(&["FRUSTRATIONS.md", "src/app.js"]), ..Default::default() },
            "S",
        )
        .unwrap();

        // PREMISE: the two renderings must actually differ, or a bug that made
        // build() ignore `fresh` entirely would satisfy this test vacuously.
        assert_ne!(worthy, diverged, "premise: the two arms must render differently");

        let mut missing: Vec<&str> = Vec::new();
        for block in worthy.split("\n\n") {
            let block = block.trim();
            // The provenance stamp carries a wall-clock time, so the two
            // renderings differ in it by construction; compare on the marker.
            if block.is_empty() || block.starts_with("(S; tree observed") {
                continue;
            }
            if ARM_SPECIFIC.iter().any(|a| block.contains(a)) {
                continue;
            }
            // Compare on the block's own leading marker rather than the whole
            // text: the arms legitimately word counts and lists differently.
            let marker: String = block.chars().take(40).collect();
            if !diverged.contains(marker.trim()) {
                missing.push(block);
            }
        }
        assert!(
            missing.is_empty(),
            "these caveats reach the commit-worthy arm and NOT the DIVERGED arm. Either they \
             are set-wide and belong at the top of `build` (see AMUX-3718, twice), or they are \
             genuinely arm-specific and belong in ARM_SPECIFIC with a reason:\n\n{}\n\n\
             --- DIVERGED rendering was ---\n{diverged}",
            missing.join("\n---\n")
        );
    }

    /// THE PROCEDURE MUST BE INLINE, NEVER A REPO-RELATIVE CITATION (AMUX-3718).
    ///
    /// The nudge fires in EVERY lane's own checkout, and `.claude/rules/` exists
    /// in amux and in almost none of them (measured: absent in ~/Dev/mixpeek).
    /// A path citation therefore resolves for the author and dead-ends for the
    /// reader — who then either follows the dangerous half from memory or files
    /// a bug saying the file does not exist. Both happened on 2026-08-25.
    #[test]
    fn the_nudge_never_cites_a_repo_relative_path_for_its_own_procedure() {
        let dirty = s(&["FRUSTRATIONS.md"]);
        for fresh in [
            Freshness { diverged: s(&["FRUSTRATIONS.md"]), ..Default::default() },
            Freshness { stale: s(&["FRUSTRATIONS.md"]), ..Default::default() },
            Freshness { edited: s(&["FRUSTRATIONS.md"]), ..Default::default() },
        ] {
            let m = build("/repo", &dirty, &Ownership::default(), &fresh, "S").unwrap();
            assert!(
                !m.contains(".claude/rules/"),
                "the procedure must travel with the message, not as a path only the amux \
                 checkout can open: {m}"
            );
        }
    }

    /// THE EXIT-CODE DISCRIMINATOR, against a real repo (MG-1467). The classifier
    /// shells git, so the one thing worth pinning end to end is that it tests the
    /// PROCESS EXIT CODE for absence, not stdout emptiness: plain `git rev-parse`
    /// echoes an unresolvable revspec to stdout and still exits nonzero, and
    /// three sessions shipped the is-empty bug on 2026-08-10. A checkout that is
    /// behind origin exercises all four classes at once.
    #[tokio::test]
    async fn freshness_from_repo_classifies_new_same_stale_and_edited() {
        let tmp = std::env::temp_dir().join(format!("amux-fresh-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let (bare, work) = (tmp.join("origin.git"), tmp.join("work"));
        std::fs::create_dir_all(&work).unwrap();
        let git = |dir: &std::path::Path, args: &[&str]| {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(args)
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        };
        std::process::Command::new("git")
            .args(["init", "--bare", "-b", "main"])
            .arg(&bare)
            .output()
            .unwrap();
        git(&work, &["init", "-b", "main"]);
        git(&work, &["config", "user.email", "t@t"]);
        git(&work, &["config", "user.name", "t"]);
        git(&work, &["remote", "add", "origin", bare.to_str().unwrap()]);
        // Base commit (this stays HEAD): stale.txt=v0, same.txt=X, edited.txt=orig.
        std::fs::write(work.join("stale.txt"), "v0\n").unwrap();
        std::fs::write(work.join("same.txt"), "X\n").unwrap();
        std::fs::write(work.join("edited.txt"), "orig\n").unwrap();
        git(&work, &["add", "-A"]);
        git(&work, &["commit", "-m", "base"]);
        git(&work, &["push", "-q", "origin", "main"]);

        // A peer advances origin ONLY on stale.txt (v0 -> v2), pushes. Local
        // HEAD does not contain this commit.
        let peer = tmp.join("peer");
        std::process::Command::new("git")
            .args(["clone", "-q", bare.to_str().unwrap()])
            .arg(&peer)
            .output()
            .unwrap();
        git(&peer, &["config", "user.email", "t@t"]);
        git(&peer, &["config", "user.name", "t"]);
        std::fs::write(peer.join("stale.txt"), "v2\n").unwrap();
        git(&peer, &["add", "-A"]);
        git(&peer, &["commit", "-m", "peer moves stale.txt"]);
        git(&peer, &["push", "-q", "origin", "main"]);

        // Fetch the moved ref but do NOT merge: origin/main is now ahead of HEAD.
        git(&work, &["fetch", "-q", "origin"]);

        // Worktree edits that produce each class:
        //   stale.txt = v1  (differs from HEAD v0 AND from origin v2, origin moved) -> STALE
        //   edited.txt = changed (origin unmoved on this path)                      -> EDITED
        //   same.txt   = X  (byte-identical to origin)                              -> SAME
        //   new.txt    = fresh (absent from origin)                                 -> NEW
        std::fs::write(work.join("stale.txt"), "v1\n").unwrap();
        std::fs::write(work.join("edited.txt"), "changed\n").unwrap();
        std::fs::write(work.join("new.txt"), "fresh\n").unwrap();

        let dir = work.to_str().unwrap();
        let fresh = freshness_from_repo(
            dir,
            &s(&["new.txt", "same.txt", "stale.txt", "edited.txt"]),
        )
        .await;

        assert_eq!(fresh.new, s(&["new.txt"]), "absent-from-origin must be NEW (exit-code test)");
        assert_eq!(fresh.same, s(&["same.txt"]), "identical-to-origin must be SAME");
        assert_eq!(fresh.stale, s(&["stale.txt"]), "origin-ahead-on-path must be STALE");
        assert_eq!(fresh.edited, s(&["edited.txt"]), "differs, origin unmoved, must be EDITED");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// AMUX-3695: an OLD COMMITTED REVISION on disk is not an edit, and both
    /// ancestry arms are blind to it.
    ///
    /// Reported by mixpeek-frustrations reviewing the refs-agree gate. Step 2b
    /// is reached only when the worktree differs from origin while HEAD and
    /// origin AGREE, and that state holds two populations: a genuine new edit,
    /// and an old revision sitting on disk whose commit would silently revert
    /// what both refs hold. The arms cannot see the second PRECISELY because
    /// the refs agree, so there is no "origin is ahead" to find.
    ///
    /// BOTH CELLS ARE LOAD-BEARING. Without the edited one, classifying every
    /// refs-agree path as `revived` would pass the first and prescribe a
    /// restore against genuinely new work — which destroys it, and is the one
    /// direction this gate must never fail in.
    #[tokio::test]
    async fn an_old_revision_on_disk_is_revived_not_edited() {
        let tmp = std::env::temp_dir().join(format!("amux-revived-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let (bare, work) = (tmp.join("origin.git"), tmp.join("work"));
        std::fs::create_dir_all(&work).unwrap();
        let git = |dir: &std::path::Path, args: &[&str]| {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(args)
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        };
        std::process::Command::new("git")
            .args(["init", "--bare", "-b", "main"])
            .arg(&bare)
            .output()
            .unwrap();
        git(&work, &["init", "-b", "main"]);
        git(&work, &["config", "user.email", "t@t"]);
        git(&work, &["config", "user.name", "t"]);
        git(&work, &["remote", "add", "origin", bare.to_str().unwrap()]);

        // v1 of the path is COMMITTED, then superseded by v2. Both refs end up
        // agreeing on v2, which is the state that blinds the ancestry arms.
        std::fs::write(work.join("revived.txt"), "v1-old\n").unwrap();
        std::fs::write(work.join("edited.txt"), "orig\n").unwrap();
        git(&work, &["add", "-A"]);
        git(&work, &["commit", "-m", "v1"]);
        std::fs::write(work.join("revived.txt"), "v2-current\n").unwrap();
        git(&work, &["add", "-A"]);
        git(&work, &["commit", "-m", "v2"]);
        git(&work, &["push", "-q", "origin", "main"]);
        git(&work, &["fetch", "-q", "origin"]);

        // THE SPECIMEN: put the OLD committed content back on disk. Byte for
        // byte v1, which is a real commit in this repo's history.
        std::fs::write(work.join("revived.txt"), "v1-old\n").unwrap();
        // The control: content that was never committed anywhere.
        std::fs::write(work.join("edited.txt"), "genuinely new\n").unwrap();

        let dir = work.to_str().unwrap();
        let fresh = freshness_from_repo(dir, &s(&["revived.txt", "edited.txt"])).await;

        assert_eq!(
            fresh.revived,
            s(&["revived.txt"]),
            "an old COMMITTED revision on disk reverts what both refs hold — not an edit: {fresh:?}"
        );
        assert_eq!(
            fresh.edited,
            s(&["edited.txt"]),
            "content committed nowhere is novel work, and a restore would DESTROY it: {fresh:?}"
        );

        // AND IT MUST REACH THE READER. A bucket that classifies correctly and
        // renders nothing is worse than no bucket: the path silently leaves
        // commit_worthy and the nudge stops naming it at all, so a silent
        // revert becomes an invisible one.
        let n = build(dir, &s(&["revived.txt", "edited.txt"]), &Default::default(), &fresh, "")
            .expect("a dirty tree must produce a nudge");
        // The PROPERTY, not the exact heading: the heading now varies with the
        // share (a majority-revived checkout gets a different lede), and pinning
        // the wording would make improving that message look like a regression
        // — which is exactly how this assertion first went red.
        assert!(
            n.contains("OLD REVISION") && n.contains("ALREADY COMMITTED"),
            "the section must render: {n}"
        );
        assert!(n.contains("revived.txt"), "and must name the path: {n}");
        assert!(
            n.contains("SILENTLY REVERTS"),
            "and must say what committing it does, not merely that it is odd: {n}"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// AMUX-3695 follow-up: the discriminator is BOUNDED, and the bound is
    /// never silent.
    ///
    /// mixpeek-frustrations measured the shipped version on a repo 6x this one
    /// (594 refs, 26,190 commits, 43,837 files) and the cost model did not
    /// survive: a walk costs 691ms there against 103ms here, and the O(1)
    /// prefilter INVERTS — 58 of 59 sampled dirty paths already had their blob
    /// in the object database, so 98% paid the full walk. Their idle nudge that
    /// morning listed 772 paths. Unbounded, that is minutes of git subprocesses
    /// on a shared checkout, i.e. a detector paying its cost in the same
    /// resource the sessions need.
    ///
    /// THE SECOND CELL IS THE ONE THAT MATTERS. Falling back to `edited` is the
    /// safe direction, and it is also byte-identical to the output for a path
    /// that WAS checked and found novel. If the nudge does not say which
    /// happened, a bound becomes a false statement about coverage.
    #[tokio::test]
    async fn the_revived_check_is_bounded_and_says_what_it_skipped() {
        let tmp = std::env::temp_dir().join(format!("amux-revcap-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let (bare, work) = (tmp.join("origin.git"), tmp.join("work"));
        std::fs::create_dir_all(&work).unwrap();
        let git = |dir: &std::path::Path, args: &[&str]| {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(args)
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        };
        std::process::Command::new("git")
            .args(["init", "--bare", "-b", "main"])
            .arg(&bare)
            .output()
            .unwrap();
        git(&work, &["init", "-b", "main"]);
        git(&work, &["config", "user.email", "t@t"]);
        git(&work, &["config", "user.name", "t"]);
        git(&work, &["remote", "add", "origin", bare.to_str().unwrap()]);

        // Six paths, each with a real v1 superseded by v2, so every one of them
        // WOULD classify as revived if the budget allowed.
        let names: Vec<String> = (0..6).map(|i| format!("r{i}.txt")).collect();
        for n in &names {
            std::fs::write(work.join(n), "v1-old\n").unwrap();
        }
        git(&work, &["add", "-A"]);
        git(&work, &["commit", "-m", "v1"]);
        for n in &names {
            std::fs::write(work.join(n), "v2-current\n").unwrap();
        }
        git(&work, &["add", "-A"]);
        git(&work, &["commit", "-m", "v2"]);
        git(&work, &["push", "-q", "origin", "main"]);
        git(&work, &["fetch", "-q", "origin"]);
        for n in &names {
            std::fs::write(work.join(n), "v1-old\n").unwrap();
        }

        // Cap at 2. Set on this process only; the default (40) is what ships.
        let _serial = REVIVED_ENV.lock().await;
        std::env::set_var("AMUX_NUDGE_REVIVED_MAX_PATHS", "2");
        let dir = work.to_str().unwrap();
        let fresh = freshness_from_repo(dir, &names).await;
        std::env::remove_var("AMUX_NUDGE_REVIVED_MAX_PATHS");

        // CELL 1 — the cap holds, and the overflow lands in the SAFE bucket.
        assert_eq!(fresh.revived.len(), 2, "the cap must bound the walks: {fresh:?}");
        assert_eq!(fresh.revived_unchecked, 4, "and the rest must be COUNTED: {fresh:?}");
        assert_eq!(
            fresh.edited.len(),
            4,
            "unchecked paths fall back to edited, never to revived — a false revived \
             prescribes a restore against work that may be novel: {fresh:?}"
        );

        // CELL 2 — and the nudge SAYS so. Without this the bound is a lie by
        // omission: 4 paths reported as ordinary edits with nothing marking
        // them as unexamined.
        let n = build(dir, &names, &Default::default(), &fresh, "").expect("nudge");
        assert!(n.contains("NOT CHECKED FOR OLD-REVISION"), "the cap must be visible: {n}");
        assert!(n.contains("2 of 6 candidate path(s) were examined"), "state coverage: {n}");
        // NO PERCENTAGE AT SMALL n. "33%" off 6 candidates adds nothing the two
        // counts do not already say, and printing a rate where a count is the
        // honest quantity is the same error the SHARE has at n=4
        // (mixpeek-frustrations: at 4 observations the estimable values are
        // 0/25/50/75/100 and the true 63% is not in the range at all).
        assert!(
            !n.contains("% coverage"),
            "a coverage rate off 6 candidates is a count wearing a percent sign: {n}"
        );

        // ...AND IT DOES APPEAR once there are enough to be worth feeling as a
        // rate. Driven through `build` directly with a synthetic Freshness,
        // because 4-of-59 is the shape that matters and standing up 59 real
        // paths would test git rather than this. Both cells, because a floor
        // that suppressed the percentage ALWAYS would pass the one above.
        let big = Freshness {
            revived: vec!["a.txt".into()],
            edited: (0..58).map(|i| format!("e{i}.txt")).collect(),
            revived_checked: 4,
            revived_unchecked: 55,
            ..Default::default()
        };
        let mut big_paths = big.revived.clone();
        big_paths.extend(big.edited.clone());
        let bn = build(dir, &big_paths, &Default::default(), &big, "").expect("nudge");
        assert!(bn.contains("4 of 59 candidate path(s) were examined"), "{bn}");
        assert!(bn.contains("(6% coverage)"), "59 candidates is enough to feel as a rate: {bn}");

        // THEIR REAL SHAPE, 4 of 524, which integer division takes to exactly
        // zero. "0% coverage" next to "4 ... were examined" is two fields
        // contradicting each other, and it reads as "nothing was checked".
        let huge = Freshness {
            revived: vec!["a.txt".into()],
            edited: (0..523).map(|i| format!("e{i}.txt")).collect(),
            revived_checked: 4,
            revived_unchecked: 520,
            ..Default::default()
        };
        let mut huge_paths = huge.revived.clone();
        huge_paths.extend(huge.edited.clone());
        let hn = build(dir, &huge_paths, &Default::default(), &huge, "").expect("nudge");
        assert!(hn.contains("4 of 524 candidate path(s) were examined"), "{hn}");
        assert!(hn.contains("(<1% coverage)"), "0.8% must not render as 0%: {hn}");
        assert!(!hn.contains("(0% coverage)"), "{hn}");
        assert!(
            n.contains("NOT a finding that they are clean"),
            "and must refuse the reading that silence means checked: {n}"
        );
        assert!(
            n.contains("is NOT a sample"),
            "and must refuse the OTHER misreading — that the checked proportion generalises \
             to the rest (mixpeek-frustrations measured 4 of 59 under the default budget): {n}"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// Serialises the tests that set AMUX_NUDGE_REVIVED_MAX_PATHS.
    ///
    /// The var is PROCESS-GLOBAL and cargo runs tests in parallel, so without
    /// this one test's `remove_var` lands in the middle of the other's run and
    /// the cap silently becomes the default. That is a flake that reproduces
    /// perhaps one run in ten and reads as a logic bug in the code under test.
    /// `tokio::sync::Mutex`, not `std`: the guard is held across `.await` in
    /// both tests, and a std guard there is a clippy deny and a real deadlock
    /// hazard on a multi-threaded runtime.
    static REVIVED_ENV: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    /// AMUX-3695 probe 2: the budgeted sample is spread across directories, not
    /// taken alphabetically.
    ///
    /// mixpeek-frustrations measured the same dirty set two ways at the same
    /// moment: head-of-`git status` read 35% revived, a random sample read 75%.
    /// The cause is directory mix, not chance — status is alphabetical, so the
    /// first 20 paths there are 11 canvas/apps and 6 .github/workflows while the
    /// population is 342 SDK packages full of regen churn. First-N samples the
    /// wrong end of the repo.
    ///
    /// That matters beyond the sample, because the share threshold decides
    /// between "one checkout-level condition" and "N alarms", and 35% and 75%
    /// fall on opposite sides of it. The same repo, the same second, decided by
    /// path ordering.
    ///
    /// The fixture reproduces the shape: an alphabetically-first directory that
    /// would monopolise a first-N budget, and a later one that would never be
    /// reached.
    #[tokio::test]
    async fn the_budgeted_sample_is_spread_across_directories_not_taken_alphabetically() {
        let tmp = std::env::temp_dir().join(format!("amux-strat-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let (bare, work) = (tmp.join("origin.git"), tmp.join("work"));
        std::fs::create_dir_all(work.join("aaa")).unwrap();
        std::fs::create_dir_all(work.join("zzz")).unwrap();
        let git = |dir: &std::path::Path, args: &[&str]| {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(args)
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        };
        std::process::Command::new("git")
            .args(["init", "--bare", "-b", "main"])
            .arg(&bare)
            .output()
            .unwrap();
        git(&work, &["init", "-b", "main"]);
        git(&work, &["config", "user.email", "t@t"]);
        git(&work, &["config", "user.name", "t"]);
        git(&work, &["remote", "add", "origin", bare.to_str().unwrap()]);

        // Four in `aaa` (alphabetically first, would eat a first-N budget of 2)
        // and four in `zzz` (would never be reached).
        let names: Vec<String> = (0..4)
            .map(|i| format!("aaa/a{i}.txt"))
            .chain((0..4).map(|i| format!("zzz/z{i}.txt")))
            .collect();
        for n in &names {
            std::fs::write(work.join(n), "v1-old\n").unwrap();
        }
        git(&work, &["add", "-A"]);
        git(&work, &["commit", "-m", "v1"]);
        for n in &names {
            std::fs::write(work.join(n), "v2-current\n").unwrap();
        }
        git(&work, &["add", "-A"]);
        git(&work, &["commit", "-m", "v2"]);
        git(&work, &["push", "-q", "origin", "main"]);
        git(&work, &["fetch", "-q", "origin"]);
        for n in &names {
            std::fs::write(work.join(n), "v1-old\n").unwrap();
        }

        let _serial = REVIVED_ENV.lock().await;
        std::env::set_var("AMUX_NUDGE_REVIVED_MAX_PATHS", "2");
        // NEUTRALISE THE CLOCK, because this test is about the CAP and the
        // ORDERING (AMUX-3760). With the default 2000ms budget the two bounds
        // race, and on a loaded machine the clock wins: it failed twice while a
        // release build was compiling, with `revived_checked: 0` and every path
        // in `edited`, then passed 3/3 on a quiet one. A test that is green
        // three runs in four is worse than one that is red, because the green
        // runs are what get pushed.
        //
        // This is not papering over the flake — the underlying cause was that
        // the budget clock started before the classification phase, which is
        // fixed above. Pinning the budget here is what makes the assertion
        // BELOW mean "the cap bound it", which is the property under test. The
        // clock's own behaviour is covered by its own cell.
        std::env::set_var("AMUX_NUDGE_REVIVED_BUDGET_MS", "600000");
        let dir = work.to_str().unwrap();
        let fresh = freshness_from_repo(dir, &names).await;
        // The determinism cell below runs under the SAME cap. Removing the var
        // before it was the first draft's bug: the second run then used the
        // default 40, checked all eight, and "disagreed" with the first for a
        // reason that had nothing to do with ordering.
        let again = freshness_from_repo(dir, &names).await;
        std::env::remove_var("AMUX_NUDGE_REVIVED_MAX_PATHS");
        std::env::remove_var("AMUX_NUDGE_REVIVED_BUDGET_MS");

        assert_eq!(fresh.revived_checked, 2, "the cap still binds: {fresh:?}");
        assert_eq!(
            fresh.revived_stopped_by, "cap",
            "and it must be the CAP that bound it, not the clock — otherwise this test is \
             measuring machine load, which is exactly how it went flaky: {fresh:?}"
        );
        // THE CLAIM: both directories are represented. Under the first-N version
        // this was aaa/a0 and aaa/a1 and `zzz` was invisible.
        let dirs: std::collections::BTreeSet<&str> =
            fresh.revived.iter().filter_map(|p| p.split('/').next()).collect();
        assert_eq!(
            dirs.len(),
            2,
            "a 2-path budget over two directories must take one from each, not two from the \
             alphabetically first: {:?}",
            fresh.revived
        );
        assert!(dirs.contains("zzz"), "the LATER directory must be reachable: {:?}", fresh.revived);

        // DETERMINISM. A shuffle would also spread the sample and would make the
        // nudge report a different verdict on each firing over an unchanged
        // tree, which is worse than a biased one. Round-robin is stable.
        assert_eq!(
            fresh.revived, again.revived,
            "two runs over an unchanged tree must agree, or the nudge contradicts itself"
        );

        // THE CLOCK CELL (AMUX-3760). The failure that made this test flaky was
        // a clock cut at ZERO checked, and it was indistinguishable from a cap
        // of zero or from a run that found nothing to check. Reproduce it on
        // purpose, with a budget that cannot be met, and assert it LABELS
        // itself.
        //
        // This is the honest version of the bug: a zero here is legitimate, and
        // what was missing is the field saying which bound produced it. The
        // same distinction the `revived_unchecked` counter already draws
        // between "we did not look" and "we looked and found little".
        std::env::set_var("AMUX_NUDGE_REVIVED_BUDGET_MS", "0");
        let starved = freshness_from_repo(dir, &names).await;
        std::env::remove_var("AMUX_NUDGE_REVIVED_BUDGET_MS");
        assert_eq!(starved.revived_checked, 0, "a zero budget checks nothing: {starved:?}");
        assert_eq!(
            starved.revived_stopped_by, "clock",
            "and it must say the CLOCK stopped it — a cap hit and a clock hit want opposite \
             actions, and before this they were the same silent zero: {starved:?}"
        );
        assert_eq!(
            starved.revived_unchecked,
            names.len(),
            "every candidate must be COUNTED as unchecked, not quietly reported as an ordinary \
             edit: {starved:?}"
        );
        assert!(
            starved.revived.is_empty() && starved.edited.len() == names.len(),
            "the safe fallback is still `edited`; the counters are what make it honest: {starved:?}"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// AMUX-3695 probe 3: the allocation is PROPORTIONAL to group size, not one
    /// slot per group.
    ///
    /// Fixing the alphabetical bias with one-path-per-directory introduced a
    /// worse one when groups are unequal, and mixpeek-frustrations measured how
    /// unequal they are: 524 dirty files in 41 groups, 17 of them singleton
    /// root-level .md files, and three directories holding 63% of the files.
    /// Equal-per-group gives the singletons 41% of the weight for 3% of the
    /// population, and the three dominant directories 7% for 63% of it. Their
    /// deterministic first four came out as three root .md files and one
    /// workflow, with ZERO of the 332 SDK files — the population the check is
    /// actually about there.
    ///
    /// This is a PURE ordering test on purpose: standing up 200 real git paths
    /// would measure git, and the defect is in the allocation.
    #[test]
    fn the_allocation_is_proportional_to_group_size_not_one_slot_per_group() {
        // Their shape in miniature: one dominant directory and a crowd of
        // singletons that would otherwise monopolise the budget.
        let mut paths: Vec<String> = (0..20).map(|i| format!("big/b{i:02}.txt")).collect();
        paths.extend((0..8).map(|i| format!("s{i}.md")));

        let mut by_dir: std::collections::BTreeMap<&str, Vec<&String>> = Default::default();
        for p in &paths {
            by_dir.entry(p.split('/').next().unwrap_or("")).or_default().push(p);
        }
        let offset = |name: &str| -> f64 {
            let mut h: u64 = 0xcbf2_9ce4_8422_2325;
            for b in name.as_bytes() {
                h ^= *b as u64;
                h = h.wrapping_mul(0x1000_0000_01b3);
            }
            (h >> 11) as f64 / (1u64 << 53) as f64
        };
        let mut keyed: Vec<(f64, &String)> = Vec::new();
        for (name, v) in &by_dir {
            let (n, off) = (v.len() as f64, offset(name));
            for (i, p) in v.iter().enumerate() {
                keyed.push(((i as f64 + off) / n, p));
            }
        }
        keyed.sort_by(|a, b| a.0.total_cmp(&b.0).then_with(|| a.1.cmp(b.1)));
        let order: Vec<&str> = keyed.iter().map(|(_, p)| p.as_str()).collect();

        // `big` is 20 of 28 files (71%). Over the first 10 picks it must get
        // roughly that share. Equal-per-group would give it 5 of 10 (50%) —
        // one per round against 8 singleton groups that exhaust after one each.
        let first10 = &order[..10];
        let big_share = first10.iter().filter(|p| p.starts_with("big/")).count();
        assert!(
            big_share >= 6,
            "the directory holding 71% of the files must take most of the early budget, not \
             one slot: got {big_share} of 10 — {first10:?}"
        );
        // ...and the singletons must not be shut out either, or this would have
        // swapped one bias for its mirror image.
        assert!(
            first10.iter().any(|p| p.ends_with(".md")),
            "proportional is not winner-take-all: {first10:?}"
        );
        // DETERMINISM, again: the tie-break on path is what makes equal keys
        // (a singleton at 0.5 against an odd group's midpoint) stable.
        let mut keyed2 = keyed.clone();
        keyed2.sort_by(|a, b| a.0.total_cmp(&b.0).then_with(|| a.1.cmp(b.1)));
        let order2: Vec<&str> = keyed2.iter().map(|(_, p)| p.as_str()).collect();
        assert_eq!(order, order2, "the ordering must be stable across runs");
    }

    /// A GRAFT-PUSH CHECKOUT MUST NOT READ AS DIVERGED (reported by
    /// mixpeek-frustrations, 2026-08-24).
    ///
    /// `git log origin/main..HEAD -- <path>` counts commits BY SHA, and a commit
    /// already upstream under a different sha — cherry-picked, rebased, replayed
    /// by a graft push — sits in that range permanently. On such a checkout every
    /// path reads local-ahead, so DIVERGED fired for paths that were merely
    /// STALE, and the safe restore was withheld from the one file class that
    /// most needs it: the append-only ledgers.
    ///
    /// Both cells run against the SAME repo so the control is not a different
    /// world. `replayed.md` is the specimen (local commits exist, worktree
    /// contributes no line origin lacks -> STALE); `truly-diverged.md` holds one
    /// line origin has never seen -> DIVERGED stands. Without the second, a
    /// downgrade that fired unconditionally would pass the first — the
    /// matches-everything filter that looks identical to a correct one from the
    /// rows alone.
    #[tokio::test]
    async fn a_replayed_commit_downgrades_diverged_to_stale_but_real_divergence_stands() {
        let tmp = std::env::temp_dir().join(format!("amux-graft-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let (bare, work, peer) = (tmp.join("origin.git"), tmp.join("work"), tmp.join("peer"));
        std::fs::create_dir_all(&work).unwrap();
        let git = |dir: &std::path::Path, args: &[&str]| {
            let out =
                std::process::Command::new("git").arg("-C").arg(dir).args(args).output().unwrap();
            assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        };
        std::process::Command::new("git")
            .args(["init", "--bare", "-b", "main"])
            .arg(&bare)
            .output()
            .unwrap();
        git(&work, &["init", "-b", "main"]);
        git(&work, &["config", "user.email", "t@t"]);
        git(&work, &["config", "user.name", "t"]);
        git(&work, &["remote", "add", "origin", bare.to_str().unwrap()]);
        std::fs::write(work.join("replayed.md"), "entry A\n").unwrap();
        std::fs::write(work.join("truly-diverged.md"), "entry A\n").unwrap();
        // BLOB TWIN + local deletion (mixpeek-frustrations' second report).
        std::fs::write(work.join("twin.md"), "entry A\n").unwrap();
        // STRICT SUPERSET (gtm-media-assets' report, 2026-08-26): the worktree
        // ends up holding everything origin has PLUS local lines, while both
        // ancestry arms print commits.
        std::fs::write(work.join("superset.md"), "entry A\n").unwrap();
        git(&work, &["add", "-A"]);
        git(&work, &["commit", "-m", "base"]);
        git(&work, &["push", "-q", "origin", "main"]);

        // LOCAL commits entry B on both paths. These commits never reach origin
        // under this sha — the graft-push shape.
        std::fs::write(work.join("replayed.md"), "entry A\nentry B\n").unwrap();
        std::fs::write(work.join("truly-diverged.md"), "entry A\nentry B\n").unwrap();
        std::fs::write(work.join("twin.md"), "entry A\nentry B\n").unwrap();
        std::fs::write(work.join("superset.md"), "entry A\nentry LOCAL\n").unwrap();
        git(&work, &["add", "-A"]);
        git(&work, &["commit", "-m", "local: entry B"]);

        // A peer lands entry B AND entry C from the base — so origin's copy is a
        // SUPERSET of the local content, reached by different commits.
        std::process::Command::new("git")
            .args(["clone", "-q", bare.to_str().unwrap()])
            .arg(&peer)
            .output()
            .unwrap();
        git(&peer, &["config", "user.email", "t@t"]);
        git(&peer, &["config", "user.name", "t"]);
        std::fs::write(peer.join("replayed.md"), "entry A\nentry B\nentry C\n").unwrap();
        std::fs::write(peer.join("truly-diverged.md"), "entry A\nentry B\nentry C\n").unwrap();
        // The peer lands the SAME BYTES local already committed, under a
        // different sha: the graft twin. HEAD:twin.md == origin/main:twin.md.
        std::fs::write(peer.join("twin.md"), "entry A\nentry B\n").unwrap();
        // Origin moves on superset.md with a line the LOCAL commit never had.
        std::fs::write(peer.join("superset.md"), "entry A\nentry PEER\n").unwrap();
        git(&peer, &["add", "-A"]);
        git(&peer, &["commit", "-m", "peer: entries B and C"]);
        git(&peer, &["push", "-q", "origin", "main"]);
        git(&work, &["fetch", "-q", "origin"]);

        // Worktree: the specimen carries only content origin already has. The
        // control carries one line origin has never seen.
        std::fs::write(work.join("replayed.md"), "entry A\nentry B\n").unwrap();
        std::fs::write(
            work.join("truly-diverged.md"),
            "entry A\nentry B\nentry LOCAL-ONLY\n",
        )
        .unwrap();
        // The superset specimen: everything origin has, plus a local line. This
        // is the shape `git diff --numstat origin/main` reports as "N  0".
        std::fs::write(work.join("superset.md"), "entry A\nentry PEER\nentry LOCAL\n").unwrap();

        let dir = work.to_str().unwrap();

        // PREMISE CHECK, not decoration: both paths must read local-ahead BY SHA,
        // or this test proves nothing about the downgrade — it would just be
        // exercising the ordinary STALE path. "I built the specimen" is a claim,
        // not a premise.
        for p in ["replayed.md", "truly-diverged.md", "superset.md"] {
            let ahead = std::process::Command::new("git")
                .args(["-C", dir, "log", "--oneline", "origin/main..HEAD", "--", p])
                .output()
                .unwrap();
            assert!(
                !String::from_utf8_lossy(&ahead.stdout).trim().is_empty(),
                "{p} is not local-ahead by sha, so the DIVERGED arm is never reached and this \
                 test is vacuous"
            );
        }

        let fresh =
            freshness_from_repo(dir, &s(&["replayed.md", "truly-diverged.md", "superset.md"]))
                .await;

        // A STRICT SUPERSET OF ORIGIN IS NOT DIVERGED (gtm-media-assets, 2026-08-26).
        // Both ancestry arms print commits — the premise loop above proves it —
        // and the worktree still holds every line origin has. Committing reverts
        // nothing, so telling the owner that both remedies destroy landed work
        // sends them to a hand-merge that can only lose work.
        //
        // NOT STALE EITHER, and that is the trap in this cell: STALE prescribes
        // `git checkout origin/main -- <path>`, which would delete `entry LOCAL`.
        // A downgrade to the nearer-looking class would still destroy the work.
        assert_eq!(
            fresh.edited,
            s(&["superset.md"]),
            "a worktree containing every line origin has, plus local ones, is an ordinary EDIT: \
             {fresh:?}"
        );

        // The two assertions below are also the control for the cell above: they
        // are exact-equality, so a downgrade that fired unconditionally would
        // empty them and fail here rather than passing quietly.
        assert_eq!(
            fresh.stale,
            s(&["replayed.md"]),
            "a path whose local commits contribute no line origin lacks is STALE, not DIVERGED — \
             the restore is safe and withholding it is the reported bug"
        );
        assert_eq!(
            fresh.diverged,
            s(&["truly-diverged.md"]),
            "a path holding a line origin has never seen must STAY DIVERGED — a downgrade that \
             fires unconditionally passes the specimen above and destroys real work"
        );

        // A LOCALLY DELETED FILE WHOSE REFS AGREE IS NOT DIVERGED
        // (mixpeek-frustrations' second report, 2026-08-24 — their
        // discriminator, on their specimen's shape).
        //
        // Their live case: research/extractors/HYPERSPECTRAL-RASTER-EXTRACTOR-GAP.md,
        // blob 1e435f5c9fba at HEAD, at origin/main and at BOTH graft twins,
        // and NOT ON DISK. Step 2's `hash-object -- <path>` cannot read a
        // deleted file, so the worktree-identical check cannot run and the path
        // falls through to the ancestry arms — which on this checkout both
        // print commits. Verdict: "novel and stale at once, NEITHER standard
        // remedy is safe", on a path where every blob matches.
        //
        // IT LIVES IN THIS TEST, NOT THE FOUR-CLASS ONE, and that was not the
        // first draft. I wrote these cells against a fixture where origin had
        // NOT moved on the path, so the row never reached the ancestry arms at
        // all — mutating the gate to `if false` left them GREEN. The mutation
        // caught it; reading the test did not. A deleted-file cell only
        // discriminates where both arms genuinely fire.
        std::fs::remove_file(work.join("twin.md")).unwrap();
        let twin_ahead = std::process::Command::new("git")
            .args(["-C", dir, "log", "--oneline", "origin/main..HEAD", "--", "twin.md"])
            .output()
            .unwrap();
        assert!(
            !String::from_utf8_lossy(&twin_ahead.stdout).trim().is_empty(),
            "premise: twin.md must be local-ahead by sha, or the DIVERGED arm is never reached \
             and this cell is vacuous — which is exactly how its first draft failed"
        );
        let del = freshness_from_repo(dir, &s(&["twin.md"])).await;
        assert!(
            del.diverged.is_empty(),
            "identical blobs at HEAD and origin/main cannot be diverged whatever the two \
             ancestries say — there is nothing to merge and nothing at risk: {del:?}"
        );
        assert!(
            del.stale.is_empty(),
            "and not STALE either: STALE prescribes a RESTORE, and whether a local deletion is \
             deliberate is the OWNER's call, not something a nudge decides for them: {del:?}"
        );
        assert_eq!(
            del.edited,
            s(&["twin.md"]),
            "it is an ordinary worktree change against agreeing refs: {del:?}"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// SET SEMANTICS HID TWO REAL LOSSES (gtm-media-assets, 2026-08-26,
    /// reviewing 90eaa6dc). Both downgrade arms asked "is any line of X absent
    /// from Y" with a `HashSet` of trimmed lines, which cannot see:
    ///
    ///   MULTIPLICITY  dropping ONE of a repeated line. Origin `x = 1 / } / }`
    ///                 against a worktree holding a single `}` scored zero, so
    ///                 the nudge said committing reverts nothing while a closing
    ///                 brace was being deleted.
    ///   INDENTATION   `str::trim` makes leading whitespace invisible, and in
    ///                 Python or YAML an indent change IS a semantic change.
    ///
    /// They found it by reading the code and reproducing it against their own
    /// reimplementation, and flagged it as a hypothesis until a real harness
    /// agreed. This is that harness: these cells run the shipped functions
    /// through `freshness_from_repo` against a constructed repo.
    ///
    /// THE LAST CELL IS THE CONTROL and it is the reason this test cannot be
    /// satisfied by making the counters pessimistic. A genuine superset must
    /// still downgrade; a fix that simply counted more would fail there.
    ///
    /// PRECONDITION ASSERTED, not assumed: every path must read local-ahead by
    /// sha, or the DIVERGED arm is never reached and the whole test degenerates
    /// into an ordinary-STALE case that passes for the wrong reason.
    #[tokio::test]
    async fn a_dropped_duplicate_and_a_reindent_are_losses_the_set_could_not_see() {
        let tmp = std::env::temp_dir().join(format!("amux-multiset-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let (bare, work, peer) = (tmp.join("origin.git"), tmp.join("work"), tmp.join("peer"));
        std::fs::create_dir_all(&work).unwrap();
        let git = |dir: &std::path::Path, args: &[&str]| {
            let out =
                std::process::Command::new("git").arg("-C").arg(dir).args(args).output().unwrap();
            assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        };
        std::process::Command::new("git")
            .args(["init", "--bare", "-b", "main"])
            .arg(&bare)
            .output()
            .unwrap();
        git(&work, &["init", "-b", "main"]);
        git(&work, &["config", "user.email", "t@t"]);
        git(&work, &["config", "user.name", "t"]);
        git(&work, &["remote", "add", "origin", bare.to_str().unwrap()]);

        // BASE, pushed. braces.md carries a REPEATED line; indent.py carries a
        // block whose meaning is its leading whitespace.
        std::fs::write(work.join("braces.md"), "x = 1\n}\n}\n").unwrap();
        std::fs::write(work.join("indent.py"), "def f():\n    if a:\n        g()\n").unwrap();
        std::fs::write(work.join("superset-ctl.md"), "entry A\n").unwrap();
        git(&work, &["add", "-A"]);
        git(&work, &["commit", "-m", "base"]);
        git(&work, &["push", "-q", "origin", "main"]);

        // LOCAL commits that never reach origin under this sha — the graft shape
        // that makes every path read local-ahead.
        std::fs::write(work.join("braces.md"), "x = 1\n}\n}\nLOCAL\n").unwrap();
        std::fs::write(work.join("indent.py"), "def f():\n    if a:\n        g()\nLOCAL\n")
            .unwrap();
        std::fs::write(work.join("superset-ctl.md"), "entry A\nentry LOCAL\n").unwrap();
        git(&work, &["add", "-A"]);
        git(&work, &["commit", "-m", "local"]);

        // A peer moves origin ahead of HEAD on all three paths.
        std::process::Command::new("git")
            .args(["clone", "-q", bare.to_str().unwrap()])
            .arg(&peer)
            .output()
            .unwrap();
        git(&peer, &["config", "user.email", "t@t"]);
        git(&peer, &["config", "user.name", "t"]);
        std::fs::write(peer.join("braces.md"), "x = 1\n}\n}\nPEER\n").unwrap();
        std::fs::write(peer.join("indent.py"), "def f():\n    if a:\n        g()\nPEER\n").unwrap();
        std::fs::write(peer.join("superset-ctl.md"), "entry A\nentry PEER\n").unwrap();
        git(&peer, &["add", "-A"]);
        git(&peer, &["commit", "-m", "peer"]);
        git(&peer, &["push", "-q", "origin", "main"]);
        git(&work, &["fetch", "-q", "origin"]);

        // WORKTREES. Each holds novel lines (so arm 3c cannot downgrade it and
        // the mirror question is actually asked), and each loses something of
        // origin's that set-of-trimmed-lines cannot see.
        std::fs::write(work.join("braces.md"), "x = 1\n}\nPEER\nLOCAL\nNOVEL\n").unwrap(); // one } gone
        std::fs::write(
            work.join("indent.py"),
            "def f():\n    if a:\n    g()\nPEER\nLOCAL\nNOVEL\n", // g() dedented 8 -> 4
        )
        .unwrap();
        // CONTROL: loses nothing of origin's.
        std::fs::write(work.join("superset-ctl.md"), "entry A\nentry PEER\nentry LOCAL\n").unwrap();

        let dir = work.to_str().unwrap();
        let names = s(&["braces.md", "indent.py", "superset-ctl.md"]);
        for p in &names {
            let ahead = std::process::Command::new("git")
                .args(["-C", dir, "log", "--oneline", "origin/main..HEAD", "--", p])
                .output()
                .unwrap();
            assert!(
                !String::from_utf8_lossy(&ahead.stdout).trim().is_empty(),
                "premise: {p} must be local-ahead by sha, or the DIVERGED arm is never reached \
                 and this cell is vacuous"
            );
        }

        let fresh = freshness_from_repo(dir, &names).await;

        assert_eq!(
            fresh.diverged,
            s(&["braces.md", "indent.py"]),
            "dropping one of a repeated line, and re-indenting a block, are both losses of \
             origin content — committing reverts them, so DIVERGED must stand: {fresh:?}"
        );
        assert_eq!(
            fresh.edited,
            s(&["superset-ctl.md"]),
            "and the control must still downgrade, or the fix is just a pessimistic counter: \
             {fresh:?}"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// The shared counter's own contract, at the unit level, so a future edit
    /// cannot satisfy the fixture test by coincidence. Order-insensitivity is
    /// the property the set was chosen for and must survive.
    #[test]
    fn missing_line_instances_counts_instances_and_respects_indentation() {
        assert_eq!(missing_line_instances("a\nb\n", "b\na\n"), 0, "a MOVED line is not a loss");
        assert_eq!(missing_line_instances("}\n", "}\n}\n"), 1, "one of two braces is missing");
        assert_eq!(missing_line_instances("}\n}\n", "}\n"), 0, "a spare copy is not a loss");
        assert_eq!(
            missing_line_instances("    g()\n", "        g()\n"),
            1,
            "a re-indent is a real change wherever whitespace carries meaning"
        );
        assert_eq!(
            missing_line_instances("a\n\n   \nb\n", "a\nb\n"),
            0,
            "blank and whitespace-only lines are not content"
        );
        // THE TWO ENDS OF A LINE ARE NOT THE SAME KIND OF THING (AMUX-3786).
        // These two cells must hold TOGETHER: dropping the second would let a
        // full `trim` back in and re-open the re-indent bug, and dropping the
        // first would hold DIVERGED open every time an editor strips trailing
        // whitespace on save.
        assert_eq!(
            missing_line_instances("g()\n", "g()   \n"),
            0,
            "TRAILING whitespace is dropped ON PURPOSE, not because it is meaningless — a \
             Markdown hard break is two trailing spaces. Losing it costs a rendered line break; \
             keeping it holds DIVERGED open on every strip-on-save. See the trade in \
             missing_line_instances"
        );
        assert_eq!(
            missing_line_instances("g()\n", "    g()\n"),
            1,
            "LEADING whitespace is content wherever indentation carries meaning"
        );
    }

    /// The restore-safety discriminator must be pinned in EVERY arm that
    /// prescribes a restore, not only the STALE renderer.
    ///
    /// `stale_section_prescribes_find_object_never_blob_existence` pins ONE
    /// copy of the recipe. The advice is emitted from at least three places
    /// (`build`, `append_only_note`, `commit_worthy_body`), and on 2026-08-28 a
    /// mutation reverting `commit_worthy_body`'s copy to
    /// `git cat-file -e $(git hash-object <path>)` left all 40 tests green,
    /// while the commit-worthy and DIVERGED arms went to ZERO guards and went
    /// on prescribing `git checkout origin/main -- <path>` 3 and 4 times each.
    /// That is DESKT-10's own defect one arm over: a check pinning the wrong
    /// layer is exactly as green as one pinning the right layer (ethos rule 7,
    /// AF-161). The recipe it would restore to deletes never-committed
    /// mid-edits, which is the cold-outbound near-miss of 2026-08-17.
    ///
    /// Asserted per arm rather than over the concatenation, because a guard in
    /// the STALE section does not reach a reader in the commit-worthy one. The
    /// restore prescription itself is the CONTROL: an arm that stopped
    /// prescribing a restore would make the guard assertion vacuous, so it
    /// fails there instead of passing quietly.
    #[test]
    fn every_arm_that_prescribes_a_restore_carries_the_find_object_guard() {
        let dirty = s(&["FRUSTRATIONS.md", "a.rs"]);
        // TWO AXES, because the advice is branched on BOTH and a matrix over
        // one of them leaves whole message bodies unreachable. The first
        // version of this test varied freshness only; `commit_worthy_body`
        // emits an entirely separate recipe under `mine.is_empty()` (the
        // OWNERSHIP IS UNKNOWN arm), so a mutation planted there rendered in no
        // cell and the suite stayed green (amux, 2026-08-28).
        let freshness: [(&str, Freshness); 4] = [
            ("commit-worthy", Freshness::default()),
            ("diverged", Freshness { diverged: s(&["a.rs"]), ..Default::default() }),
            ("stale", Freshness { stale: s(&["a.rs"]), ..Default::default() }),
            ("revived", Freshness { revived: s(&["a.rs"]), ..Default::default() }),
        ];
        let ownership: [(&str, Ownership); 2] = [
            ("mine", Ownership::default()),
            ("unknown-owner", Ownership { unclaimed: s(&["a.rs", "FRUSTRATIONS.md"]), ..Default::default() }),
        ];
        for ((fname, fresh), (oname, own)) in freshness
            .iter()
            .flat_map(|f| ownership.iter().map(move |o| (f, o)))
        {
            let arm = format!("{fname}/{oname}");
            let arm = arm.as_str();
            let m = build("/repo", &dirty, own, fresh, "S")
                .unwrap_or_else(|| panic!("{arm}: must render"));
            assert!(
                m.contains("git checkout origin/main -- <path>"),
                "{arm}: premise gone — this arm no longer prescribes a restore, so the guard \
                 assertion below would pass vacuously: {m}"
            );
            assert!(
                m.contains("--find-object=$(git hash-object <path>)"),
                "{arm}: prescribes `git checkout origin/main -- <path>` with NO find-object \
                 restore-safety check in the same message. Blob existence answers yes for a \
                 `git add`ed mid-edit that is in no commit, so the restore it green-lights is an \
                 irreversible delete: {m}"
            );
            // NOT asserted universally. The unknown-ownership arm prescribes the
            // correct find-object check and never names the wrong recipe at all,
            // so requiring the warning here would fail on code that is right.
            // Whether that arm SHOULD carry the warning is amux's call on their
            // own message text; reported, not decided here (ethos rule 8). The
            // STALE section's warning is pinned by
            // `stale_section_prescribes_find_object_never_blob_existence`, so
            // dropping it from this loop loses no coverage.
            // EVERY occurrence, not merely one somewhere in the arm (amux,
            // 2026-08-28). The `contains` checks above pin "this arm mentions
            // the guard at least once". The advice is emitted from SIX
            // production strings, so mutating one back to blob-existence left
            // a sibling occurrence satisfying them and 41 tests stayed green.
            //
            // Counting guards against restore prescriptions does NOT work and
            // was measured before being rejected: the restore command also
            // appears in prose and in do-not warnings, so today's CORRECT code
            // reads 3 prescriptions to 1 guard (commit-worthy), 4 to 1
            // (diverged), 5 to 2 (stale), 4 to 2 (revived). That invariant is
            // false on code that is right.
            //
            // This is the rule the section actually follows, and it holds with
            // zero slack in all four arms: blob existence may appear ONLY
            // inside a do-not warning. An occurrence outside one is a
            // prescription, and a correct guard sitting paragraphs away does
            // not reach a reader acting on the line in front of them, which is
            // this test's own rationale applied within an arm instead of
            // across arms.
            // Matched by PROXIMITY, not by one exact phrasing. The first
            // version keyed on "Do NOT substitute `git cat-file -e" and the
            // unknown-ownership arm says "Do NOT use" — correct text, flagged
            // as a live prescription. A matcher pinned to one wording reports
            // the arms that phrase it differently, which is the same
            // wrong-layer error this test exists to catch, pointed at prose.
            let blob_total = m.matches("cat-file -e").count();
            let blob_warned = m
                .match_indices("cat-file -e")
                .filter(|(i, _)| m[i.saturating_sub(40)..*i].contains("Do NOT"))
                .count();
            assert_eq!(
                blob_total, blob_warned,
                "{arm}: {} of {} blob-existence mention(s) are NOT inside a do-not warning, so \
                 at least one is prescribed as a check. `git add` writes the blob without \
                 committing, so that recipe green-lights an irreversible delete of a mid-edit \
                 that is in no commit: {m}",
                blob_total - blob_warned, blob_total
            );
        }
    }



}
