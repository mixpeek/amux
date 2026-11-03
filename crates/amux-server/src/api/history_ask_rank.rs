//! Which messages an Ask sends the model (2026-11-03).
//!
//! Ask used to send the newest messages that fit, about 300 of the ~11,700 in
//! a 14-day window, so it answered about the last day and a half whatever the
//! question was. Ethan asked "did we have a message around using the hiking
//! video to validate e2e using mixpeek local image to iterate on a ns
//! manifest?" and got "I don't see a message": MSG-103447 (backend, 11-02,
//! "iterating entirely using the mixpeek image ... ns manifest") was in the
//! window and never reached the model.
//!
//! Now every message in the window is scored against the question by the words
//! they share, a rare word counting for more than a common one (IDF), and the
//! best matches take up to half the prompt; the newest messages fill the rest.
//! No model call: ranking is arithmetic, the model's call is the answer
//! (ethos rule 2). That message ranks 8th of 11,690 this way.
use std::collections::{HashMap, HashSet};

use super::history_ask::Row;

/// Matched messages kept at most, and the share of the prompt they may take.
pub(crate) const RELEVANT_MAX: usize = 150;
const RELEVANT_SHARE: usize = 2;
/// Characters of a message that are scored, so one pasted transcript cannot
/// match every question.
const SCORED_CHARS: usize = 4000;

const STOP: &[&str] = &[
    "a", "an", "the", "and", "or", "of", "to", "in", "on", "for", "with", "we", "did", "do", "does", "have",
    "has", "had", "is", "are", "was", "were", "be", "been", "it", "its", "this", "that", "these", "those",
    "there", "their", "them", "they", "i", "you", "u", "me", "my", "our", "ur", "around", "about", "using",
    "use", "used", "via", "any", "some", "what", "which", "who", "how", "when", "where", "why", "can",
    "could", "should", "would", "will", "just", "message", "messages", "msg", "ask", "asked", "said",
    "say", "tell", "told", "anything", "something", "if", "not", "no", "from", "at", "by", "as", "so",
];

/// Lowercase alphanumeric words with a light suffix strip, so "iterating",
/// "iterate" and "iterates" meet.
pub(crate) fn terms(text: &str) -> Vec<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(stem)
        .collect()
}

fn stem(w: &str) -> String {
    let mut s = w;
    for suf in ["ing", "ions", "ion", "ed", "es", "s"] {
        if s.len() > 4 && s.ends_with(suf) {
            s = &s[..s.len() - suf.len()];
            break;
        }
    }
    if s.len() > 4 && s.ends_with('e') {
        s = &s[..s.len() - 1];
    }
    s.to_string()
}

/// The question's distinct content words.
pub(crate) fn query_terms(question: &str) -> Vec<String> {
    let stop: HashSet<String> = STOP.iter().map(|w| stem(w)).collect();
    let mut out: Vec<String> = Vec::new();
    for t in terms(question) {
        if !stop.contains(&t) && !out.contains(&t) {
            out.push(t);
        }
    }
    out
}

/// Score every row against the question: the sum of the IDF of each question
/// word it contains. A row must share at least two words with the question
/// (or its only one, for a one-word question) to count as a match.
/// Returns (row index, score), best first.
pub(crate) fn rank(question: &str, rows: &[Row]) -> Vec<(usize, f64)> {
    let q = query_terms(question);
    if q.is_empty() || rows.is_empty() {
        return Vec::new();
    }
    let docs: Vec<HashSet<String>> = rows
        .iter()
        .map(|r| {
            let text: String = r.text.chars().take(SCORED_CHARS).collect();
            terms(&text).into_iter().filter(|t| q.contains(t)).collect()
        })
        .collect();
    let n = docs.len() as f64;
    let mut df: HashMap<&str, usize> = HashMap::new();
    for d in &docs {
        for t in d {
            *df.entry(t.as_str()).or_default() += 1;
        }
    }
    let idf = |t: &str| (1.0 + n / (1.0 + *df.get(t).unwrap_or(&0) as f64)).ln();
    let need = q.len().min(2);
    let mut scored: Vec<(usize, f64)> = docs
        .iter()
        .enumerate()
        .filter(|(_, d)| d.len() >= need)
        .map(|(i, d)| (i, d.iter().map(|t| idf(t)).sum::<f64>()))
        .collect();
    scored.sort_by(|a, b| b.1.total_cmp(&a.1).then(rows[b.0].ts.cmp(&rows[a.0].ts)));
    scored
}

/// Pick the rows to send: the best matches first, up to RELEVANT_MAX and half
/// of `budget` characters (as `line_cost` measures a row), then the newest
/// rows until the budget is spent or `recent_max` are taken. Returns the
/// chosen rows OLDEST FIRST, the order the corpus is rendered in, and the ids
/// chosen as matches, best first.
///
/// A message that contains the question word for word is the asker's own
/// question (pasted into a worker, or asked before), never its answer: it
/// would always rank first, so it is left out of the matches.
pub(crate) fn select(
    question: &str,
    rows: &[Row],
    budget: usize,
    recent_max: usize,
    line_cost: impl Fn(&Row) -> usize,
) -> (Vec<Row>, Vec<i64>) {
    let mut chosen: HashSet<usize> = HashSet::new();
    let mut used = 0;
    let mut matches: Vec<i64> = Vec::new();
    let asked = question.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase();
    let is_the_question = |r: &Row| {
        asked.len() >= 20 && r.text.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase().contains(&asked)
    };
    for (i, _) in rank(question, rows).into_iter().filter(|(i, _)| !is_the_question(&rows[*i])).take(RELEVANT_MAX) {
        let cost = line_cost(&rows[i]);
        if used + cost > budget / RELEVANT_SHARE {
            continue;
        }
        used += cost;
        chosen.insert(i);
        matches.push(rows[i].id);
    }
    // Newest first by timestamp, whatever order the caller loaded them in.
    let mut by_new: Vec<usize> = (0..rows.len()).collect();
    by_new.sort_by(|a, b| rows[*b].ts.cmp(&rows[*a].ts).then(rows[*b].id.cmp(&rows[*a].id)));
    let mut recent = 0;
    for i in by_new {
        if recent >= recent_max {
            break;
        }
        if chosen.contains(&i) {
            continue;
        }
        let cost = line_cost(&rows[i]);
        if used + cost > budget {
            break;
        }
        used += cost;
        chosen.insert(i);
        recent += 1;
    }
    let mut out: Vec<Row> = chosen.into_iter().map(|i| rows[i].clone()).collect();
    out.sort_by(|a, b| a.ts.cmp(&b.ts).then(a.id.cmp(&b.id)));
    (out, matches)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: i64, ts: i64, text: &str) -> Row {
        Row { id, ts, session: "w".into(), kind: "human".into(), text: text.into(), context: String::new() }
    }

    /// The 2026-11-03 miss: an older message that answers the question, under
    /// a pile of newer traffic that fills the budget on its own.
    #[test]
    fn an_old_message_that_answers_the_question_reaches_the_model_past_newer_traffic() {
        let mut rows = vec![row(
            103_447,
            1_000,
            "make sure youre doing this entirely iterating entirely using the mixpeek image. quality of models from within the image / ns manifest rapidly iterating",
        )];
        for i in 0..500 {
            rows.push(row(200_000 + i, 10_000 + i, &format!("status update {i}: the mixpeek build is green, image pushed")));
        }
        let q = "did we have a message around using the hiking video to validate e2e using mixpeek local image to iterate on a ns manifest?";
        rows.push(row(104_094, 20_000, &format!("i know i asked this and cant find it: \"{q}\"")));
        let (picked, matches) = select(q, &rows, 2_000, 400, |r| r.text.len() + 1);
        assert!(picked.iter().any(|r| r.id == 103_447), "the answering message is sent");
        assert_eq!(matches.first(), Some(&103_447), "and named first: {matches:?}");
        assert!(!matches.contains(&104_094), "the asker's own question is not a match");
        assert!(picked.windows(2).all(|w| w[0].ts <= w[1].ts), "oldest first");
        assert!(picked.iter().any(|r| r.id == 200_499), "the newest still fill the rest");
    }

    #[test]
    fn the_recent_fill_keeps_the_newest_whatever_order_the_rows_came_in() {
        // Newest first, the order load_rows returns.
        let rows: Vec<Row> = (0..50).rev().map(|i| row(i, i * 10, &format!("note {i}"))).collect();
        let (picked, matches) = select("zebra quartz", &rows, 100, 400, |_| 10);
        assert!(matches.is_empty());
        let ids: Vec<i64> = picked.iter().map(|r| r.id).collect();
        assert_eq!(ids, (40..50).collect::<Vec<_>>(), "the ten newest, oldest first");
    }

    #[test]
    fn a_rare_shared_word_outranks_a_common_one() {
        let mut rows: Vec<Row> = (0..20).map(|i| row(i, i, "mixpeek image build")).collect();
        rows.push(row(99, 0, "the ns manifest for the mixpeek image"));
        let ranked = rank("mixpeek ns manifest", &rows);
        assert_eq!(ranked.first().map(|(i, _)| rows[*i].id), Some(99));
    }

    #[test]
    fn suffixes_meet_and_filler_words_do_not_count() {
        assert_eq!(terms("iterating iterate iterates"), vec!["iterat", "iterat", "iterat"]);
        assert_eq!(query_terms("did we have a message about the manifest?"), vec!["manifest"]);
    }
}
