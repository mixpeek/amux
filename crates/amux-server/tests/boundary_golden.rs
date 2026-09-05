//! GOLDEN tests for the AMUX-2597 nativized families (/api/fs, /api/ls,
//! /api/autocomplete/dir, /api/groups error shapes): the expected bodies are
//! RECORDED live Python responses (tests/fixtures/boundary/live_recorded.json,
//! captured 2026-08-09 against build d5996d11556c with GET-only probes), not
//! hand-written JSON — so a shape drift from the Python contract fails
//! against what Python actually said, not against a paraphrase of it.
//!
//! The recording was made against a scratch tree; this test REBUILDS that
//! exact tree in a tempdir and rewrites the recorded root into the temp
//! root, so it is hermetic and CI-safe. Volatile fields are normalized on
//! BOTH sides identically: `modified` timestamps and `elapsed_ms` dropped,
//! directory sizes zeroed (filesystem-dependent), search results sorted by
//! (path, line) because rg's parallel output order is nondeterministic.
//! Home-relative deny cases rewrite the recorded home to $HOME (the rule is
//! home-relative on both origins).
//!
//! The live-fleet-dependent groups cases (groups_dashboard, scoped variants)
//! were exercised by tests/boundary_live_oracle.rs, deleted with the Python
//! server (2026-08-09): once 8822 was answered by Rust, that oracle diffed
//! Rust against itself — a check that could not fail (ethos rule 7). These
//! RECORDED goldens are the surviving contract memory; here the
//! fleet-independent parts are pinned: the contract strings, the config-miss
//! default shape, and the 404 shapes.

use amux_server::api::{router, AppState};
use amux_server::db::Store;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt;

const FIXTURE: &str = include_str!("fixtures/boundary/live_recorded.json");

fn build_tree(root: &std::path::Path) {
    std::fs::create_dir_all(root.join("sub")).unwrap();
    std::fs::write(root.join("a.txt"), "hello world\nsecond line with needle here\n").unwrap();
    std::fs::write(root.join("bin.dat"), b"h\xc3\xa9llo binary\x00tail").unwrap();
    std::fs::write(root.join("sub/nested.md"), "needle in sub\n").unwrap();
    std::fs::write(root.join("utf8.txt"), "café\n").unwrap();
    std::fs::write(root.join(".hidden.txt"), "ignored needle\n").unwrap();
    std::os::unix::fs::symlink("/nonexistent-target-xyz", root.join("broken-link")).unwrap();
}

fn urlenc(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// Normalize a body for comparison; `roots` maps path prefixes (recorded or
/// local) to placeholder tokens.
fn normalize(v: &Value, roots: &[(String, String)], case: &str) -> Value {
    fn subst(s: &str, roots: &[(String, String)]) -> String {
        let mut out = s.to_string();
        for (from, to) in roots {
            out = out.replace(from, to);
        }
        out
    }
    match v {
        Value::String(s) => Value::String(subst(s, roots)),
        Value::Array(items) => {
            let mut arr: Vec<Value> =
                items.iter().map(|i| normalize(i, roots, case)).collect();
            // Search results: rg's parallel order is nondeterministic.
            if arr.iter().all(|i| i.get("line").is_some() && i.get("path").is_some()) && !arr.is_empty()
            {
                arr.sort_by_key(|i| {
                    (
                        i["path"].as_str().unwrap_or("").to_string(),
                        i["line"].as_i64().unwrap_or(0),
                    )
                });
            }
            Value::Array(arr)
        }
        Value::Object(map) => {
            let mut out = serde_json::Map::new();
            let is_dir_entry = map.get("dir").and_then(|d| d.as_bool()) == Some(true);
            for (k, val) in map {
                match k.as_str() {
                    // Timestamps differ between recording and rebuild.
                    "modified" | "elapsed_ms" => continue,
                    // Directory byte-size is filesystem trivia.
                    "size" if is_dir_entry => {
                        out.insert(k.clone(), Value::from(0));
                        continue;
                    }
                    // A truncated search keeps whichever match rg emitted
                    // first — compare the COUNT, not the pick.
                    "results" if case == "fs_search_limit" => {
                        out.insert(
                            k.clone(),
                            Value::from(val.as_array().map(|a| a.len()).unwrap_or(0)),
                        );
                        continue;
                    }
                    "files" if case == "fs_search_limit" => continue,
                    _ => {}
                }
                out.insert(k.clone(), normalize(val, roots, case));
            }
            Value::Object(out)
        }
        other => other.clone(),
    }
}

fn have_rg() -> bool {
    std::env::var("PATH")
        .unwrap_or_default()
        .split(':')
        .any(|d| std::path::Path::new(d).join("rg").is_file())
}

/// One test fn: it mutates process env (AMUX_HOME) for the groups half.
#[tokio::test]
async fn native_output_matches_recorded_python_fixtures() {
    let fixture: Value = serde_json::from_str(FIXTURE).unwrap();
    let recorded_root = fixture["root"].as_str().unwrap().to_string();
    let recorded_parent = std::path::Path::new(&recorded_root)
        .parent()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    // Recorded deny cases point at the recording machine's home.
    let recorded_home = "/Users/ethan";
    let local_home = std::env::var("HOME").unwrap();

    let td = tempfile::tempdir().unwrap();
    // Canonicalize: /var/folders symlinks to /private/var on macOS, and the
    // native handlers resolve paths, so the ROOT string in responses is the
    // resolved one.
    let root = td.path().canonicalize().unwrap().join("fstree");
    build_tree(&root);
    let root_s = root.to_string_lossy().into_owned();
    let parent_s = root.parent().unwrap().to_string_lossy().into_owned();

    // Hermetic fleet home so /api/groups never reads the real ~/.amux here.
    let fleet = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(fleet.path().join("sessions")).unwrap();
    std::env::set_var("AMUX_HOME", fleet.path());

    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("t.db")).unwrap();
    let app = router(AppState {
        store: std::sync::Arc::new(store),
        started: std::time::Instant::now(),
        build_hash: "test".into(),
        auth_token: None,
    reconciled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
    });

    // Placeholder mapping applied to BOTH sides.
    let recorded_roots = vec![
        (recorded_root.clone(), "<ROOT>".to_string()),
        (recorded_parent.clone(), "<PARENT>".to_string()),
        (recorded_home.to_string(), "<HOME>".to_string()),
    ];
    let local_roots = vec![
        (root_s.clone(), "<ROOT>".to_string()),
        (parent_s.clone(), "<PARENT>".to_string()),
        (local_home.clone(), "<HOME>".to_string()),
    ];

    let cases = fixture["cases"].as_object().unwrap();
    let rg = have_rg();
    let mut compared = 0;
    let mut skipped: Vec<&str> = vec![];
    for (name, case) in cases {
        // Live-fleet-dependent cases belong to the live oracle.
        if matches!(
            name.as_str(),
            "groups_dashboard"
                | "tags_dashboard"
                | "groups_config_first_live"
                | "groups_config_gtm"
                | "groups_scoped_tagged"
                | "groups_scoped_untagged"
                | "groups_scoped_unknown"
                | "groups_scoped_worker_hdr"
        ) {
            skipped.push(name);
            continue;
        }
        if name.starts_with("fs_search") && !rg {
            // LOUD skip: without rg the native engine answers "engine:
            // none", which is a real contract case but not THIS fixture.
            println!("SKIP {name}: rg not on PATH");
            skipped.push(name);
            continue;
        }
        // Rewrite the recorded machine's paths in the URL to this run's —
        // both encodings: the recording used quote(safe='/') for the fs
        // cases (literal slashes) and quote(safe='') for the ls ones.
        let url = case["url"]
            .as_str()
            .unwrap()
            .replace(&urlenc(&recorded_root), &urlenc(&root_s))
            .replace(&recorded_root, &root_s)
            .replace(&urlenc(&format!("{recorded_home}/.ssh")), &urlenc(&format!("{local_home}/.ssh")))
            .replace(&format!("{recorded_home}/.ssh"), &format!("{local_home}/.ssh"));
        let res = app
            .clone()
            .oneshot(Request::builder().uri(&url).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = res.status().as_u16();
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);

        let want_status = case["status"].as_u64().unwrap() as u16;
        let want = normalize(&case["body"], &recorded_roots, name);
        let got = normalize(&body, &local_roots, name);
        assert_eq!(status, want_status, "{name}: {url} -> {body}");
        assert_eq!(got, want, "{name}: native body diverges from recorded python");
        compared += 1;
    }
    assert!(
        compared >= 20,
        "fixture erosion: only {compared} cases compared (skipped: {skipped:?})"
    );

    // The groups CONTRACT strings must be byte-identical to what the live
    // Python server emitted, independent of fleet composition.
    let res = app
        .clone()
        .oneshot(Request::builder().uri("/api/groups").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
    let native: Value = serde_json::from_slice(&bytes).unwrap();
    let recorded = &fixture["cases"]["groups_dashboard"]["body"];
    for key in ["derived_from", "set_on_a_worker", "configure_group"] {
        assert_eq!(native[key], recorded[key], "groups contract string {key}");
    }

    std::env::remove_var("AMUX_HOME");
}
