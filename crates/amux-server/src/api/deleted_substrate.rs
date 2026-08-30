//! `GET /api/board/deleted-substrate` — closed cards whose files no longer exist.
//!
//! AMUX-3608, spun out of AMUX-3606, which found ONE card (BACKE-3183) asserting
//! a shipped, reviewed fix whose entire substrate was deleted at 792ce1f. The
//! question this answers is whether that was a one-off or a class.
//!
//! # The method the parent card proposed is wrong, and it was run before it was
//! checked
//!
//! AMUX-3606 suggested sweeping cards whose `desc` cites the deleted filename.
//! Measured: 151 cards cite it, 127 done/verified, 73 closed "before" the
//! deletion. Then the positive control ran, and BOTH halves of that predicate
//! missed the one instance known to be positive:
//!
//! - BACKE-3183's `desc` does not contain the string at all. Its evidence lives
//!   in the `log` column, against a 10,178-character desc with zero hits. A
//!   `desc LIKE` cannot see it.
//! - Its `updated` is 03:18 that morning because two sessions wrote to it that
//!   night. `updated` is LAST TOUCH, not close time, so filtering closed cards
//!   by it measures who typed most recently.
//!
//! 73 was a confident wrong number of exactly the shape ethos rule 7 warns
//! about: a filter that feels like a measurement because you ran a command.
//!
//! # The method that works
//!
//! A closed card's `log` names the COMMITS that closed it (``05:03`` `commit
//! 63d64c1 — ...`). Resolve each sha, list its paths, check whether they still
//! exist. No text heuristics; nothing depends on anyone having typed a filename.
//!
//! Controlled on BACKE-3183, where it discriminates AND splits the card in half,
//! which a per-card grain would have lost:
//!
//! ```text
//! 63d64c1  DELETED  amux-server.py, tests/test_upstream_dirt.py
//! 9df0195  DELETED  amux-server.py
//! c2d57ed  ALIVE    amux                  (the `amux board reviewer` verb)
//! 41ed4bf  ALIVE    frustrations.md
//! ```
//!
//! The verdict is PER COMMIT. "Half of this card survived" is the common case
//! and the useful one.
//!
//! # It SURFACES, it never closes, and it is not a reopen trigger
//!
//! A deleted path is not proof the behaviour is gone: it may have been ported
//! under a new name, and most of these were ported deliberately and correctly.
//! That judgment belongs to a human or the owning session (ethos rule 8). So the
//! honest headline is "N cards cite work whose files no longer exist", NEVER
//! "N regressions", and the endpoint mutates nothing. Same discipline as its
//! sibling `commit_mentions`.
//!
//! # The trap that would have made this report zero
//!
//! `git diff-tree --stdin` requires FULL 40-character shas. Given an abbreviated
//! one it prints the sha line and NO paths, silently — so every card would have
//! come back with an empty path list and the check would have reported a clean
//! board. Measured here before it shipped, not reasoned about. `git cat-file
//! --batch-check` does the expansion for every sha in ONE process and marks the
//! ones this repo does not have as `missing`, which is also how a sha belonging
//! to a different repo is told apart from a sha that resolves to nothing.

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};

use super::AppState;

/// Cap on shas resolved per repo, REPORTED rather than silently applied. A
/// silent truncation reads as "covered everything" when it did not (AF-131).
const MAX_SHAS: usize = 4000;

#[derive(Deserialize, Default)]
pub struct Params {
    /// Restrict to one session's cards.
    pub session: Option<String>,
    /// Only report cards with at least one DELETED commit. Default true: the
    /// full join is mostly cards that are entirely fine, and a report whose
    /// headline count includes them is one nobody finishes reading.
    pub only_deleted: Option<bool>,
}

/// Commit shas a card's `log` names, in order, deduped.
///
/// ANCHORED ON THE WORD `commit`, not on "looks like hex". The log is prose
/// written by many sessions and a bare 7-40 hex matcher would take card ids,
/// timestamps, and any word that happens to be `deadbeef` — a filter that
/// matches too much returns a confident wrong answer rather than silence.
///
/// The anchor is the format the board itself writes: ``05:03`` `commit 63d64c1 —
/// subject`. `committed`, `commits` and `commit-mentions` do not match, because
/// a space must follow.
pub(crate) fn shas_in_log(log: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let bytes = log.as_bytes();
    let mut from = 0usize;
    while let Some(rel) = log[from..].find("commit ") {
        let start = from + rel;
        // A word boundary before, so `precommit ` / `xcommit ` do not match.
        let before_ok = start == 0 || !bytes[start - 1].is_ascii_alphanumeric();
        let mut i = start + "commit ".len();
        let hex_start = i;
        while i < bytes.len() && bytes[i].is_ascii_hexdigit() {
            i += 1;
        }
        let len = i - hex_start;
        // The char AFTER the run must not be alphanumeric, or `commit deadbeefz`
        // yields a sha that is really a truncated word.
        let after_ok = i >= bytes.len() || !bytes[i].is_ascii_alphanumeric();
        if before_ok && after_ok && (7..=40).contains(&len) {
            let sha = log[hex_start..i].to_ascii_lowercase();
            if seen.insert(sha.clone()) {
                out.push(sha);
            }
        }
        from = start + "commit ".len();
    }
    out
}

/// Closed cards and the shas their logs name.
fn closed_cards(
    conn: &rusqlite::Connection,
    session: Option<&str>,
) -> rusqlite::Result<BTreeMap<String, (Value, Vec<String>)>> {
    // CLOSED is the predicate, and it is `status`, never `updated`. The parent
    // card's `updated` filter is the specific wrong turn documented at the top
    // of this file: `updated` is last touch, so it measures who typed most
    // recently rather than when the card closed.
    let sql = "SELECT id, status, COALESCE(session,''), COALESCE(title,''), COALESCE(log,'') \
               FROM issues \
               WHERE status IN ('done','verified') AND deleted IS NULL \
               AND COALESCE(archived,0)=0 \
               AND (?1 IS NULL OR session = ?1)";
    let mut st = conn.prepare(sql)?;
    let rows = st.query_map(rusqlite::params![session], |r| {
        let log: String = r.get(4)?;
        Ok((
            r.get::<_, String>(0)?,
            (
                json!({
                    "status":  r.get::<_, String>(1)?,
                    "session": r.get::<_, String>(2)?,
                    "title":   r.get::<_, String>(3)?,
                }),
                shas_in_log(&log),
            ),
        ))
    })?;
    Ok(rows.flatten().filter(|(_, (_, shas))| !shas.is_empty()).collect())
}

async fn git_toplevel(dir: &str) -> Option<String> {
    let out = tokio::process::Command::new("git")
        .args(["-C", dir, "rev-parse", "--show-toplevel"])
        .output()
        .await
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Feed `stdin` to a git subcommand and return stdout. One process for N shas.
async fn git_stdin(repo: &str, args: &[&str], stdin: String) -> Option<String> {
    use tokio::io::AsyncWriteExt;
    let mut child = tokio::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    if let Some(mut si) = child.stdin.take() {
        let _ = si.write_all(stdin.as_bytes()).await;
        let _ = si.shutdown().await;
    }
    let out = child.wait_with_output().await.ok()?;
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// abbreviated sha -> full sha, for the ones THIS repo has.
///
/// A sha absent here is not an error: cards come from many repos and a sha this
/// checkout never saw belongs to somebody else's. That is reported as
/// `unresolved` rather than folded into "no paths changed", which would read as
/// "nothing was deleted".
async fn resolve_shas(repo: &str, shas: &BTreeSet<String>) -> BTreeMap<String, String> {
    let list: Vec<&String> = shas.iter().take(MAX_SHAS).collect();
    let stdin: String =
        list.iter().map(|s| format!("{s}\n")).collect::<Vec<_>>().concat();
    let Some(out) = git_stdin(repo, &["cat-file", "--batch-check=%(objectname) %(objecttype)"], stdin).await
    else {
        return BTreeMap::new();
    };
    let mut map = BTreeMap::new();
    for (line, abbrev) in out.lines().zip(list.iter()) {
        let mut it = line.split_whitespace();
        let (Some(full), Some(kind)) = (it.next(), it.next()) else { continue };
        if kind == "commit" && full.len() == 40 {
            map.insert((*abbrev).clone(), full.to_string());
        }
    }
    map
}

/// full sha -> the paths that commit touched. One process for N commits.
async fn paths_of(repo: &str, fulls: &BTreeSet<String>) -> BTreeMap<String, Vec<String>> {
    let stdin: String = fulls.iter().map(|s| format!("{s}\n")).collect::<Vec<_>>().concat();
    // NO `--no-commit-id`: with `--stdin` the sha line is what attributes the
    // following paths to a commit, so suppressing it would merge every commit's
    // paths into one anonymous list.
    let Some(out) = git_stdin(repo, &["diff-tree", "--stdin", "-r", "--name-only"], stdin).await
    else {
        return BTreeMap::new();
    };
    let mut map: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut cur: Option<String> = None;
    for line in out.lines() {
        let line = line.trim_end();
        if line.is_empty() {
            continue;
        }
        if line.len() == 40 && line.bytes().all(|b| b.is_ascii_hexdigit()) {
            cur = Some(line.to_string());
            map.entry(line.to_string()).or_default();
        } else if let Some(c) = cur.as_ref() {
            map.entry(c.clone()).or_default().push(line.to_string());
        }
    }
    map
}

/// Every path that exists at HEAD. One `ls-tree`, then set membership, instead
/// of a `cat-file -e` per path.
async fn live_paths(repo: &str) -> BTreeSet<String> {
    let out = tokio::process::Command::new("git")
        .args(["-C", repo, "ls-tree", "-r", "HEAD", "--name-only"])
        .output()
        .await;
    let Ok(out) = out else { return BTreeSet::new() };
    String::from_utf8_lossy(&out.stdout).lines().map(str::to_string).collect()
}

pub async fn deleted_substrate(
    State(state): State<AppState>,
    Query(p): Query<Params>,
) -> (StatusCode, Json<Value>) {
    let (code, body) = payload(&state, p).await;
    (code, Json(body))
}

async fn payload(state: &AppState, p: Params) -> (StatusCode, Value) {
    let only_deleted = p.only_deleted.unwrap_or(true);
    let cards = {
        let conn = match state.store.read() {
            Ok(c) => c,
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    json!({ "error": format!("store unreadable: {e}") }),
                )
            }
        };
        match closed_cards(&conn, p.session.as_deref()) {
            Ok(m) => m,
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    json!({ "error": format!("board unreadable: {e}") }),
                )
            }
        }
    };
    if cards.is_empty() {
        return (
            StatusCode::OK,
            json!({
                "cards": [], "closed_cards_scanned": 0, "repos": [],
                "verdict": "no closed card names a commit",
            }),
        );
    }

    let dirs: BTreeSet<String> = {
        let conn = match state.store.read() {
            Ok(c) => c,
            Err(_) => {
                return (StatusCode::INTERNAL_SERVER_ERROR, json!({ "error": "store unreadable" }))
            }
        };
        match super::sessions_legacy::build_array(&conn) {
            Ok(arr) => arr
                .iter()
                .filter_map(|v| {
                    let name = v["name"].as_str()?;
                    let dir = v["dir"].as_str().unwrap_or("");
                    cards
                        .values()
                        .any(|(c, _)| c["session"].as_str() == Some(name))
                        .then(|| dir.to_string())
                })
                .filter(|d| !d.is_empty())
                .collect(),
            Err(_) => BTreeSet::new(),
        }
    };
    let mut repos: BTreeSet<String> = BTreeSet::new();
    for d in &dirs {
        if let Some(top) = git_toplevel(d).await {
            repos.insert(top);
        }
    }

    let all_shas: BTreeSet<String> =
        cards.values().flat_map(|(_, s)| s.iter().cloned()).collect();
    let truncated = all_shas.len() > MAX_SHAS;

    // sha (as written in the log) -> (repo, verdict, deleted paths, live paths)
    let mut verdict: BTreeMap<String, Value> = BTreeMap::new();
    for repo in &repos {
        let resolved = resolve_shas(repo, &all_shas).await;
        if resolved.is_empty() {
            continue;
        }
        let fulls: BTreeSet<String> = resolved.values().cloned().collect();
        let paths = paths_of(repo, &fulls).await;
        let live = live_paths(repo).await;
        for (abbrev, full) in &resolved {
            // First repo that HAS the sha wins; a sha resolving in two repos is
            // not a case this fleet produces (shas are content-addressed).
            if verdict.contains_key(abbrev) {
                continue;
            }
            let touched = paths.get(full).cloned().unwrap_or_default();
            let (gone, alive): (Vec<String>, Vec<String>) =
                touched.into_iter().partition(|p| !live.contains(p));
            verdict.insert(
                abbrev.clone(),
                json!({
                    "sha": abbrev, "repo": repo,
                    "verdict": if gone.is_empty() { "ALIVE" } else { "DELETED" },
                    "deleted_paths": gone, "live_paths": alive,
                }),
            );
        }
    }

    let mut out: Vec<Value> = Vec::new();
    let mut cards_with_deletions = 0usize;
    for (id, (meta, shas)) in &cards {
        let commits: Vec<Value> = shas
            .iter()
            .map(|s| {
                verdict.get(s).cloned().unwrap_or_else(|| {
                    json!({
                        "sha": s, "verdict": "UNRESOLVED",
                        // Absence is not evidence: a sha no repo here has is one
                        // from another checkout, NOT one whose files survived.
                        "why": "no scanned repo has this commit — it belongs to a checkout this \
                                server cannot see, so nothing is known about its paths",
                    })
                })
            })
            .collect();
        let any_deleted = commits.iter().any(|c| c["verdict"] == "DELETED");
        if any_deleted {
            cards_with_deletions += 1;
        }
        if only_deleted && !any_deleted {
            continue;
        }
        out.push(json!({
            "id": id, "status": meta["status"], "session": meta["session"],
            "title": meta["title"], "commits": commits,
        }));
    }
    // Deterministic: most deleted commits first, then id, so two runs of an
    // unchanged board produce the same bytes and a reader can diff them.
    out.sort_by(|a, b| {
        let n = |v: &Value| {
            v["commits"].as_array().map_or(0, |c| {
                c.iter().filter(|x| x["verdict"] == "DELETED").count()
            })
        };
        n(b).cmp(&n(a)).then_with(|| {
            a["id"].as_str().unwrap_or("").cmp(b["id"].as_str().unwrap_or(""))
        })
    });

    // WHICH PATHS, RANKED. Without this the report is 89 rows that each say
    // "some file is gone" and a reader skims it; with it, the FIRST line
    // answers the question. Measured on the live board the day this shipped: 219
    // of the deletions are `amux-server.py` and all 42 distinct paths are that
    // file or its Python tests, which is one deliberate port (792ce1f) rather
    // than 89 independent losses.
    //
    // Ranked rather than filtered, and no path is special-cased. Hardcoding the
    // known deletion would be a tuned parameter for one event and would hide the
    // NEXT one; a ranking shows a new class standing out against the old without
    // anyone maintaining a list.
    let mut path_freq: BTreeMap<&str, usize> = BTreeMap::new();
    for v in verdict.values() {
        for p in v["deleted_paths"].as_array().into_iter().flatten() {
            if let Some(p) = p.as_str() {
                *path_freq.entry(p).or_default() += 1;
            }
        }
    }
    let mut ranked: Vec<(&str, usize)> = path_freq.into_iter().collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
    let top: Vec<Value> =
        ranked.iter().take(20).map(|(p, n)| json!({ "path": p, "commits": n })).collect();
    let dominant = ranked.first().map(|(p, n)| (p.to_string(), *n));

    (
        StatusCode::OK,
        json!({
            "cards": out,
            "closed_cards_scanned": cards.len(),
            "cards_with_deleted_paths": cards_with_deletions,
            "shas_seen": all_shas.len(),
            "repos": repos.iter().cloned().collect::<Vec<_>>(),
            "truncated": truncated,
            "only_deleted": only_deleted,
            "deleted_paths_ranked": top,
            "distinct_deleted_paths": ranked.len(),
            "read_this_first": match &dominant {
                Some((p, n)) => format!(
                    "{n} of the deleted-path hits are `{p}`, out of {} distinct paths. If one \
                     path dominates, this is probably ONE deliberate deletion rather than N \
                     independent losses — check that event before reading the card list.",
                    ranked.len()
                ),
                None => "no deleted paths in this scan".to_string(),
            },
            // THE HEADLINE IS A CITATION COUNT, NOT A REGRESSION COUNT. Most of
            // these were ported deliberately and correctly; a deleted path may
            // have been reimplemented under a new name. Calling them regressions
            // would turn a list to judge into a reopen queue.
            "verdict": format!(
                "{cards_with_deletions} of {} closed cards cite at least one commit whose files \
                 no longer exist. That is a list to JUDGE, not a regression count: a deleted \
                 path may have been ported under a new name, and most were. Nothing here is \
                 reopened or closed.",
                cards.len()
            ),
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The parser, on the board's REAL log format and on the things that must
    /// not match.
    ///
    /// The positive is BACKE-3183's own line. The negatives are what a bare
    /// "looks like hex" matcher would have swallowed: a card id, a word that
    /// happens to be hex, and `committed`/`precommit` — a filter that matches
    /// too much returns a confident wrong answer rather than silence.
    #[test]
    fn shas_come_from_the_word_commit_not_from_looking_hexish() {
        let log = "`04:55` Auto-picked up from queue\n\
                   `05:03` commit 63d64c1 — fix(gates): clear verify-gate dirt (BACKE-3183)\n\
                   `05:20` commit 9df0195 — second half\n\
                   `06:00` committed the rest by hand\n\
                   `06:10` precommit ffffff1 ran\n\
                   `06:20` see AMUX-3608 and deadbeef for context\n\
                   `06:30` commit 63d64c1 — same one again";
        let got = shas_in_log(log);
        assert_eq!(got, vec!["63d64c1", "9df0195"], "{got:?}");
    }

    /// The boundary cells, each one a way the run-length check can be wrong.
    #[test]
    fn sha_runs_are_bounded_at_both_ends() {
        // Too short to be a sha (git's own minimum unique length is 7 here).
        assert!(shas_in_log("commit abc123 — nope").is_empty());
        // Exactly 7 is the shortest real one.
        assert_eq!(shas_in_log("commit abc1234 — yes"), vec!["abc1234"]);
        // 40 is a full sha; 41 hex chars is not a sha and must not be truncated
        // into one, which is the failure that would silently mis-resolve.
        let full = "a".repeat(40);
        assert_eq!(shas_in_log(&format!("commit {full} — yes")), vec![full.clone()]);
        assert!(shas_in_log(&format!("commit {}1 — no", "a".repeat(40))).is_empty());
        // Hex run followed by a letter is a word, not a sha.
        assert!(shas_in_log("commit deadbeefz — no").is_empty());
        // Case-folded, so the same commit written two ways is one sha.
        assert_eq!(shas_in_log("commit ABC1234 and commit abc1234"), vec!["abc1234"]);
    }

    /// `git diff-tree --stdin` output is attributed by the sha LINE, and this
    /// pins the parse of that. Written because the alternative shape —
    /// `--no-commit-id` — merges every commit's paths into one anonymous list,
    /// and the resulting report would attribute deletions to the wrong card.
    #[test]
    fn diff_tree_output_attributes_paths_to_their_commit() {
        let a = "a".repeat(40);
        let b = "b".repeat(40);
        let out = format!("{a}\nsrc/one.rs\nsrc/two.rs\n{b}\nREADME.md\n");
        // Re-implements the parse loop in `paths_of` over a fixture, because the
        // real one needs a git process.
        let mut map: BTreeMap<String, Vec<String>> = BTreeMap::new();
        let mut cur: Option<String> = None;
        for line in out.lines() {
            if line.len() == 40 && line.bytes().all(|c| c.is_ascii_hexdigit()) {
                cur = Some(line.to_string());
                map.entry(line.to_string()).or_default();
            } else if let Some(c) = cur.as_ref() {
                map.entry(c.clone()).or_default().push(line.to_string());
            }
        }
        assert_eq!(map[&a], vec!["src/one.rs", "src/two.rs"]);
        assert_eq!(map[&b], vec!["README.md"]);
    }
}
