use super::*;
use std::sync::{Arc, Mutex};

fn item(kind: &str, category: &str, ask_type: &str, q: &str) -> Value {
    json!({"key": format!("card:{}", hash(q)), "kind": kind, "card": "T-1", "worker": "lane-a",
           "category": category, "ask_type": ask_type, "question": q, "title": "t", "unblocks": ""})
}

// ---- the money cap parser: live specimens (2026-09-27) --------------------

#[test]
fn money_cap_parser_reads_live_specimens() {
    let over = "Do you approve a TS indexer node (about $90-130/mo spot or $390-425/mo on-demand), or hold?";
    assert_eq!(max_dollar_figure(over), Some(425.0));
    assert_eq!(max_dollar_figure("Approve about $16 of Gemini embedding to re-index?"), Some(16.0));
    assert_eq!(max_dollar_figure("OK to spend $40 of GPU on the backfill?"), Some(40.0));
    assert_eq!(max_dollar_figure("Do you approve GPU spend to re-extract the two broken indexes?"), None);
    assert_eq!(max_dollar_figure("Approve about $1,000 of API spend?"), Some(1000.0));
    assert_eq!(max_dollar_figure("roughly $1.5k a month"), Some(1500.0));
    assert_eq!(max_dollar_figure("about 75 USD"), Some(75.0));
    assert_eq!(max_dollar_figure("$90 to $130"), Some(130.0));
    // A bare $ with no digit, and numbers that are not money.
    assert_eq!(max_dollar_figure("set $AMUX_URL first; 3 lanes, 400 cards"), None);

    // Contract rule 11: spend is never auto-approved, whatever the figure.
    let p = Policy::default();
    for q in [over, "Approve about $16 of Gemini embedding?", "Spend $50?", "Do you approve GPU spend on-demand?"] {
        assert_eq!(decide(&p, &item("card", "money", "budget", q)), Decision::SkipCategory("money".into()), "{q}");
    }
}

// ---- policy matching per category -----------------------------------------

#[test]
fn defaults_approve_judgment_only() {
    let p = Policy::default();
    assert!(p.enabled && p.other && !p.money && !p.prod_data && !p.outbound);
    assert_eq!(p.money_cap_usd, 50.0);
    assert!(p.sources.iter().all(|(k, s)| s == "default" || (matches!(k.as_str(), "money" | "prod_data" | "outbound") && s == CONTRACT_RULE_11)));
    assert_eq!(decide(&p, &item("card", "other", "decision", "Which option?")), Decision::Approve);
    assert_eq!(
        decide(&p, &item("card", "prod_data", "decision", "Migrate prod data?")),
        Decision::SkipCategory("prod_data".into())
    );
    assert_eq!(
        decide(&p, &item("card", "outbound", "customer_outbound", "Send the welcome email?")),
        Decision::SkipCategory("outbound".into())
    );
    // An email approval is outbound whatever its category field says.
    assert_eq!(
        decide(&p, &item("email", "other", "", "Send this email to x@y.com?")),
        Decision::SkipCategory("outbound".into())
    );
    assert_eq!(
        p.summary("all workers"),
        "Auto-approve is ON for all workers: judgment asks and never money, production data, outside parties, scope, priorities or reversals of owner actions. Key, sign-in and grant asks go back to the worker to do itself."
    );

    // Even a policy struct with them switched on cannot approve these.
    let all = Policy { prod_data: true, outbound: true, money: true, ..Policy::default() };
    assert_eq!(decide(&all, &item("card", "prod_data", "decision", "Migrate prod data?")), Decision::SkipCategory("prod_data".into()));
    assert_eq!(decide(&all, &item("email", "outbound", "", "Send this email?")), Decision::SkipCategory("outbound".into()));
    assert_eq!(decide(&all, &item("card", "other", "budget", "Raise the usage limit?")), Decision::SkipCategory("money".into()), "ask_type budget is money");
    let off = Policy { enabled: false, ..all.clone() };
    assert_eq!(decide(&off, &item("card", "other", "decision", "Which option?")), Decision::Off);
    let no_other = Policy { other: false, ..Policy::default() };
    assert_eq!(
        decide(&no_other, &item("card", "other", "decision", "Which option?")),
        Decision::SkipCategory("other".into())
    );
}

#[test]
fn a_group_setting_decision_does_not_go_through_the_credential_ladder() {
    let policy = Policy::default();
    let q = "Can you set the AMUX_CONTRACT_REVIEW_MODEL key in the gs12-platform group scope to Sonnet?";
    assert_eq!(decide(&policy, &item("card", "other", "decision", q)), Decision::Never("owner_must_act"));
    assert_eq!(decide(&policy, &item("card", "other", "access", "Can you sign in to Studio?")), Decision::SendBack("credential_or_access"));
}

#[test]
fn credential_and_access_are_never_approved() {
    let all = Policy { prod_data: true, outbound: true, money_cap_usd: 1e9, send_back: false, ..Policy::default() };
    assert!(matches!(decide(&all, &item("card", "other", "credential", "Mint a key")), Decision::Never(_)));
    assert!(matches!(decide(&all, &item("card", "other", "access", "Add me to the GCP project")), Decision::Never(_)));
    // Filed without the ask type, recognised by the text.
    assert!(matches!(decide(&all, &item("card", "other", "decision", "Sign in once at the vercel app?")), Decision::Never(_)));
    assert!(matches!(decide(&all, &item("card", "other", "", "Paste the Stripe API key into server.env")), Decision::Never(_)));
}

// ---- scoped resolution -----------------------------------------------------

#[test]
fn policy_resolves_worker_over_group_over_global_with_sources() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    std::fs::create_dir_all(home.join("sessions")).unwrap();
    std::fs::create_dir_all(home.join("env")).unwrap();
    std::fs::write(home.join("amux.env"), "AMUX_NEEDS_INPUT_AUTO_OUTBOUND=1\nAMUX_NEEDS_INPUT_AUTO_MONEY_CAP=200\n").unwrap();
    std::fs::write(home.join("env/gtm.env"), "AMUX_NEEDS_INPUT_AUTO_MONEY_CAP=10\n").unwrap();
    std::fs::write(home.join("sessions/lane-a.env"), "CC_TAGS=gtm\nAMUX_NEEDS_INPUT_AUTO=0\n").unwrap();
    std::fs::write(home.join("sessions/lane-b.env"), "CC_TAGS=ops\n").unwrap();

    let a = resolve(home, "lane-a");
    assert!(!a.enabled);
    assert_eq!(a.sources["enabled"], "worker");
    assert_eq!(a.money_cap_usd, 10.0);
    assert_eq!(a.sources["money_cap_usd"], "group:gtm");
    assert!(!a.outbound, "contract rule 11: a scope cannot switch outbound on");
    assert_eq!(a.sources["outbound"], CONTRACT_RULE_11);
    assert_eq!(a.sources["other"], "default");
    assert!(a.summary("lane-a").starts_with("Auto-approve is OFF for lane-a"));

    let b = resolve(home, "lane-b");
    assert!(b.enabled);
    assert_eq!(b.money_cap_usd, 200.0);
    assert_eq!(b.sources["money_cap_usd"], "global");

    // The server.env kill switch wins over every layer.
    std::fs::write(home.join("server.env"), "AMUX_NEEDS_INPUT_AUTO=0\n").unwrap();
    let b = resolve(home, "lane-b");
    assert!(!b.enabled);
    assert_eq!(b.sources["enabled"], "server.env");
}

// ---- through the job, against a hermetic store ----------------------------

#[derive(Default)]
struct Mock {
    sends: Mutex<Vec<(String, String)>>,
    emails: Mutex<Vec<String>>,
    fyis: Mutex<Vec<String>>,
}

#[async_trait::async_trait]
impl Actions for Arc<Mock> {
    async fn send(&self, _: &AppState, worker: &str, text: &str, _: &str) -> Result<String, String> {
        if worker == "lane-refuse" {
            return Err("send to lane-refuse: steering queue refused".into());
        }
        if worker == "lane-paused" {
            return Err("send to lane-paused: target is paused: amux automation is not queued for a paused worker".into());
        }
        self.sends.lock().unwrap().push((worker.into(), text.into()));
        Ok(format!("message queued for {worker}"))
    }
    async fn card(&self, state: &AppState, card: &str, note: &str, marker: &str) -> Result<String, String> {
        // The REAL board half, against the hermetic store.
        card_via_board(state, card, note, marker).await
    }
    async fn email(&self, _: &AppState, id: &str) -> Result<String, String> {
        self.emails.lock().unwrap().push(id.into());
        Ok("sent".into())
    }
    async fn fyi(&self, _: &AppState, text: &str) {
        self.fyis.lock().unwrap().push(text.into());
    }
}

fn state() -> AppState {
    let dir = tempfile::tempdir().unwrap();
    let store = crate::db::Store::open(&dir.path().join("ni-auto.db")).unwrap();
    std::mem::forget(dir);
    AppState {
        store: Arc::new(store),
        started: std::time::Instant::now(),
        build_hash: "test".into(),
        auth_token: None,
        reconciled: Arc::new(std::sync::atomic::AtomicBool::new(true)),
    }
}

fn seed(state: &AppState, id: &str, worker: &str, ask_type: &str, q: &str, now: f64) {
    let (id, worker, ask_type, q) = (id.to_string(), worker.to_string(), ask_type.to_string(), q.to_string());
    let t = now as i64;
    state
        .store
        .write(move |conn| {
            conn.execute(
                "INSERT INTO issues (id,title,desc,status,session,created,updated,type,archived,ask_type,ask_question,ask_unblocks,ask_actor,entered_state_at)
                 VALUES (?1,?2,'context',  'needsyou',?3,?4,?4,'code',0,?5,?6,'unblocks it','Ethan',?4)",
                rusqlite::params![id, format!("title {id}"), worker, t, ask_type, q],
            )?;
            Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
        })
        .unwrap();
}

fn card(state: &AppState, id: &str) -> crate::db::board_store::IssueRow {
    crate::db::board_store::get_issue(&state.store.read().unwrap(), id).unwrap().unwrap()
}

fn outcomes(state: &AppState) -> BTreeMap<String, String> {
    load_ledger(&state.store.read().unwrap())
        .unwrap_or_default()
        .into_iter()
        .map(|e| (e.card, e.outcome))
        .collect()
}

#[tokio::test]
async fn job_approves_new_items_once_and_leaves_the_rest() {
    let st = state();
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().to_path_buf();
    std::fs::create_dir_all(home.join("sessions")).unwrap();
    std::fs::create_dir_all(home.join("email-approvals")).unwrap();
    let now = now_f64();
    let mock = Arc::new(Mock::default());

    // Already waiting when the policy first runs: the baseline.
    seed(&st, "OLD-1", "lane-a", "decision", "Which schema option?", now - 500.0);
    let rep = tick_with(&mock, &st, &home, now).await;
    assert_eq!(rep.baseline, 1);
    assert!(rep.approved.is_empty());
    assert!(mock.sends.lock().unwrap().is_empty(), "the first run approves nothing");
    assert_eq!(card(&st, "OLD-1").status, "needsyou");

    // New arrivals.
    seed(&st, "NEW-OTHER", "lane-a", "decision", "Option A or B? I recommend A.", now);
    seed(&st, "NEW-16", "lane-a", "budget", "Approve about $16 of Gemini embedding?", now);
    seed(&st, "NEW-425", "lane-a", "budget", "Approve a node at about $90-130/mo spot or $390-425/mo on-demand?", now);
    seed(&st, "NEW-NOFIG", "lane-a", "budget", "Approve GPU spend for the re-extract?", now);
    seed(&st, "NEW-CRED", "lane-a", "credential", "Mint the Stripe key", now);
    seed(&st, "NEW-PROD", "lane-a", "decision", "OK to migrate prod data to the new shard?", now);
    seed(&st, "NEW-SNOOZE", "lane-a", "decision", "Rename the flag?", now);
    seed(&st, "NEW-OFFLANE", "lane-off", "decision", "Proceed with the refactor?", now);
    seed(&st, "NEW-REFUSE", "lane-refuse", "decision", "Proceed with the rollout?", now);
    seed(&st, "NEW-PAUSED", "lane-paused", "decision", "Proceed with the cleanup?", now);
    std::fs::write(home.join("sessions/lane-off.env"), "AMUX_NEEDS_INPUT_AUTO=0\n").unwrap();
    std::fs::write(
        home.join("email-approvals/apr_00000000000000bb.json"),
        json!({"id":"apr_00000000000000bb","created":now,"session":"gtm","endpoint":"send",
               "preview":{"to":"x@example.com","subject":"hi","body":"hello"}})
        .to_string(),
    )
    .unwrap();
    st.store
        .write(move |conn| {
            conn.execute(
                "INSERT INTO prefs (key,value) VALUES (?1,?2)",
                rusqlite::params![needs_input::SNOOZE_KEY, json!({"card:NEW-SNOOZE": now + 3600.0}).to_string()],
            )?;
            Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
        })
        .unwrap();

    let rep = tick_with(&mock, &st, &home, now + 60.0).await;
    let mut approved = rep.approved.clone();
    approved.sort();
    // A paused lane gets the answer on its card (RH-131, 2026-09-27).
    assert_eq!(approved, ["NEW-OTHER", "NEW-PAUSED"]);
    assert_eq!(rep.refused, ["NEW-REFUSE"]);
    // The credential ask goes back to its worker instead of waiting.
    assert_eq!(rep.sent_back, ["NEW-CRED"]);
    let sends = mock.sends.lock().unwrap().clone();
    assert_eq!(sends.len(), 2, "{sends:?}");
    assert_eq!(sends.iter().filter(|(w, t)| w == "lane-a" && t.starts_with("Approved (NEW-") && t.contains(". Proceed.")).count(), 1);
    assert!(sends.iter().any(|(w, t)| w == "lane-a" && t.starts_with("Sent back (NEW-CRED)")));
    assert!(mock.emails.lock().unwrap().is_empty(), "outbound is off by default");
    assert_eq!(mock.fyis.lock().unwrap().len(), 1, "one FYI per batch");

    // The approve moved the card out of needsyou and noted it, through the
    // real board PATCH.
    for id in ["NEW-OTHER", "NEW-PAUSED", "NEW-CRED"] {
        let c = card(&st, id);
        assert_eq!(c.status, "todo", "{id}");
        assert!(c.desc.contains("Approved automatically by owner policy") || c.desc.contains("Sent back to lane-a"), "{id}: {}", c.desc);
    }
    // Everything else is untouched and still waiting on the owner.
    for id in ["OLD-1", "NEW-16", "NEW-425", "NEW-NOFIG", "NEW-PROD", "NEW-SNOOZE", "NEW-OFFLANE", "NEW-REFUSE"] {
        assert_eq!(card(&st, id).status, "needsyou", "{id}");
    }
    let o = outcomes(&st);
    assert_eq!(o["OLD-1"], "baseline");
    // Contract rule 11: spend stays with the owner at any figure.
    assert_eq!(o["NEW-16"], "skipped_category");
    assert_eq!(o["NEW-425"], "skipped_category");
    assert_eq!(o["NEW-NOFIG"], "skipped_category");
    assert_eq!(o["NEW-CRED"], "sent_back");
    assert_eq!(o["NEW-PAUSED"], "approved");
    assert_eq!(o["NEW-PROD"], "skipped_category");
    assert_eq!(o["NEW-SNOOZE"], "snoozed");
    assert_eq!(o["NEW-OFFLANE"], "off");
    assert_eq!(o["NEW-REFUSE"], "refused");
    assert_eq!(o["apr_00000000000000bb"], "skipped_category");

    // Dedupe: the next tick does nothing, including no retry of the refusal.
    let rep = tick_with(&mock, &st, &home, now + 120.0).await;
    assert!(rep.approved.is_empty() && rep.refused.is_empty() && rep.sent_back.is_empty(), "{rep:?}");
    assert_eq!(mock.sends.lock().unwrap().len(), 2);
    assert_eq!(mock.fyis.lock().unwrap().len(), 1);

    // The GET view lists approved and refused only, newest first.
    let led = load_ledger(&st.store.read().unwrap()).unwrap();
    let v = view(&home, "", &led, &[], now + 120.0);
    // Two approvals (one to a paused lane), one send-back, one refusal.
    assert_eq!(v["recent_count"], 4);
    assert_eq!(v["summary"], "Auto-approve is ON for all workers: judgment asks and never money, production data, outside parties, scope, priorities or reversals of owner actions. Key, sign-in and grant asks go back to the worker to do itself.");

    // The owner's explicit sweep, with outbound switched on globally: the
    // baseline item and the held email are re-evaluated; the credential ask,
    // the snoozed item and the refusal are still left alone.
    std::fs::write(home.join("amux.env"), "AMUX_NEEDS_INPUT_AUTO_OUTBOUND=1\n").unwrap();
    let (n, rep) = sweep_with(&mock, &st, &home, now + 180.0).await;
    assert!(n >= 6, "{n}");
    let mut approved = rep.approved.clone();
    approved.sort();
    assert_eq!(approved, ["OLD-1"], "outbound stays with the owner even when a scope switches it on");
    assert!(mock.emails.lock().unwrap().is_empty());
    assert_eq!(card(&st, "NEW-CRED").status, "todo", "sent back, not re-evaluated");
    assert_eq!(card(&st, "NEW-SNOOZE").status, "needsyou");
    assert_eq!(card(&st, "NEW-REFUSE").status, "needsyou");
    assert_eq!(card(&st, "NEW-425").status, "needsyou");

    // The server.env kill switch stops the job outright.
    seed(&st, "NEW-AFTER-KILL", "lane-a", "decision", "Proceed?", now + 200.0);
    std::fs::write(home.join("server.env"), "AMUX_NEEDS_INPUT_AUTO=0\n").unwrap();
    let rep = tick_with(&mock, &st, &home, now + 240.0).await;
    assert!(!rep.ran);
    assert_eq!(card(&st, "NEW-AFTER-KILL").status, "needsyou");
}

#[test]
fn owner_action_and_public_surface_are_never_auto_approved_but_mentions_are() {
    let all = Policy { enabled: true, other: true, money: true, money_cap_usd: 50.0, prod_data: true, outbound: true, ..Default::default() };
    // Not approved: left for the owner, or (send_back) returned to the worker.
    let never = |at: &str, q: &str| matches!(decide(&all, &item("card", "other", at, q)), Decision::Never(_) | Decision::SendBack(_));
    // Live 2026-09-27: asks the OWNER must act on.
    assert!(never("decision", "Will you run `! ~/.amux/seed-standing-approvals.sh` once to record the two standing approvals?"));
    assert!(never("credential", "Can you mint a new Ethan Personal org API key in Studio?"));
    // Repo rule: new endpoints and public surface stay with the owner.
    assert!(never("decision", "Should POST /v1/organizations/billing/estimate be reachable without an API key?"));
    assert!(never("decision", "Sign off (or amend) the MP-106 PITR API design so restore-to-an-older-checkpoint can be built?"));
    // Mentions are not asks: these are the worker's call.
    assert!(!never("credential", "Want me to do that pass, or would you rather look at the categories yourself first?"));
    assert!(!never("decision", "Want me to go after the Gemini source now, or wait for gs-4 to say whether it's theirs?"));
    assert!(!never("decision", "Should Mixpeek offer a sandbox or demo API key so a prospect can test one call?") );
}

#[test]
fn the_recorders_unblocks_boilerplate_does_not_make_an_ask_a_credential() {
    let all = Policy { enabled: true, other: true, money: true, money_cap_usd: 50.0, prod_data: true, outbound: true, ..Default::default() };
    let mut it = item("card", "other", "credential", "Want me to do that pass, or would you rather look at the categories yourself first?");
    it["unblocks"] = serde_json::json!("ethan completes the sign-in, grant or credential step named in the question and notes it on this card.");
    assert_eq!(decide(&all, &it), Decision::Approve);
}

#[test]
fn owner_action_asks_go_back_to_the_worker_unless_the_boundary_holds_them() {
    let p = Policy::default();
    assert!(p.send_back, "on by default (Ethan, 2026-09-27 20:23)");
    let d = |cat: &str, at: &str, q: &str| decide(&p, &item("card", cat, at, q));
    // Live specimens, 2026-09-27: the worker can reach these down the ladder.
    assert!(matches!(d("credential", "credential", "Can you mint a new Ethan Personal org API key in Studio (with admin if possible) and store it as GTM_MIXPEEK_API_KEY?"), Decision::SendBack(_)));
    assert!(matches!(d("credential", "access", "Can you rotate NPM_TOKEN in the mixpeek org's GitHub secrets?"), Decision::SendBack(_)));
    assert!(matches!(d("other", "access", "Can you run `! gcloud auth login info@mixpeek.com` in the gs-5-one-click session?"), Decision::SendBack(_)));
    // The boundary keeps these with the owner: spend, outside, prod data,
    // his own approvals, and the repo's public-surface rule.
    assert!(matches!(d("money", "credential", "Anthropic API credit balance is exhausted; can you top it up?"), Decision::SkipCategory(_) | Decision::Never(_)));
    assert!(matches!(d("other", "decision", "Will you run `! ~/.amux/seed-standing-approvals.sh` once to record the two standing approvals?"), Decision::Never(_)));
    assert!(matches!(d("other", "decision", "Should POST /v1/organizations/billing/estimate be reachable without an API key?"), Decision::Never("public_surface")));
    let off = Policy { send_back: false, ..Policy::default() };
    assert!(matches!(decide(&off, &item("card", "credential", "credential", "Can you mint a key?")), Decision::Never(_)));
    let t = send_back_text("MM-83", "Can you mint a key?");
    assert!(t.contains("CDP") && t.contains("amux computer") && t.contains("revoke the old") && !t.contains('\u{2014}'));
}

#[tokio::test]
async fn send_back_once_then_the_reask_is_the_owners_and_stale_pending_is_retried() {
    let st = state();
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().to_path_buf();
    std::fs::create_dir_all(home.join("sessions")).unwrap();
    std::fs::create_dir_all(home.join("email-approvals")).unwrap();
    let now = now_f64();
    let mock = Arc::new(Mock::default());
    tick_with(&mock, &st, &home, now).await; // empty baseline

    seed(&st, "KEY-1", "lane-a", "credential", "Can you mint a new org API key in Studio?", now);
    let rep = tick_with(&mock, &st, &home, now + 60.0).await;
    assert_eq!(rep.sent_back, ["KEY-1"]);
    let c = card(&st, "KEY-1");
    assert_eq!(c.status, "todo");
    assert!(c.desc.contains("Sent back to lane-a"), "{}", c.desc);
    let sends = mock.sends.lock().unwrap().clone();
    assert!(sends.iter().any(|(w, t)| w == "lane-a" && t.starts_with("Sent back (KEY-1)")), "{sends:?}");
    assert!(mock.fyis.lock().unwrap().iter().any(|f| f.contains("Sent 1 back")));

    // The worker tried the ladder and re-asked: it stays with the owner.
    st.store
        .write(move |conn| {
            conn.execute(
                "UPDATE issues SET status='needsyou', ask_question='Studio sign-in failed on all three rungs (browser: no profile, CDP: Chrome closed, CUA: no Studio session). Can you mint the key?' WHERE id='KEY-1'",
                [],
            )?;
            Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
        })
        .unwrap();
    let rep = tick_with(&mock, &st, &home, now + 120.0).await;
    assert!(rep.sent_back.is_empty());
    assert_eq!(card(&st, "KEY-1").status, "needsyou");
    assert_eq!(outcomes(&st)["KEY-1"], "never");

    // A pending claim from a tick that never finished is retried later.
    seed(&st, "STUCK-1", "lane-a", "decision", "Option A or B? I recommend A.", now + 130.0);
    let dk = {
        let q = needs_input::build(&st.store.read().unwrap(), &home, now + 130.0).unwrap();
        dedupe_key(q.items.iter().find(|i| i["card"] == "STUCK-1").unwrap())
    };
    let row = Entry { dk: dk.clone(), key: "card:STUCK-1".into(), card: "STUCK-1".into(), outcome: "pending".into(), at: now + 130.0, ..Default::default() };
    append(&st, vec![row], HashSet::new(), now + 130.0).await;
    let rep = tick_with(&mock, &st, &home, now + 140.0).await;
    assert!(rep.approved.is_empty(), "a fresh claim is still in flight");
    let rep = tick_with(&mock, &st, &home, now + 130.0 + PENDING_STALE_S + 1.0).await;
    assert_eq!(rep.approved, ["STUCK-1"]);
    assert_eq!(card(&st, "STUCK-1").status, "todo");
}

#[test]
fn the_three_misrouted_send_backs_stay_with_the_owner() {
    // Live 2026-09-27 20:40: all three went back to their workers.
    use crate::api::needs_input::classify;
    let p = Policy::default();
    let b3504 = "Approve the shipped BACKE-3504 design (45e9ba8f0f) on its two owner points: a new POST /v1/ops/events endpoint (native ops-event front door, in-schema, API-key-authed, best-effort)?";
    assert_eq!(decide(&p, &item("card", "other", "credential", b3504)), Decision::Never("public_surface"));
    let b3621 = "The all-content -> universal_extractor collection migration (BACKE-3621) is ready and safe to run, but it is a PROD customer-data mutation (renames collections across namespaces) and this session has no prod MONGO_URI, so I will not run it.";
    assert_eq!(classify("access", b3621).0, "prod_data");
    let g54 = "The logo corpus re-ingest you approved on MI-5270 at a 37,151-doc cost basis actually measures 189,830 docs (5.1x): approve the full re-ingest at the measured size, or cap or decline it?";
    assert_eq!(classify("budget", g54).0, "money");
    let cat = |q: &str, at: &str| { let (c, _) = classify(at, q); let mut it = item("card", c, at, q); it["category"] = json!(c); it };
    assert!(!matches!(decide(&p, &cat(b3621, "access")), Decision::SendBack(_)));
    assert!(!matches!(decide(&p, &cat(g54, "budget")), Decision::SendBack(_)));
}

#[test]
fn a_new_primitive_or_route_is_public_surface() {
    let p = Policy::default();
    let d = |q: &str| decide(&p, &item("card", "other", "decision", q));
    assert_eq!(d("Approve a new collection SourceType = a retriever execution materialized by a group_by key (one document per key)?"), Decision::Never("public_surface"));
    assert_eq!(d("Should app slug availability be checkable (for example a new GET) before a create or PATCH attempt?"), Decision::Never("public_surface"));
    assert_eq!(d("Should I get a new test fixture for the flaky suite?"), Decision::Approve);
}

#[test]
fn a_call_the_lane_reserves_for_the_owner_is_never_approved() {
    // AH-296, 2026-10-01: a scope choice for Ethan, approved automatically.
    let ah296 = item("card", "decision", "decision",
        "This is Ethan's call: keep goal spec 12's full scope and accept a later finish, or keep Sunday and narrow to what the 66 completion cards depend on?");
    assert_eq!(never_reason(&ah296), Some("reserved_for_owner"));
    let reserved = item("card", "decision", "judgment", "Cutting scope or moving the date is your call.");
    assert_eq!(never_reason(&reserved), Some("reserved_for_owner"));
    let worker_choice = item("card", "decision", "decision", "Want me to go after the Gemini source now, or wait for gs-4?");
    assert_eq!(never_reason(&worker_choice), None, "an either/or the lane did not reserve stays approvable (policy)");
}

#[test]
fn lowering_what_is_kept_for_the_owner_is_never_approved() {
    // MO-4161, 2026-10-02: approved automatically, then a lane was asked to apply it.
    let mo4161 = item("card", "decision", "other",
        "The Claude plan window is 92 percent used and AMUX_BACKGROUND_RESERVE_PCT keeps 30 percent for you, so amux refused the SCHED-544 orchestration tick at 10:39Z; do you want the reserve lowered?");
    assert_eq!(never_reason(&mo4161), Some("owners_own_share"));
    let note = item("card", "decision", "other", "30% is reserved for the human; may I set it to 10?");
    assert_eq!(never_reason(&note), Some("owners_own_share"));
    let ordinary = item("card", "decision", "decision", "Want me to keep the retry loop for another hour, or stop it now?");
    assert_eq!(never_reason(&ordinary), None, "keeping something that is not the owner's stays approvable");
}

#[test]
fn production_and_customer_changes_are_judged_as_prod_data() {
    // The three asks the policy approved on 2026-10-02, verbatim.
    for (id, q) in [
        ("MO-4149", "May gs12-compute disable webhook wh_eedaf761b44205c3 (brand_brain_ops_webhook, url placeholder.brand-brain.example.com, Bearer PLACEHOLDER_TOKEN) in your own org int_40ed22c1 (Ethan Personal), which has failed every collection.documents.written delivery since at least 2026-09-27 (23,307 FAILED events, 17,062 in the last 24 h) and keeps WebhookDLQBacklog and WebhookSLOBurnRateCritical paging?"),
        ("MO-4150", "Should the TubeScience ts-api Deployment stay at 1 replica under the HTTPScaledObject (0 to 1, hand-applied 2026-09-28 by mixpeek-finances and now declared in the model) or go back to the 3 replicas the chart rendered before?"),
        ("MO-4160", "May gs12-gates run the 8.7 end-to-end pipeline test through the TubeScience tenant ring, which promotes a no-op change (identical image, no config change) to the TubeScience production plane and rolls it back on a planted verify failure, once the nine stages exist (after 8.2, 8.3 and 8.4), and in which window?"),
    ] {
        let it = item("card", "decision", "other", q);
        assert_eq!(never_reason(&it), None, "{id}: not a never-rule");
        assert_eq!(decide(&Policy::default(), &it), Decision::SkipCategory("prod_data".into()), "{id}");
    }
    let local = item("card", "decision", "decision", "Want me to scale the local kind cluster to three nodes for the soak, or keep one?");
    assert_eq!(decide(&Policy::default(), &local), Decision::Approve, "a local change names no production or customer target");
}

#[test]
fn a_scope_or_deadline_decision_is_never_auto_approved() {
    // 2026-10-05: an auto-approval that named no list deferred seven GS-12 plan
    // items. Contract rule 11 keeps scope with the owner.
    let p = Policy::default();
    for q in [
        "Defer docs and second cloud providers out of the done-by path?",
        "May I cut these five items from the plan to hit the deadline?",
        "Move the deadline to Friday?",
        "Descope 4.11 sharding for now?",
    ] {
        assert_eq!(decide(&p, &item("card", "other", "decision", q)), Decision::Never("scope_decision"), "{q}");
    }
    // An ordinary judgment ask is still approved.
    assert_eq!(decide(&p, &item("card", "other", "decision", "Option A or B for the cache key? I recommend A.")), Decision::Approve);
}

#[test]
fn an_explicit_auto_approval_exclusion_wins_over_category_and_send_back() {
    let policy = Policy { send_back: true, ..Policy::default() };
    for field in ["question", "unblocks", "context"] {
        for exclusion in [
            "An automatic needs-input approval does not cover these (production data and a customer-facing API).",
            "This decision is not covered by automatic approval.",
            "Do not auto-approve this decision.",
            "This decision requires explicit human approval.",
        ] {
            let mut it = item("card", "other", "decision", "Choose the cache layout? I recommend A.");
            it[field] = json!(exclusion);
            assert_eq!(decide(&policy, &it), Decision::Never("explicit_approval_required"), "{field}: {exclusion}");
        }
    }
    assert_eq!(decide(&policy, &item("card", "other", "decision",
        "Choose the cache layout? No human approval is required.")), Decision::Approve);
}

#[test]
fn moving_or_retiring_production_data_is_not_a_judgment_ask() {
    for question in [
        "Move production usage and invoices onto the rollups?",
        "Retire the production usage and invoice ledgers?",
    ] {
        assert_eq!(decide(&Policy::default(), &item("card", "other", "decision", question)),
            Decision::SkipCategory("prod_data".into()), "{question}");
    }
    assert_eq!(decide(&Policy::default(), &item("card", "other", "decision",
        "Move local fixture usage onto rollups?")), Decision::Approve);
}

#[tokio::test]
async fn explicit_exclusions_survive_the_real_queue_and_approval_job() {
    let st = state();
    let dir = tempfile::tempdir().unwrap();
    let now = now_f64();
    let mock = Arc::new(Mock::default());
    assert!(tick_with(&mock, &st, dir.path(), now).await.ran);
    seed(&st, "GS-199", "lane-a", "decision",
        "Move production usage and invoices onto the rollups, retire two ledgers, and add two fields to the usage breakdown API?", now);
    seed(&st, "LOCAL-EXCLUDED", "lane-a", "decision", "Choose the local cache layout?", now);
    seed(&st, "LOCAL-ALLOWED", "lane-a", "decision", "Choose the cache key? I recommend A.", now);
    st.store.write(|conn| {
        conn.execute("UPDATE issues SET ask_unblocks=?1 WHERE id IN ('GS-199','LOCAL-EXCLUDED')",
            ["An automatic needs-input approval does not cover these (production data and a customer-facing API)."])?;
        Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
    }).unwrap();

    let rep = tick_with(&mock, &st, dir.path(), now + 1.0).await;
    assert_eq!(rep.approved, ["LOCAL-ALLOWED"]);
    for id in ["GS-199", "LOCAL-EXCLUDED"] {
        assert_eq!(card(&st, id).status, "needsyou", "{id} must retain its ask");
        assert_eq!(outcomes(&st)[id], "never");
        assert!(load_ledger(&st.store.read().unwrap()).unwrap().iter()
            .any(|e| e.card == id && e.detail.contains("explicit approval")), "{id}: auditable hold reason");
    }
    assert_eq!(mock.sends.lock().unwrap().len(), 1, "only the authorized local control is sent");
    assert!(mock.emails.lock().unwrap().is_empty());
    assert!(tick_with(&mock, &st, dir.path(), now + 2.0).await.approved.is_empty());
    assert_eq!(mock.sends.lock().unwrap().len(), 1, "a repeated job does not act again");
}

#[test]
fn an_ask_that_names_nothing_or_a_host_service_is_never_auto_approved() {
    let bare = item("needsyou", "other", "credential", "Shall I remove it?");
    assert_eq!(never_reason(&bare), Some("no_artifact_named"), "AH-391");
    let named = item("needsyou", "other", "decision", "Shall I remove the stale fixture directory under tests/data?");
    assert_ne!(never_reason(&named), Some("no_artifact_named"), "a named object is judgeable");
    let host = item("needsyou", "other", "decision", "Unload the io.amux.project-test-18972 launchd agent left from the Sep 23 install test?");
    assert_eq!(never_reason(&host), Some("host_service"));
}

#[test]
fn a_host_change_or_an_unnamed_ask_stays_with_the_owner_even_with_send_back_on() {
    let policy = Policy { enabled: true, send_back: true, ..Policy::default() };
    for q in ["Unload the io.amux.project-test-18972 launchd agent?", "Shall I remove it?"] {
        let it = item("needsyou", "other", "credential", q);
        assert!(matches!(decide(&policy, &it), Decision::Never(_)), "{q}: {:?}", decide(&policy, &it));
    }
}

#[test]
fn an_ask_to_speak_in_the_owners_name_is_never_auto_approved_or_sent_back() {
    let policy = Policy { enabled: true, send_back: true, ..Policy::default() };
    let q = "Should amux-helper tell mixpeek-override, in your name, that every GS-12 lane's next card is a reopened proof card?";
    let it = item("needsyou", "other", "decision", q);
    assert_eq!(never_reason(&it), Some("owner_voice"), "AH-394");
    assert!(matches!(decide(&policy, &it), Decision::Never("owner_voice")));
    let plain = item("needsyou", "other", "decision", "Should gs12-planes take GP-201 before GP-199 this afternoon?");
    assert_ne!(never_reason(&plain), Some("owner_voice"));
}

#[test]
fn owner_control_decisions_are_held_without_credential_send_back() {
    let policy = Policy { send_back: true, ..Policy::default() };
    for q in [
        "Should I change the fleet's priorities?",
        "Should I stop all workers?",
        "Want me to re-enable SCHED-608, which you turned off?",
    ] {
        assert_eq!(decide(&policy, &item("card", "other", "decision", q)), Decision::Never("owner_control_decision"), "{q}");
    }
    assert_eq!(decide(&policy, &item("card", "other", "decision", "Should I rerun the flaky fixture test?")), Decision::Approve);
}
