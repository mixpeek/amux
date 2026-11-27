// AMUX-3454: stamp the commit the binary was built from, so /health can
// answer "does the running server contain commit X" directly. `build` (a
// content hash) discriminates binaries but not commits, and the AF-82
// time-comparison fails when two commits land seconds apart — which cost two
// wasted verification rounds in one afternoon (a365cd50 read as containing
// 475d74a while it was dec6eaa's build).
//
// Honesty caveats, both deliberate:
// - The local builder compiles the WORKING TREE, not the commit object, so a
//   dirty tree gets a `-dirty` suffix — without it, "the build contains my
//   commit" could confidently mislead when uncommitted edits rode along.
// - Outside a git checkout (the cloud image builds from a COPY without .git)
//   this fails SOFT to "unknown": presence/absence of .git drives the
//   difference, never a build flag (single-codebase rule).
fn main() {
    api_catalog();
    let root = std::path::PathBuf::from(
        std::env::var("CARGO_MANIFEST_DIR").expect("server manifest directory"),
    )
    .join("../..");
    println!("cargo:rerun-if-changed={}", root.join("crates").display());
    println!(
        "cargo:rerun-if-changed={}",
        root.join("Cargo.toml").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        root.join("Cargo.lock").display()
    );
    let git = |args: &[&str]| -> Option<String> {
        // Pathspecs below are repository-relative. Running from this crate's
        // directory silently inspected nonexistent crates/crates and called
        // a dirty binary clean.
        let o = std::process::Command::new("git")
            .arg("-C")
            .arg(&root)
            .args(args)
            .output()
            .ok()?;
        o.status
            .success()
            .then(|| String::from_utf8_lossy(&o.stdout).trim().to_string())
    };
    // Linked worktrees store .git as a FILE. Resolve metadata through Git;
    // watching root/.git/HEAD there tells Cargo to rebuild forever because that
    // path cannot exist. Only the current branch can change this build's SHA.
    if let Some(head) = git(&["rev-parse", "--git-path", "HEAD"]) {
        println!("cargo:rerun-if-changed={}", root.join(head).display());
        if let Some(reference) = git(&["symbolic-ref", "-q", "HEAD"]) {
            if let Some(path) = git(&["rev-parse", "--git-path", &reference]) {
                let mut path = root.join(path);
                // A packed ref has no loose file yet. Watch its existing parent
                // so a later loose update invalidates the cached commit identity.
                while !path.exists() && path.pop() {}
                println!("cargo:rerun-if-changed={}", path.display());
            }
            if let Some(packed) = git(&["rev-parse", "--git-path", "packed-refs"]) {
                let packed = root.join(packed);
                // When absent, packing deletes the watched loose ref; that
                // already reruns this script and installs the packed-ref watch.
                if packed.exists() {
                    println!("cargo:rerun-if-changed={}", packed.display());
                }
            }
        }
    } else {
        println!("cargo:warning=build_git_metadata_unavailable measured=false n_considered=0 identity=unknown");
    }
    // The image build (cloud/docker/Dockerfile) has no .git; CI passes the commit it checked
    // out as AMUX_SOURCE_COMMIT so a customer server's /health can name its revision.
    println!("cargo:rerun-if-env-changed=AMUX_SOURCE_COMMIT");
    let source_commit = std::env::var("AMUX_SOURCE_COMMIT")
        .ok()
        .filter(|c| c.len() >= 12 && c.chars().all(|ch| ch.is_ascii_hexdigit()));
    let mut sha = git(&["rev-parse", "--short=12", "HEAD"]).unwrap_or_default();
    if sha.is_empty() {
        sha = source_commit
            .as_ref()
            .map(|c| c[..12].to_string())
            .unwrap_or_else(|| "unknown".into());
    } else if git(&[
        "status",
        "--porcelain",
        "--",
        "crates",
        "Cargo.toml",
        "Cargo.lock",
    ])
    .map(|s| !s.is_empty())
    .unwrap_or(false)
    {
        sha.push_str("-dirty");
    }
    let mut full = git(&["rev-parse", "HEAD"])
        .or(source_commit)
        .unwrap_or_else(|| "unknown".into());
    if sha.ends_with("-dirty") {
        full.push_str("-dirty");
    }
    println!("cargo:rustc-env=AMUX_BUILD_COMMIT_FULL={full}");
    println!("cargo:rustc-env=AMUX_BUILD_COMMIT={sha}");
}


// The API catalog (owner, 2026-11-27: "make sure all amux capabilities/apis/
// features are discoverable via api and mcp. We should be able to point an
// agent to amux and it should be able to figure everything out"). Every API
// module already opens with a `//!` doc naming its routes, bodies and refusal
// codes; this reads those docs and which routes each module defines, so
// /api/discover and the MCP server serve the code's own words. No second copy
// to drift (A10). Output: $OUT_DIR/api_catalog.rs with MODULE_DOCS and
// ROUTE_MODULES (normalized path, module).
fn api_catalog() {
    let src_dir = std::path::PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("manifest dir")).join("src");
    let api = src_dir.join("api");
    println!("cargo:rerun-if-changed={}", src_dir.display());
    let mut files = Vec::new();
    collect_rs(&src_dir, &mut files);
    files.sort();
    let mod_src = std::fs::read_to_string(api.join("mod.rs")).unwrap_or_default();
    let nests = nest_prefixes(&mod_src);
    // A module's name: relative to src/api for the API (`crm_agent`), relative
    // to src otherwise (`push`, `integrations::at_rest`).
    let name_of = |f: &std::path::Path| -> String {
        let (root, _) = if f.starts_with(&api) { (&api, ()) } else { (&src_dir, ()) };
        let rel = f.strip_prefix(root).unwrap_or(f).with_extension("");
        let parts: Vec<String> = rel.components().map(|c| c.as_os_str().to_string_lossy().to_string()).collect();
        let m = if parts.last().map(String::as_str) == Some("mod") { parts[..parts.len() - 1].join("::") } else { parts.join("::") };
        // api/mod.rs is the router itself: name it, or its own routes are lost.
        if m.is_empty() { "api".to_string() } else { m }
    };
    let known: std::collections::BTreeMap<String, String> = files.iter().map(|f| {
        let m = name_of(f);
        (m.rsplit("::").next().unwrap_or(&m).to_string(), m)
    }).collect();
    let mut docs: Vec<(String, String)> = Vec::new();
    let mut routes: Vec<(String, String)> = Vec::new();
    for f in &files {
        let module = name_of(f);
        if module == "main" {
            continue;
        }
        let src = std::fs::read_to_string(f).unwrap_or_default();
        let doc: Vec<&str> = src.lines().take_while(|l| l.starts_with("//!")).map(|l| l.trim_start_matches("//!").strip_prefix(' ').unwrap_or(l.trim_start_matches("//!"))).collect();
        if !doc.is_empty() {
            docs.push((module.clone(), doc.join("\n")));
        }
        let base = module.rsplit("::").next().unwrap_or(&module).to_string();
        let top = module.split("::").next().unwrap_or(&module).to_string();
        for (fn_name, path, handler) in route_literals(&src) {
            let full = if path.starts_with("/api") || path.starts_with("/.well-known") || path.starts_with("/mcp") || path.starts_with("/oauth")
                || path.starts_with("/health") || path.starts_with("/llms") || path.starts_with("/pub") || path.starts_with("/f/")
                || path.starts_with("/s/") || path.starts_with("/invite") || path.starts_with("/manifest") || path.starts_with("/pages") {
                Some(path.clone())
            } else {
                // The prefix its router function is nested under; a module nested
                // once (whose router is built in a helper such as `routes_with`)
                // takes that one prefix.
                let mine: Vec<&(String, String, String)> = nests.iter().filter(|(m, _, _)| m == &base || m == &top).collect();
                mine.iter().find(|(_, f2, _)| f2 == &fn_name).or_else(|| if mine.len() == 1 { mine.first() } else { None })
                    .map(|(_, _, pre)| if path == "/" { pre.clone() } else { format!("{pre}{path}") })
            };
            let Some(full) = full else { continue };
            // A route declared in a router file but served by another module's
            // handler (`get(sessions_legacy::task_history)`) belongs to that module.
            let owners: Vec<String> = idents_before_paths(&handler).into_iter().filter_map(|i| known.get(&i).cloned()).filter(|m| m != &module).collect();
            if owners.is_empty() {
                routes.push((norm_path(&full), module.clone()));
            } else {
                for o in owners {
                    routes.push((norm_path(&full), o));
                }
            }
        }
    }
    routes.sort();
    routes.dedup();
    let mut out = String::from("// Generated by build.rs (api_catalog). Do not edit.\n");
    out.push_str("pub static MODULE_DOCS: &[(&str, &str)] = &[\n");
    for (m, d) in &docs {
        out.push_str(&format!("    ({m:?}, {d:?}),\n"));
    }
    out.push_str("];\npub static ROUTE_MODULES: &[(&str, &str)] = &[\n");
    for (p, m) in &routes {
        out.push_str(&format!("    ({p:?}, {m:?}),\n"));
    }
    out.push_str("];\n");
    let dest = std::path::PathBuf::from(std::env::var("OUT_DIR").expect("out dir")).join("api_catalog.rs");
    let _ = std::fs::write(dest, out);
}

/// Every identifier written just before `::` in an expression (`crm_agent::approve` gives `crm_agent`).
fn idents_before_paths(expr: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut i = 0;
    while let Some(k) = expr[i..].find("::") {
        let at = i + k;
        let before: String = expr[..at].chars().rev().take_while(|c| c.is_ascii_alphanumeric() || *c == '_').collect::<Vec<_>>().into_iter().rev().collect();
        if !before.is_empty() && !out.contains(&before) {
            out.push(before);
        }
        i = at + 2;
    }
    out
}

fn collect_rs(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            collect_rs(&p, out);
        } else if p.extension().and_then(|x| x.to_str()) == Some("rs") {
            out.push(p);
        }
    }
}

/// `{id}` and `{*rest}` become `{}`, and a trailing slash goes, so a route
/// written either way matches the route table's spelling.
fn norm_path(p: &str) -> String {
    let mut out = String::new();
    let mut in_param = false;
    for c in p.chars() {
        match c {
            '{' => { in_param = true; out.push_str("{}"); }
            '}' => in_param = false,
            _ if in_param => {}
            _ => out.push(c),
        }
    }
    let t = out.trim_end_matches('/');
    if t.is_empty() { "/".into() } else { t.to_string() }
}

/// (module, function, prefix) for every `.nest("/prefix", module::function()...)` in api/mod.rs.
fn nest_prefixes(src: &str) -> Vec<(String, String, String)> {
    let mut out = Vec::new();
    let b = src.as_bytes();
    let mut i = 0;
    while let Some(k) = src[i..].find(".nest(") {
        let open = i + k + 5;
        let close = match_paren(b, open);
        let expr = &src[open + 1..close.min(src.len())];
        if let Some(q1) = expr.find('"') {
            if let Some(q2) = expr[q1 + 1..].find('"') {
                let prefix = expr[q1 + 1..q1 + 1 + q2].to_string();
                let rest = &expr[q1 + 2 + q2..];
                for (m, f) in calls(rest) {
                    out.push((m, f, prefix.clone()));
                }
            }
        }
        i = open + 1;
    }
    out
}

fn match_paren(b: &[u8], open: usize) -> usize {
    let mut depth = 0i32;
    let mut j = open;
    while j < b.len() {
        match b[j] {
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return j;
                }
            }
            _ => {}
        }
        j += 1;
    }
    b.len()
}

/// Every `ident::ident()` call in an expression, as (module, function).
fn calls(expr: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut i = 0;
    while let Some(k) = expr[i..].find("::") {
        let at = i + k;
        let before: String = expr[..at].chars().rev().take_while(|c| c.is_ascii_alphanumeric() || *c == '_').collect::<Vec<_>>().into_iter().rev().collect();
        let after: String = expr[at + 2..].chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '_').collect();
        if !before.is_empty() && !after.is_empty() && expr[at + 2 + after.len()..].starts_with("()") {
            out.push((before, after));
        }
        i = at + 2;
    }
    out
}

/// (enclosing function, path, handler expression) for every `.route("path", handler)` in a source file.
fn route_literals(src: &str) -> Vec<(String, String, String)> {
    let mut fns: Vec<(usize, String)> = Vec::new();
    let mut pos = 0;
    for line in src.split_inclusive('\n') {
        let t = line.trim_start();
        let t = t.strip_prefix("pub(crate) ").or_else(|| t.strip_prefix("pub ")).unwrap_or(t);
        let t = t.strip_prefix("async ").unwrap_or(t);
        if let Some(r) = t.strip_prefix("fn ") {
            let name: String = r.chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '_').collect();
            fns.push((pos, name));
        }
        pos += line.len();
    }
    let mut out = Vec::new();
    let mut i = 0;
    while let Some(k) = src[i..].find(".route(") {
        let at = i + k;
        let after = src[at + 7..].trim_start();
        if let Some(rest) = after.strip_prefix('"') {
            if let Some(end) = rest.find('"') {
                let f = fns.iter().rev().find(|(p, _)| *p < at).map(|(_, n)| n.clone()).unwrap_or_default();
                let close = match_paren(src.as_bytes(), at + 6);
                let handler = src.get(at + 7..close).unwrap_or("").to_string();
                out.push((f, rest[..end].to_string(), handler));
            }
        }
        i = at + 7;
    }
    out
}
