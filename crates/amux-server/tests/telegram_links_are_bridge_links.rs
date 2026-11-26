//! Telegram's chat links are `bridge_links` rows (migration 0178, primitive
//! integrity audit 2026-11-26 P1). The old separate link table may be named
//! only by the migrations that created, altered, copied and dropped it. Any
//! other mention is code reading or writing a table that no longer exists, or
//! a doc pointing at one.

use std::path::{Path, PathBuf};

const OLD_TABLE: &str = concat!("telegram", "_mappings");

fn files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let p = e.path();
        let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if p.is_dir() {
            if !matches!(name, "target" | "node_modules" | ".git" | "migrations") {
                files(&p, out);
            }
        } else if matches!(p.extension().and_then(|x| x.to_str()), Some("rs" | "js" | "ts" | "mjs" | "html" | "sh" | "md" | "toml"))
            || name == "amux"
        {
            out.push(p);
        }
    }
}

#[test]
fn nothing_outside_migrations_names_the_old_telegram_link_table() {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let roots = [
        repo.join("crates"),
        repo.join("scripts"),
        repo.join("e2e"),
        repo.join(".claude/commands"),
        repo.join("amux"),
    ];
    let mut all = Vec::new();
    for r in &roots {
        if r.is_file() {
            all.push(r.clone());
        } else {
            files(r, &mut all);
        }
    }
    assert!(all.len() > 200, "the walk found only {} files; it did not read the tree", all.len());
    let hits: Vec<String> = all
        .iter()
        .filter(|p| !p.ends_with("telegram_links_are_bridge_links.rs"))
        .filter_map(|p| {
            let text = std::fs::read_to_string(p).ok()?;
            let lines: Vec<String> = text
                .lines()
                .enumerate()
                .filter(|(_, l)| l.contains(OLD_TABLE))
                .map(|(i, _)| format!("{}:{}", p.display(), i + 1))
                .collect();
            (!lines.is_empty()).then(|| lines.join("\n"))
        })
        .collect();
    assert!(hits.is_empty(), "{OLD_TABLE} is named outside migrations:\n{}", hits.join("\n"));
    println!("PASS {} files read, none name {OLD_TABLE} outside migrations", all.len());
}
