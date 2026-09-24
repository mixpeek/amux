//! Syntax highlighting for peek history, rendered to ANSI on the server.
//!
//! Ethan 2026-09-24: "we should have proper syntax highlighting ... whatever is
//! the fastest/low latency approach ... see claude code". The dashboard already
//! turns ANSI SGR (including 24-bit `38;2`/`48;2`) into coloured spans, so
//! colouring here costs the phone nothing: no highlighter to download, no
//! per-frame tokenising in the browser, and the result is memoised with the
//! transcript it belongs to.
//!
//! Two shapes, both the ones Claude Code draws in its own terminal:
//! - an Edit's `structuredPatch`: numbered hunk lines on red/green rows;
//! - a fenced code block in prose.
//!
//! Nothing here drops text. Highlighting only adds colour; when a block is too
//! large to colour within budget it is emitted plain, in full.

use std::sync::OnceLock;
use syntect::easy::HighlightLines;
use syntect::highlighting::{Color, Theme, ThemeSet};
use syntect::parsing::{SyntaxReference, SyntaxSet};

/// Colour at most this many lines per block. Past it the block is printed
/// plain (never truncated): a 20k-line generated file must not stall a peek.
const MAX_HIGHLIGHT_LINES: usize = 4_000;

fn syntaxes() -> &'static SyntaxSet {
    static SS: OnceLock<SyntaxSet> = OnceLock::new();
    SS.get_or_init(SyntaxSet::load_defaults_newlines)
}

fn theme() -> &'static Theme {
    static TH: OnceLock<Theme> = OnceLock::new();
    TH.get_or_init(|| {
        let mut set = ThemeSet::load_defaults();
        set.themes
            .remove("base16-ocean.dark")
            .or_else(|| set.themes.into_values().next())
            .unwrap_or_default()
    })
}

/// The syntax for a file path (by extension, then by file name).
pub(crate) fn syntax_for_path(path: &str) -> Option<&'static SyntaxReference> {
    let ss = syntaxes();
    let file = path.rsplit('/').next().unwrap_or(path);
    let ext = file.rsplit_once('.').map(|(_, e)| e).unwrap_or(file);
    ss.find_syntax_by_extension(ext)
        .or_else(|| ss.find_syntax_by_extension(file))
        .filter(|s| s.name != "Plain Text")
}

/// The syntax for a fenced block's info string (`rust`, `ts`, `bash` ...).
pub(crate) fn syntax_for_token(lang: &str) -> Option<&'static SyntaxReference> {
    let lang = lang.split_whitespace().next().unwrap_or("");
    if lang.is_empty() {
        return None;
    }
    let ss = syntaxes();
    let alias = match lang.to_ascii_lowercase().as_str() {
        "ts" | "typescript" | "tsx" | "mjs" | "cjs" | "jsx" => "js",
        "shell" | "zsh" | "console" => "sh",
        "py" => "py",
        "yml" => "yaml",
        other => return ss.find_syntax_by_token(other).filter(|s| s.name != "Plain Text"),
    };
    ss.find_syntax_by_token(alias).filter(|s| s.name != "Plain Text")
}

fn fg(c: Color) -> String {
    format!("\x1b[38;2;{};{};{}m", c.r, c.g, c.b)
}

/// Highlight `lines` as one continuous block (parser state carries across
/// lines, so a multi-line string or comment colours correctly). Returns one
/// ANSI string per input line, WITHOUT a trailing reset, so a caller can put
/// a background under it. `None` when the block is over budget or the parser
/// errs; the caller then prints the lines plain.
pub(crate) fn highlight_block(lines: &[&str], syntax: &SyntaxReference) -> Option<Vec<String>> {
    if lines.len() > MAX_HIGHLIGHT_LINES {
        return None;
    }
    let ss = syntaxes();
    let mut h = HighlightLines::new(syntax, theme());
    let mut out = Vec::with_capacity(lines.len());
    for line in lines {
        let with_nl = format!("{line}\n");
        let ranges = h.highlight_line(&with_nl, ss).ok()?;
        let mut s = String::with_capacity(line.len() + 32);
        for (style, text) in ranges {
            let text = text.trim_end_matches('\n');
            if text.is_empty() {
                continue;
            }
            s.push_str(&fg(style.foreground));
            s.push_str(text);
        }
        out.push(s);
    }
    Some(out)
}

const ADD_BG: &str = "\x1b[48;2;22;58;32m";
const DEL_BG: &str = "\x1b[48;2;78;22;28m";
const DIM: &str = "\x1b[38;5;246m";
const RESET: &str = "\x1b[0m";

/// An Edit/MultiEdit/Write result as Claude Code draws it: a summary line,
/// then every hunk line with its line number, removed rows on red and added
/// rows on green, code coloured by the file's syntax. `patch` is the
/// `toolUseResult.structuredPatch` array. Returns `None` if it is not one.
pub(crate) fn render_structured_patch(file_path: &str, patch: &serde_json::Value) -> Option<Vec<String>> {
    let hunks = patch.as_array().filter(|a| !a.is_empty())?;
    let (mut added, mut removed) = (0usize, 0usize);
    let mut maxno = 1i64;
    for h in hunks {
        let os = h["oldStart"].as_i64().unwrap_or(1);
        let ns = h["newStart"].as_i64().unwrap_or(1);
        let ol = h["oldLines"].as_i64().unwrap_or(0);
        let nl = h["newLines"].as_i64().unwrap_or(0);
        maxno = maxno.max(os + ol).max(ns + nl);
        for l in h["lines"].as_array()? {
            match l.as_str().unwrap_or("").chars().next() {
                Some('+') => added += 1,
                Some('-') => removed += 1,
                _ => {}
            }
        }
    }
    let width = maxno.to_string().len();
    let plural = |n: usize, w: &str| format!("{n} {w}{}", if n == 1 { "" } else { "s" });
    let summary = match (added, removed) {
        (a, 0) => format!("Added {}", plural(a, "line")),
        (0, r) => format!("Removed {}", plural(r, "line")),
        (a, r) => format!("Added {}, removed {}", plural(a, "line"), plural(r, "line")),
    };
    let mut out = vec![format!("{DIM}  \u{23bf}  {summary}{RESET}")];
    let syntax = syntax_for_path(file_path);
    for (hi, h) in hunks.iter().enumerate() {
        if hi > 0 {
            out.push(format!("{DIM}     {:>width$} \u{2026}{RESET}", ""));
        }
        let lines: Vec<&str> = h["lines"].as_array()?.iter().map(|l| l.as_str().unwrap_or("")).collect();
        // Colour the CODE (after the one-character +/-/space marker) as one
        // block, so strings and comments that span lines colour correctly.
        let code: Vec<&str> = lines.iter().map(|l| l.get(1..).unwrap_or("")).collect();
        let coloured = syntax.and_then(|s| highlight_block(&code, s));
        let (mut old_no, mut new_no) = (
            h["oldStart"].as_i64().unwrap_or(1),
            h["newStart"].as_i64().unwrap_or(1),
        );
        for (i, l) in lines.iter().enumerate() {
            let mark = l.chars().next().unwrap_or(' ');
            let body = coloured
                .as_ref()
                .map(|c| c[i].clone())
                .unwrap_or_else(|| format!("\x1b[39m{}", code[i]));
            let (no, bg, sign) = match mark {
                '+' => {
                    let n = new_no;
                    new_no += 1;
                    (n, ADD_BG, '+')
                }
                '-' => {
                    let n = old_no;
                    old_no += 1;
                    (n, DEL_BG, '-')
                }
                _ => {
                    let n = new_no;
                    old_no += 1;
                    new_no += 1;
                    (n, "", ' ')
                }
            };
            out.push(format!("     {bg}{DIM}{no:>width$} {sign}\x1b[39m {body}{RESET}"));
        }
    }
    Some(out)
}

/// A Write's full content, numbered and coloured (Claude Code shows the file it
/// wrote). Every line is printed.
pub(crate) fn render_written_file(file_path: &str, content: &str) -> Vec<String> {
    let lines: Vec<&str> = content.split('\n').collect();
    let lines: &[&str] = if lines.last() == Some(&"") { &lines[..lines.len() - 1] } else { &lines };
    let width = lines.len().max(1).to_string().len();
    let coloured = syntax_for_path(file_path).and_then(|s| highlight_block(lines, s));
    let mut out = vec![format!("{DIM}  \u{23bf}  Wrote {} line{}{RESET}", lines.len(), if lines.len() == 1 { "" } else { "s" })];
    for (i, l) in lines.iter().enumerate() {
        let body = coloured.as_ref().map(|c| c[i].clone()).unwrap_or_else(|| format!("\x1b[39m{l}"));
        out.push(format!("     {DIM}{:>width$}\x1b[39m {body}{RESET}", i + 1));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strip(s: &str) -> String {
        regex::Regex::new(r"\x1b\[[0-9;]*m").unwrap().replace_all(s, "").into_owned()
    }

    #[test]
    fn a_patch_renders_numbered_coloured_rows_and_keeps_every_line() {
        let patch = serde_json::json!([{
            "oldStart": 74, "oldLines": 2, "newStart": 74, "newLines": 3,
            "lines": ["-  // Start carol", "+  // Start carol and wait", "+  let x = \"s\";", "   await start();"]
        }]);
        let out = render_structured_patch("e2e/chaos/x.rs", &patch).expect("a patch");
        let plain: Vec<String> = out.iter().map(|l| strip(l)).collect();
        assert_eq!(plain[0].trim(), "\u{23bf}  Added 2 lines, removed 1 line");
        assert!(plain[1].contains("74 -   // Start carol"), "{plain:?}");
        assert!(plain[2].contains("74 +   // Start carol and wait"), "{plain:?}");
        assert!(plain[3].contains("75 +   let x = \"s\";"), "{plain:?}");
        assert!(plain[4].contains("76     await start();"), "{plain:?}");
        assert!(out[1].contains(DEL_BG) && out[2].contains(ADD_BG), "rows are backed red/green");
        assert!(out[3].contains("\x1b[38;2;"), "code is syntax-coloured: {:?}", out[3]);
    }

    #[test]
    fn an_unknown_language_is_printed_plain_and_whole() {
        let out = render_written_file("notes.unknownext", "alpha\nbeta\n");
        let plain: Vec<String> = out.iter().map(|l| strip(l)).collect();
        assert!(plain[1].ends_with("1 alpha") && plain[2].ends_with("2 beta"), "{plain:?}");
    }

    #[test]
    fn fenced_languages_resolve() {
        for l in ["rust", "js", "ts", "python", "bash", "json", "yaml", "sql", "go"] {
            assert!(syntax_for_token(l).is_some(), "{l}");
        }
        assert!(syntax_for_token("").is_none());
    }
}
