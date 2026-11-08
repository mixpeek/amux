//! Refusals, the timeline, veto and crash recovery for the decision digest.
//! Every test runs [`tick_with`] over a real store; only the channels and the
//! worker are mocks. Crash cases SIGKILL a real child process mid-application.

use super::*;
use chrono::TimeZone;
use std::sync::{Arc, Mutex};

const SAFE_Q: &str = "Should the sidebar group sort newest first?";
const SAFE_REC: &str = "Sort the sidebar group newest first";

struct Mail(Arc<Mutex<Vec<(String, String, String)>>>);
#[async_trait::async_trait]
impl AlertChannels for Mail {
    async fn push(&self, _: &AppState, _: &str, _: &str) -> Result<(), String> {
        Ok(())
    }
    async fn sms(&self, _: &str, _: &str) -> (bool, String) {
        panic!("the digest never pages SMS")
    }
    async fn email(&self, to: &str, subject: &str, body: &str) -> (bool, String) {
        self.0.lock().unwrap().push((to.into(), subject.into(), body.into()));
        (true, "mock".into())
    }
}

/// Worker mock. Sends land in a file, so they outlive a SIGKILL.
struct Acts {
    file: std::path::PathBuf,
    isolated: Vec<String>,
    /// Die here: claimed, sent or carded.
    kill_at: Option<String>,
    send_error: Option<String>,
}
fn die_if(acts: &Acts, at: &str) {
    if acts.kill_at.as_deref() == Some(at) {
        unsafe { libc::kill(libc::getpid(), libc::SIGKILL) };
    }
}
impl Acts {
    fn new(home: &Path) -> Self {
        Acts { file: home.join("sends.log"), isolated: vec![], kill_at: None, send_error: None }
    }
    fn sends(&self) -> Vec<String> {
        std::fs::read_to_string(&self.file).unwrap_or_default().lines().map(str::to_string).collect()
    }
}
#[async_trait::async_trait]
impl Actions for Acts {
    fn isolated(&self, worker: &str) -> bool {
        self.isolated.iter().any(|w| w == worker)
    }
    async fn already_sent(&self, _: &AppState, id: &str) -> Result<bool, String> {
        Ok(self.sends().iter().any(|l| l.starts_with(id)))
    }
    async fn send(&self, _: &AppState, _: &str, text: &str, id: &str) -> Result<String, String> {
        die_if(self, "claimed");
        if let Some(e) = &self.send_error {
            return Err(e.clone());
        }
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&self.file).unwrap();
        writeln!(f, "{id} {text}").unwrap();
        f.sync_all().unwrap();
        die_if(self, "sent");
        Ok("mock sent".into())
    }
    async fn card(&self, state: &AppState, card: &str, _: &str, _: &str) -> Result<String, String> {
        let id = card.to_string();
        state
            .store
            .write_async(move |c| {
                c.execute("UPDATE issues SET status='todo' WHERE id=?1", [&id])?;
                Ok(no_events(true))
            })
            .await
            .map_err(|e| e.to_string())?;
        die_if(self, "carded");
        Ok("moved".into())
    }
}

fn state_at(home: &Path) -> AppState {
    AppState {
        store: Arc::new(crate::db::Store::open(&home.join("t.db")).unwrap()),
        started: std::time::Instant::now(),
        build_hash: "t".into(),
        auth_token: None,
        reconciled: Arc::new(std::sync::atomic::AtomicBool::new(true)),
    }
}

/// `(to, subject, body)` of every email the digest sent.
type Sent = Arc<Mutex<Vec<(String, String, String)>>>;

fn setup(apply: bool) -> (tempfile::TempDir, AppState, Sent) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("server.env"), "AMUX_OWNER_EMAIL=owner@example.com\n").unwrap();
    // Default on: an unset key is the apply case.
    if !apply {
        std::fs::write(dir.path().join("amux.env"), format!("{APPLY_KEY}=0\n")).unwrap();
    }
    let state = state_at(dir.path());
    (dir, state, Arc::new(Mutex::new(vec![])))
}

/// One fixture write on the store's writer thread.
fn exec(state: &AppState, sql: &'static str, args: &[&str]) {
    let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
    state
        .store
        .write(move |c| {
            c.execute(sql, rusqlite::params_from_iter(args.iter()))?;
            Ok(no_events(true))
        })
        .unwrap();
}

/// `(id, ask_type, source, question, desc)`.
fn seed(state: &AppState, id: &str, ask_type: &str, source: &str, question: &str, desc: &str) {
    exec(
        state,
        "INSERT INTO issues (id,title,desc,status,session,created,updated,type,archived,ask_type,ask_question,ask_unblocks,ask_actor,entered_state_at,source)
         VALUES (?1,?1,?2,'needsyou','lane-a',1000,1000,'chore',0,?3,?4,'the worker continues','Ethan',1000,?5)",
        &[id, desc, ask_type, question, source],
    );
}

fn recommend(state: &AppState, id: &str, rec: &str, scope: &str) {
    exec(
        state,
        "INSERT INTO decision_digest_recommendations(card,recommendation,scope) VALUES(?1,?2,?3)
         ON CONFLICT(card) DO UPDATE SET recommendation=?2,scope=?3",
        &[id, rec, scope],
    );
}

fn safe_card(state: &AppState, id: &str) {
    seed(state, id, "decision", "agent", SAFE_Q, "sidebar order");
    recommend(state, id, SAFE_REC, SAFE_SCOPE);
}

fn at(day: u32, hour: u32) -> chrono::DateTime<chrono::Local> {
    chrono::Local.with_ymd_and_hms(2026, 11, day, hour, 5, 0).unwrap()
}

fn ledger(state: &AppState, id: &str) -> Row {
    load_rows(&state.store.read().unwrap()).unwrap().remove(id).unwrap_or_default()
}

fn status(state: &AppState, id: &str) -> String {
    state.store.read().unwrap().query_row("SELECT status FROM issues WHERE id=?1", [id], |r| r.get(0)).unwrap()
}

/// Two delivered digests and the morning of the effect date: everything a
/// card needs, so what stops it next is the thing under test.
async fn publish_twice(m: &Mail, a: &Acts, s: &AppState, home: &Path) {
    assert!(tick_with(m, a, s, home, at(2, 10)).await.digest_sent);
    assert!(tick_with(m, a, s, home, at(3, 10)).await.digest_sent);
}

#[tokio::test]
async fn safe_card_applies_only_after_two_digests_and_the_grace_hours() {
    let (dir, s, sent) = setup(true);
    let home = dir.path();
    let (m, a) = (Mail(sent.clone()), Acts::new(home));
    safe_card(&s, "OK-1");
    // Day one, past the apply hour: published, and far too early to fire.
    assert!(tick_with(&m, &a, &s, home, at(2, 20)).await.applied.is_empty());
    assert_eq!(ledger(&s, "OK-1").effect_date, "2026-11-04");
    // Day two: the one full digest in between.
    assert!(tick_with(&m, &a, &s, home, at(3, 20)).await.applied.is_empty());
    // Effect day, digest out, inside the grace hours.
    assert!(tick_with(&m, &a, &s, home, at(4, 10)).await.applied.is_empty());
    assert!(a.sends().is_empty());
    assert!(tick_with(&m, &a, &s, home, at(4, 17)).await.applied.is_empty());
    let r = tick_with(&m, &a, &s, home, at(4, 18)).await;
    assert_eq!(r.applied, ["OK-1"]);
    let sends = a.sends();
    assert_eq!(sends.len(), 1);
    assert!(sends[0].contains("not the owner") && sends[0].contains(CLOSING) && sends[0].contains(SAFE_REC), "{}", sends[0]);
    let row = ledger(&s, "OK-1");
    assert_eq!(row.state, "applied");
    assert!(row.receipt.contains("decision-digest-"), "{row:?}");
    let mails = sent.lock().unwrap();
    assert_eq!(mails.len(), 3);
    assert!(mails.iter().all(|x| x.0 == "owner@example.com"), "owner only");
    assert!(mails[0].2.contains(SAFE_REC) && mails[0].2.contains("(2026-11-04)") && mails[0].2.contains("unless vetoed"), "{}", mails[0].2);
    assert!(mails.iter().all(|x| !x.2.contains('\u{2014}')), "no em-dashes");
}

#[tokio::test]
async fn dry_run_shows_the_date_and_sends_nothing_to_a_worker() {
    let (dir, s, sent) = setup(false);
    let home = dir.path();
    let (m, a) = (Mail(sent.clone()), Acts::new(home));
    safe_card(&s, "DRY-1");
    seed(&s, "NOREC-1", "decision", "agent", SAFE_Q, "x");
    publish_twice(&m, &a, &s, home).await;
    tick_with(&m, &a, &s, home, at(4, 10)).await;
    let r = tick_with(&m, &a, &s, home, at(4, 20)).await;
    assert!(r.applied.is_empty());
    assert_eq!(r.would_apply, ["DRY-1"]);
    assert!(a.sends().is_empty(), "dry run reaches no worker: {:?}", a.sends());
    assert_eq!(status(&s, "DRY-1"), "needsyou");
    let mails = sent.lock().unwrap();
    assert!(mails[0].2.contains("Would take effect"), "{}", mails[0].2);
    assert!(mails[0].2.contains("Needs a recommendation"), "{}", mails[0].2);
}

/// The grace window runs from when the owner got it, whatever the hour.
#[tokio::test]
async fn a_late_digest_still_gets_the_full_grace_window() {
    let (dir, s, sent) = setup(true);
    let home = dir.path();
    let (m, a) = (Mail(sent), Acts::new(home));
    safe_card(&s, "LATE-1");
    publish_twice(&m, &a, &s, home).await;
    assert!(tick_with(&m, &a, &s, home, at(4, 14)).await.digest_sent, "sent late, as after email retries");
    assert!(tick_with(&m, &a, &s, home, at(4, 21)).await.applied.is_empty());
    assert_eq!(tick_with(&m, &a, &s, home, at(4, 22)).await.applied, ["LATE-1"]);
}

#[test]
fn a_digest_hour_past_the_window_is_capped() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("server.env"), "AMUX_DECISION_DIGEST_HOUR=23\n").unwrap();
    assert_eq!(digest_hour(dir.path()), 15);
}

#[tokio::test]
async fn a_dry_run_date_promises_nothing_when_apply_turns_on() {
    let (dir, s, sent) = setup(false);
    let home = dir.path();
    let (m, a) = (Mail(sent), Acts::new(home));
    safe_card(&s, "FLIP-1");
    publish_twice(&m, &a, &s, home).await;
    tick_with(&m, &a, &s, home, at(4, 10)).await;
    std::fs::remove_file(home.join("amux.env")).unwrap();
    assert!(tick_with(&m, &a, &s, home, at(4, 20)).await.applied.is_empty());
    tick_with(&m, &a, &s, home, at(5, 20)).await;
    assert_eq!(ledger(&s, "FLIP-1").effect_date, "2026-11-07", "the clock restarted live");
    assert!(a.sends().is_empty());
}

/// Each refusal, on a card otherwise identical to one that applies.
#[tokio::test]
async fn refusals_never_apply_and_say_why() {
    let (dir, s, sent) = setup(true);
    let home = dir.path();
    let (m, mut a) = (Mail(sent.clone()), Acts::new(home));
    seed(&s, "MONEY-1", "budget", "agent", SAFE_Q, "sidebar");
    recommend(&s, "MONEY-1", SAFE_REC, SAFE_SCOPE);
    seed(&s, "MONEY-2", "decision", "agent", "Should we take the larger instance at $400 per month?", "x");
    recommend(&s, "MONEY-2", SAFE_REC, SAFE_SCOPE);
    seed(&s, "CUST-1", "customer_outbound", "agent", SAFE_Q, "sidebar");
    recommend(&s, "CUST-1", SAFE_REC, SAFE_SCOPE);
    seed(&s, "CUST-2", "decision", "agent", SAFE_Q, "this rewrites customer data");
    recommend(&s, "CUST-2", SAFE_REC, SAFE_SCOPE);
    seed(&s, "CUST-3", "decision", "agent", SAFE_Q, "sidebar");
    recommend(&s, "CUST-3", "Email the TubeScience customer the new order", SAFE_SCOPE);
    seed(&s, "UNCL-1", "", "agent", SAFE_Q, "sidebar");
    recommend(&s, "UNCL-1", SAFE_REC, SAFE_SCOPE);
    // Clean text and no declaration: absence of a keyword proves nothing.
    seed(&s, "UNCL-2", "decision", "agent", SAFE_Q, "sidebar");
    recommend(&s, "UNCL-2", SAFE_REC, "");
    seed(&s, "HARN-1", "decision", "land_approval", SAFE_Q, "sidebar");
    recommend(&s, "HARN-1", SAFE_REC, SAFE_SCOPE);
    seed(&s, "NOREC-1", "decision", "agent", SAFE_Q, "sidebar");
    seed(&s, "ISO-1", "decision", "agent", SAFE_Q, "sidebar");
    recommend(&s, "ISO-1", SAFE_REC, SAFE_SCOPE);
    exec(&s, "UPDATE issues SET session='lane-iso' WHERE id='ISO-1'", &[]);
    a.isolated = vec!["lane-iso".into()];
    for (d, h) in [(2, 20), (3, 20), (4, 20), (5, 20), (6, 20)] {
        let r = tick_with(&m, &a, &s, home, at(d, h)).await;
        assert!(r.digest_sent, "a refusal is proven only by a digest that was recorded");
        assert!(r.applied.is_empty());
    }
    assert!(a.sends().iter().all(|l| l.contains("no recommendation")), "only the ask reaches a worker: {:?}", a.sends());
    let c = s.store.read().unwrap();
    let n: i64 = c.query_row("SELECT count(*) FROM decision_digest_ledger", [], |r| r.get(0)).unwrap();
    assert_eq!(n, 0, "none of these was ever given a date");
    drop(c);
    let mails = sent.lock().unwrap();
    let body = &mails[0].2;
    for line in ["MONEY-1", "MONEY-2", "CUST-1", "CUST-2", "CUST-3", "UNCL-1", "UNCL-2", "HARN-1", "ISO-1"] {
        let at = body.find(&format!("- {line} ")).unwrap_or_else(|| panic!("{line} missing: {body}"));
        assert!(body[at..].contains("Never auto-applies:"), "{line}: {body}");
    }
    assert!(body.contains("unclassified: its worker has not declared"), "{body}");
    assert!(body.contains("unclassified: the card has no typed ask"), "{body}");
    assert!(body.contains("unclassified: filed by a harness job"), "{body}");
    assert!(mails[0].1.contains("0 taking effect"), "{}", mails[0].1);
}

#[tokio::test]
async fn veto_is_final_attributed_and_outlives_a_new_clock() {
    let (dir, s, sent) = setup(true);
    let home = dir.path();
    let (m, a) = (Mail(sent.clone()), Acts::new(home));
    safe_card(&s, "VETO-1");
    publish_twice(&m, &a, &s, home).await;
    veto(&s, "VETO-1", "ethan", "keep it").await.unwrap();
    for (d, h) in [(4, 20), (5, 20), (6, 20)] {
        assert!(tick_with(&m, &a, &s, home, at(d, h)).await.applied.is_empty());
    }
    let row = ledger(&s, "VETO-1");
    assert_eq!((row.state.as_str(), row.actor.as_str(), row.reason.as_str()), ("vetoed", "ethan", "keep it"));
    assert!(a.sends().is_empty());
    assert_eq!(status(&s, "VETO-1"), "needsyou");
    // A worker re-filing the ask does not lift it.
    recommend(&s, "VETO-1", "Sort the sidebar group oldest first", SAFE_SCOPE);
    tick_with(&m, &a, &s, home, at(7, 20)).await;
    assert_eq!(ledger(&s, "VETO-1").state, "vetoed");
    assert!(sent.lock().unwrap().last().unwrap().2.contains("Vetoed by ethan"));
}

#[tokio::test]
async fn an_owner_note_stops_the_card_for_good() {
    let (dir, s, sent) = setup(true);
    let home = dir.path();
    let (m, a) = (Mail(sent), Acts::new(home));
    safe_card(&s, "NOTE-1");
    publish_twice(&m, &a, &s, home).await;
    let line = bs::append_log(ledger_log(&s, "NOTE-1").as_deref(), "11:00", "api-anonymous: desc +20 chars");
    exec(&s, "UPDATE issues SET log=?1 WHERE id='NOTE-1'", &[&line]);
    assert!(tick_with(&m, &a, &s, home, at(4, 20)).await.applied.is_empty());
    let row = ledger(&s, "NOTE-1");
    assert_eq!(row.state, "stopped");
    assert!(row.reason.contains("api-anonymous"), "{row:?}");
    assert!(a.sends().is_empty());
}

fn ledger_log(s: &AppState, id: &str) -> Option<String> {
    s.store.read().unwrap().query_row("SELECT log FROM issues WHERE id=?1", [id], |r| r.get(0)).unwrap()
}

#[tokio::test]
async fn a_card_changed_after_the_digest_starts_its_clock_over() {
    let (dir, s, sent) = setup(true);
    let home = dir.path();
    let (m, a) = (Mail(sent), Acts::new(home));
    safe_card(&s, "CHG-1");
    publish_twice(&m, &a, &s, home).await;
    recommend(&s, "CHG-1", "Sort the sidebar group by name", SAFE_SCOPE);
    assert!(tick_with(&m, &a, &s, home, at(4, 20)).await.applied.is_empty());
    assert!(a.sends().is_empty(), "the owner never read this recommendation");
    assert_eq!(ledger(&s, "CHG-1").effect_date, "2026-11-06");
    assert_eq!(ledger(&s, "CHG-1").digests, 1);
    // The worker's own log line and hygiene's are not owner activity.
    let log = bs::append_log(None, "11:00", "lane-a: ask_question");
    let log = bs::append_log(Some(&log), "11:01", "Auto-aged: this card has been in needsyou for 20 days");
    assert_eq!(outside_activity(&log, 0, &sha(""), "lane-a"), None);
}

#[tokio::test]
async fn a_status_change_stops_it_and_a_return_is_a_new_ask() {
    let (dir, s, sent) = setup(true);
    let home = dir.path();
    let (m, a) = (Mail(sent), Acts::new(home));
    safe_card(&s, "ST-1");
    publish_twice(&m, &a, &s, home).await;
    exec(&s, "UPDATE issues SET status='todo' WHERE id='ST-1'", &[]);
    tick_with(&m, &a, &s, home, at(4, 20)).await;
    assert_eq!(ledger(&s, "ST-1").state, "reset");
    exec(&s, "UPDATE issues SET status='needsyou',entered_state_at=2000 WHERE id='ST-1'", &[]);
    assert!(tick_with(&m, &a, &s, home, at(5, 20)).await.applied.is_empty());
    assert_eq!(ledger(&s, "ST-1").effect_date, "2026-11-07");
    assert!(a.sends().is_empty());
}

#[tokio::test]
async fn an_undelivered_digest_publishes_nothing() {
    struct NoMail;
    #[async_trait::async_trait]
    impl AlertChannels for NoMail {
        async fn push(&self, _: &AppState, _: &str, _: &str) -> Result<(), String> {
            Ok(())
        }
        async fn sms(&self, _: &str, _: &str) -> (bool, String) {
            (false, String::new())
        }
        async fn email(&self, _: &str, _: &str, _: &str) -> (bool, String) {
            (false, "down".into())
        }
    }
    let (dir, s, _) = setup(true);
    let home = dir.path();
    let a = Acts::new(home);
    safe_card(&s, "OFFLINE-1");
    let r = tick_with(&NoMail, &a, &s, home, at(2, 20)).await;
    assert!(!r.digest_sent);
    assert_eq!(ledger(&s, "OFFLINE-1"), Row::default(), "no date was published");
}

#[tokio::test]
async fn an_early_date_fires_on_a_tick_after_that_days_digest() {
    let (dir, s, sent) = setup(true);
    let home = dir.path();
    let (m, a) = (Mail(sent), Acts::new(home));
    safe_card(&s, "GAP-1");
    publish_twice(&m, &a, &s, home).await;
    let mut r = ledger(&s, "GAP-1");
    r.effect_date = "2026-11-03".into();
    s.store
        .write(move |c| {
            upsert(c, &r)?;
            Ok(no_events(true))
        })
        .unwrap();
    // No digest due: the second digest already went out on this date.
    assert_eq!(tick_with(&m, &a, &s, home, at(3, 20)).await.applied, ["GAP-1"]);
    assert_eq!(a.sends().len(), 1);
}

#[tokio::test]
async fn a_paused_worker_is_held_and_a_refused_send_gets_no_receipt() {
    let (dir, s, sent) = setup(true);
    let home = dir.path();
    let (m, mut a) = (Mail(sent), Acts::new(home));
    safe_card(&s, "P-1");
    publish_twice(&m, &a, &s, home).await;
    tick_with(&m, &a, &s, home, at(4, 10)).await;
    a.send_error = Some("send to lane-a: target is paused".into());
    assert!(tick_with(&m, &a, &s, home, at(4, 18)).await.applied.is_empty());
    assert_eq!(ledger(&s, "P-1").state, "tracked");
    a.send_error = Some("send to lane-a: no such worker".into());
    for h in 19..23 {
        assert!(tick_with(&m, &a, &s, home, at(4, h)).await.applied.is_empty());
    }
    let row = ledger(&s, "P-1");
    assert_eq!(row.state, "failed", "{row:?}");
    assert!(row.receipt.is_empty(), "no application without a receipt");
    assert_eq!(status(&s, "P-1"), "needsyou");
}

/// `(kill point, sends expected after restart)`. The child dies with SIGKILL
/// at each boundary; the parent is the restarted server on the same DB.
#[tokio::test]
async fn sigkill_mid_application_never_duplicates_and_always_ends_with_a_receipt() {
    for point in ["claimed", "sent", "carded"] {
        let (dir, s, sent) = setup(true);
        let home = dir.path();
        let (m, a) = (Mail(sent), Acts::new(home));
        safe_card(&s, "K-1");
        publish_twice(&m, &a, &s, home).await;
        tick_with(&m, &a, &s, home, at(4, 10)).await;
        // One process owns the writer: this one lets go, the child dies
        // holding it, and the "restarted server" opens the same DB below.
        drop(s);
        let exe = std::env::current_exe().unwrap();
        let st = std::process::Command::new(exe)
            .args(["--exact", "api::decision_digest::tests::crash_child", "--ignored", "--nocapture"])
            .env("DIGEST_CRASH_HOME", home)
            .env("DIGEST_CRASH_AT", point)
            .output()
            .unwrap();
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(st.status.signal(), Some(libc::SIGKILL), "{point}: {}", String::from_utf8_lossy(&st.stderr));
        let s = state_at(home);
        assert_eq!(ledger(&s, "K-1").state, "applying", "{point}: claim committed before the kill");
        assert_eq!(ledger(&s, "K-1").receipt, "", "{point}");
        let r = tick_with(&m, &a, &s, home, at(4, 18)).await;
        assert_eq!(r.applied, ["K-1"], "{point}");
        assert_eq!(a.sends().len(), 1, "{point}: exactly one message: {:?}", a.sends());
        let row = ledger(&s, "K-1");
        assert_eq!(row.state, "applied", "{point}");
        assert!(!row.receipt.is_empty(), "{point}");
        assert_eq!(status(&s, "K-1"), "todo", "{point}");
        assert!(tick_with(&m, &a, &s, home, at(4, 19)).await.applied.is_empty(), "{point}: no second application");
        assert_eq!(a.sends().len(), 1, "{point}");
    }
}

#[tokio::test]
async fn a_crash_before_the_send_rechecks_every_gate_on_restart() {
    let (dir, s, sent) = setup(true);
    let home = dir.path();
    let (m, a) = (Mail(sent), Acts::new(home));
    safe_card(&s, "K-2");
    publish_twice(&m, &a, &s, home).await;
    tick_with(&m, &a, &s, home, at(4, 10)).await;
    assert!(transition(&s, "K-2", &["tracked"], "applying", "claimed", ACTOR, None).await);
    recommend(&s, "K-2", "Email the customer about the sidebar", SAFE_SCOPE);
    assert!(tick_with(&m, &a, &s, home, at(4, 18)).await.applied.is_empty());
    assert!(a.sends().is_empty());
    assert_eq!(ledger(&s, "K-2").state, "reset");
}

/// The body of the crash test: not a test on its own.
#[tokio::test]
#[ignore = "child process of sigkill_mid_application_*"]
async fn crash_child() {
    let (Ok(home), Ok(point)) = (std::env::var("DIGEST_CRASH_HOME"), std::env::var("DIGEST_CRASH_AT")) else { return };
    let home = std::path::PathBuf::from(home);
    let s = state_at(&home);
    let mut a = Acts::new(&home);
    a.kill_at = Some(point);
    tick_with(&Mail(Arc::new(Mutex::new(vec![]))), &a, &s, &home, at(4, 18)).await;
    panic!("the child should have been killed");
}

#[test]
fn apply_is_default_on_and_every_off_switch_wins() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    assert!(apply_on(home, "lane-a"), "default on");
    std::fs::create_dir_all(home.join("sessions")).unwrap();
    std::fs::write(home.join("sessions/lane-a.env"), format!("{APPLY_KEY}=0\n")).unwrap();
    assert!(!apply_on(home, "lane-a"), "worker scope opts out");
    assert!(apply_on(home, "lane-b"), "and only that worker");
    std::fs::write(home.join("sessions/lane-a.env"), format!("{APPLY_KEY}=maybe\n")).unwrap();
    assert!(!apply_on(home, "lane-a"), "a mistyped value stops");
    std::fs::write(home.join("amux.env"), format!("{APPLY_KEY}=0\n")).unwrap();
    assert!(!apply_on(home, "lane-b"), "global scope opts out");
    std::fs::remove_file(home.join("amux.env")).unwrap();
    std::fs::write(home.join("server.env"), format!("{APPLY_KEY}=0\n")).unwrap();
    assert!(!apply_on(home, "lane-b"), "server.env stops the fleet");
}

/// The plan's limits line, one case each, on a card that otherwise applies.
#[test]
fn the_plans_limits_are_hard_stops() {
    for (text, why) in [
        ("We should spend on a bigger runner", "new_spend"),
        ("Write the corrected rows into TubeScience Supabase", "tubescience_systems"),
        ("Delete the stale snapshot rows", "customer_or_production_data"),
        ("Purge the old sidebar cache", "destructive"),
        ("Add a new endpoint for the sidebar", "repo_gate"),
        ("Add a new primitive for the sidebar", "repo_gate"),
        ("Disable the land gate for this path", "gate_loss"),
    ] {
        assert_eq!(boundary_in_text(text), Some(why), "{text}");
    }
}

#[test]
fn boundary_text_finds_each_class_and_clean_text_alone_is_not_safe() {
    assert_eq!(boundary_in_text("spend $40 on it"), Some("new_spend"));
    assert_eq!(boundary_in_text("take the $40 tier"), Some("money"));
    assert_eq!(boundary_in_text("Reply to the prospect"), Some("outside_send"));
    assert_eq!(boundary_in_text("touches a tenant"), Some("customer_or_production_data"));
    assert_eq!(boundary_in_text("permanently remove it"), Some("irreversible"));
    assert_eq!(boundary_in_text("add a new endpoint"), Some("repo_gate"));
    assert_eq!(boundary_in_text(SAFE_Q), None);
    let v = View {
        card: "X".into(),
        worker: "w".into(),
        item: json!({"ask_type": "decision", "ask_actor": "Ethan", "question": SAFE_Q, "title": "x", "category": "other"}),
        title: "x".into(),
        question: SAFE_Q.into(),
        recommendation: SAFE_REC.into(),
        scope: String::new(),
        source: "agent".into(),
        text: SAFE_Q.into(),
        isolated: false,
        snoozed: false,
        fingerprint: "f".into(),
        log: String::new(),
    };
    assert_eq!(classify_view(&v, 1.0), Class::Unclassified("scope_not_declared".into()));
    let ok = View { scope: SAFE_SCOPE.into(), ..v };
    assert_eq!(classify_view(&ok, 1.0), Class::Safe);
    assert_eq!(classify_view(&View { snoozed: true, ..ok.clone() }, 1.0), Class::Never("snoozed".into()));
    assert_eq!(classify_view(&View { isolated: true, ..ok }, 1.0), Class::Never("isolated_worker".into()));
}

#[test]
fn a_date_only_moves_later_and_needs_two_digests() {
    let v = View {
        card: "X".into(), worker: "w".into(), item: json!({}), title: String::new(), question: String::new(),
        recommendation: String::new(), scope: String::new(), source: String::new(), text: String::new(),
        isolated: false, snoozed: false, fingerprint: "f".into(), log: String::new(),
    };
    let one = publish(None, &v, "2026-11-02", true, 0);
    assert_eq!(one.effect_date, "2026-11-04", "the floor holds");
    assert!(!date_reached(&one, "2026-11-30"), "one digest never fires");
    // The second digest was late: the date moves past it.
    let two = publish(Some(&one), &v, "2026-11-10", true, 2);
    assert_eq!(two.effect_date, "2026-11-11");
    assert!(!date_reached(&two, "2026-11-10"));
    assert!(date_reached(&two, "2026-11-11"));
    assert_eq!(publish(Some(&two), &v, "2026-11-10", true, 2).digests, 2, "one digest a day");
}
