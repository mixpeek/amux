//! Self-contained board policy runs in its own process (environment is global).
use amux_server::api::{router, AppState};
use amux_server::db::Store;
use axum::{body::Body, http::{Request, StatusCode}};
use serde_json::{json, Value};
use tower::ServiceExt;

async fn call(app: &axum::Router, method: &str, path: &str, worker: &str, body: Value) -> (StatusCode, Value) {
    let mut req = Request::builder().method(method).uri(path).header("Content-Type","application/json");
    if !worker.is_empty() { req = req.header("X-Amux-Worker", worker); }
    let req = req.body(Body::from(body.to_string())).unwrap();
    let r=app.clone().oneshot(req).await.unwrap();
    let status=r.status();
    let bytes=axum::body::to_bytes(r.into_body(),usize::MAX).await.unwrap();
    (status,serde_json::from_slice(&bytes).unwrap())
}

#[tokio::test]
async fn workers_keep_assignments_and_dependencies_on_their_own_board() {
    let home=tempfile::tempdir().unwrap();
    std::fs::create_dir(home.path().join("sessions")).unwrap();
    for lane in ["owner","peer"] {
        std::fs::write(home.path().join(format!("sessions/{lane}.env")),"CC_DIR=/tmp\n").unwrap();
    }
    std::env::set_var("AMUX_HOME",home.path());
    std::env::set_var("AMUX_BOARD_DELEGATION","0");
    let store=std::sync::Arc::new(Store::open(&home.path().join("test.db")).unwrap());
    let app=router(AppState{store:store.clone(),started:std::time::Instant::now(),build_hash:"test".into(),auth_token:None,
        reconciled:std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true))});
    let create_body=|title:&str|json!({"title":title,"status":"backlog","type":"chore","next_action":"Implement the local outcome","acceptance_criteria":["Recorded artifact passes its check"]});
    let (s,dep)=call(&app,"POST","/api/board","peer",create_body("Peer component")).await;
    assert_eq!(s,StatusCode::CREATED,"{dep}");
    let peer_id=dep["id"].as_str().unwrap();
    let (s,own)=call(&app,"POST","/api/board","owner",create_body("Own outcome")).await;
    assert_eq!(s,StatusCode::CREATED,"{own}");
    let id=own["id"].as_str().unwrap();
    let count=||store.read().unwrap().query_row("SELECT count(*) FROM issues",[],|r|r.get::<_,i64>(0)).unwrap();
    let before=count();
    for route in [json!({"request_to":"peer"}),json!({"session":"peer"})] {
        let mut b=create_body("Not another worker assignment");
        b.as_object_mut().unwrap().extend(route.as_object().unwrap().clone());
        let (s,v)=call(&app,"POST","/api/board","owner",b).await;
        assert!(matches!(s,StatusCode::CONFLICT|StatusCode::FORBIDDEN),"{v}");
    }
    let mut b=create_body("Not a cross-board wait"); b["depends_on"]=json!([peer_id]);
    let (s,v)=call(&app,"POST","/api/board","owner",b).await;
    assert_eq!(s,StatusCode::CONFLICT,"{v}");
    assert_eq!(v["code"],"cross_board_dependency_forbidden");
    assert!(v["how_to_fix"].as_str().unwrap().contains("Do not relocate a peer wait"));
    assert_eq!(count(),before,"no refused path may mint a card");
    let (s,v)=call(&app,"PATCH",&format!("/api/board/{id}"),"owner",json!({"depends_on":[peer_id],"desc_append":"must not persist"})).await;
    assert_eq!(s,StatusCode::CONFLICT,"{v}");
    assert_eq!(v["code"],"cross_board_dependency_forbidden");
    let (_,unchanged)=call(&app,"GET",&format!("/api/board/{id}"),"owner",Value::Null).await;
    assert_eq!(unchanged["rev"],own["rev"]);
    assert_eq!(unchanged["desc"],own["desc"]);
    // Same-board real dependencies still work; historical foreign edges can
    // be removed in the same edit that records the owner's next action.
    let (_,local)=call(&app,"POST","/api/board","owner",create_body("Local input")).await;
    let (s,v)=call(&app,"PATCH",&format!("/api/board/{id}"),"owner",json!({"depends_on":[local["id"]]})).await;
    assert_eq!(s,StatusCode::OK,"{v}");
    let (s,v)=call(&app,"PATCH",&format!("/api/board/{id}"),"owner",json!({"depends_on":[],"desc_append":format!("Reuse evidence from {peer_id}; own any missing implementation here")})).await;
    assert_eq!(s,StatusCode::OK,"{v}");
    // A legacy delegation opt-in never authorizes execution dependencies.
    std::env::set_var("AMUX_BOARD_DELEGATION", "1");
    for target in [peer_id, "MISSING-999"] {
        let mut b = create_body("Forbidden even when delegation is enabled");
        b["depends_on"] = json!([target]);
        let (s, v) = call(&app, "POST", "/api/board", "owner", b).await;
        assert_eq!(s, StatusCode::CONFLICT, "{v}");
        assert_eq!(v["code"], "cross_board_dependency_forbidden");
    }
    let local_id = local["id"].as_str().unwrap();
    let (s, v) = call(&app, "PATCH", &format!("/api/board/{id}"), "owner", json!({"depends_on":[local_id]})).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let (_, before_move) = call(&app, "GET", &format!("/api/board/{local_id}"), "owner", Value::Null).await;
    let (s, v) = call(&app, "PATCH", &format!("/api/board/{local_id}"), "", json!({"session":"peer", "desc_append":"must not persist"})).await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    assert_eq!(v["relation"], "incoming_dependents");
    let (_, after_move) = call(&app, "GET", &format!("/api/board/{local_id}"), "owner", Value::Null).await;
    assert_eq!(before_move["rev"], after_move["rev"]);
    assert_eq!(before_move["desc"], after_move["desc"]);
    assert_eq!(after_move["session"], "owner");

    let legacy_id = id.to_owned();
    let legacy_peer = peer_id.to_owned();
    store.write(move |conn| {
        conn.execute("UPDATE issues SET status='verified', depends_on=?1 WHERE id=?2", rusqlite::params![json!([legacy_peer]).to_string(), legacy_id])?;
        Ok(amux_server::db::WriteOutcome { applied: true, events: vec![] })
    }).unwrap();
    let (s, v) = call(&app, "PATCH", &format!("/api/board/{id}"), "owner", json!({"status":"backlog"})).await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    assert_eq!(v["code"], "cross_board_dependency_forbidden");
    let (s, v) = call(&app, "PATCH", &format!("/api/board/{id}"), "owner", json!({"status":"backlog", "depends_on":[local_id]})).await;
    assert_eq!(s, StatusCode::OK, "{v}");

    // Internal mutations use the same invariant, not only the HTTP handlers.
    // Seed a legacy edge by SQL to prove unrelated evidence edits and removal
    // remain possible, without introducing a production escape hatch.
    use amux_server::db::board_store as bs;
    let (id, local_id, peer_id) = (id.to_owned(), local_id.to_owned(), peer_id.to_owned());
    store.write(move |conn| {
        let (id, local_id, peer_id) = (id.as_str(), local_id.as_str(), peer_id.as_str());
        let new = bs::NewIssue {
            title:"Internal foreign dependency".into(), desc:String::new(), status:"backlog".into(),
            session:Some("owner".into()), item_type:"chore".into(), creator:"owner".into(), owner_type:"agent".into(),
            due:None, due_time:None, reviewer:None, shepherd:None, gate:vec![], depends_on:vec![peer_id.into()], tags:vec![],
            next_action:None, acceptance_criteria:None, ask_type:None, ask_question:None, ask_unblocks:None, ask_actor:None,
            source:None, requested_by:None, callback_session:None, callback_prompt:None,
        };
        let count_before: i64 = conn.query_row("SELECT count(*) FROM issues", [], |r| r.get(0)).unwrap();
        assert!(bs::create_issue(conn, &new, 1).is_err());
        let count_after: i64 = conn.query_row("SELECT count(*) FROM issues", [], |r| r.get(0)).unwrap();
        assert_eq!(count_before, count_after);
        let mut input = bs::get_issue(conn, local_id).unwrap().unwrap();
        input.session = Some("peer".into());
        assert!(bs::save_patched(conn, &mut input).is_err());
        let mut child = bs::get_issue(conn, id).unwrap().unwrap();
        child.depends_on = vec![peer_id.into()];
        assert!(bs::save_patched(conn, &mut child).is_err());
        conn.execute("UPDATE issues SET depends_on=?1 WHERE id=?2", rusqlite::params![json!([peer_id]).to_string(), id]).unwrap();
        let mut legacy = bs::get_issue(conn, id).unwrap().unwrap();
        legacy.next_action = Some("Verify the local implementation".into());
        bs::save_patched(conn, &mut legacy).unwrap();
        conn.execute("UPDATE issues SET status='verified' WHERE id=?1", [id]).unwrap();
        legacy.status = "todo".into();
        assert!(bs::save_patched(conn, &mut legacy).is_err(), "reopening cannot reactivate a historical cross-board wait");
        legacy.depends_on.clear();
        bs::save_patched(conn, &mut legacy).unwrap();
        // Missing and ownerless references are not an exemption. Ownerless cards
        // may depend on other ownerless cards, but cannot cross into a worker board.
        conn.execute("UPDATE issues SET session=NULL WHERE id=?1", [peer_id]).unwrap();
        assert_eq!(bs::foreign_dependencies(conn, &bs::BoardOwner::new(None, Some("owner")), &[peer_id.into()]).unwrap().len(), 1);
        assert_eq!(bs::foreign_dependencies(conn, &bs::BoardOwner::new(None, None), &[local_id.into()]).unwrap().len(), 1);
        assert!(bs::foreign_dependencies(conn, &bs::BoardOwner::new(None, None), &[peer_id.into()]).unwrap().is_empty());
        assert_eq!(bs::foreign_dependencies(conn, &bs::BoardOwner::new(None, None), &["MISSING-999".into()]).unwrap().len(), 1);
        Ok(amux_server::db::WriteOutcome { applied: true, events: vec![] })
    }).unwrap();
}
