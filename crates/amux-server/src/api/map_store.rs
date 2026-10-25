//! The Map tab's store (2026-10-20, migration 0128). One row per pin with an
//! edit history; the map's tags, settings, trials, saved lists and tag
//! categories live in `map_meta`. Handlers are in map.rs; everything here is a
//! plain function over a connection so it can be tested without a server.
//!
//! A pin is a JSON document. Fields this module understands:
//! - `status`: "" | "want" | "been" | "favorite" | "closed"
//! - `myRating`: null or 1..=5 (the owner's own rating). `rating` and
//!   `ratingCount` are Google's, written by search since before this store,
//!   and are kept as sent.
//! - `visits`: {count, first, last} matched from location history
//! - `placeId` + `place`: the linked Google place and its last refresh
//! - `eventAt` + `calEventId`: a dated pin and its calendar event
//!
//! Everything else (name, desc, notes, tags, photoName, ...) is kept as sent.
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Map, Value};
use std::path::Path;

pub const META_KEYS: &[&str] = &["tags", "settings", "trials", "lists", "tagGroups"];
pub const STATUSES: &[&str] = &["", "want", "been", "favorite", "closed"];
/// A phone-reported visit counts for a pin within this distance.
pub const VISIT_RADIUS_M: f64 = 60.0;
/// ... and only if it lasted at least this long (a walk past is not a visit).
pub const VISIT_MIN_S: f64 = 600.0;
/// Two pins this close with similar names are offered as duplicates.
pub const DUP_RADIUS_M: f64 = 40.0;

#[derive(Debug)]
pub enum MapError {
    Bad(String),
    Conflict(Value),
    NotFound(String),
    Db(rusqlite::Error),
}

impl From<rusqlite::Error> for MapError {
    fn from(e: rusqlite::Error) -> Self {
        MapError::Db(e)
    }
}

impl std::fmt::Display for MapError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MapError::Bad(m) | MapError::NotFound(m) => write!(f, "{m}"),
            MapError::Conflict(v) => write!(f, "conflict: {v}"),
            MapError::Db(e) => write!(f, "{e}"),
        }
    }
}

pub type R<T> = Result<T, MapError>;

pub fn now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

fn iso(ts: f64) -> String {
    chrono::DateTime::from_timestamp(ts as i64, 0)
        .map(|d| d.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
        .unwrap_or_default()
}

/// Great-circle distance in meters.
pub fn dist_m(lat1: f64, lng1: f64, lat2: f64, lng2: f64) -> f64 {
    let (p1, p2) = (lat1.to_radians(), lat2.to_radians());
    let dp = (lat2 - lat1).to_radians();
    let dl = (lng2 - lng1).to_radians();
    let a = (dp / 2.0).sin().powi(2) + p1.cos() * p2.cos() * (dl / 2.0).sin().powi(2);
    6_371_000.0 * 2.0 * a.sqrt().asin()
}

/// A number sent as a number or a numeric string (the old SPA sent both).
fn num(v: Option<&Value>) -> Option<f64> {
    match v? {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
    .filter(|x: &f64| x.is_finite())
}

pub fn latlng(p: &Value) -> Option<(f64, f64)> {
    Some((num(p.get("lat"))?, num(p.get("lng"))?))
}

fn s<'a>(p: &'a Value, k: &str) -> &'a str {
    p.get(k).and_then(Value::as_str).unwrap_or("")
}

/// Lowercased words, for name matching.
fn words(name: &str) -> Vec<String> {
    name.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty() && !matches!(*w, "the" | "a" | "and" | "nyc" | "ny" | "cafe" | "café" | "restaurant" | "bar"))
        .map(str::to_string)
        .collect()
}

/// 0..1: how alike two place names are. Containment counts as a strong match
/// ("Joe's Pizza" vs "Joe's Pizza Broadway").
pub fn name_similarity(a: &str, b: &str) -> f64 {
    let (wa, wb) = (words(a), words(b));
    if wa.is_empty() || wb.is_empty() {
        return 0.0;
    }
    let (ja, jb) = (wa.join(" "), wb.join(" "));
    if ja == jb {
        return 1.0;
    }
    if (ja.len() >= 4 && jb.contains(&ja)) || (jb.len() >= 4 && ja.contains(&jb)) {
        return 0.9;
    }
    let inter = wa.iter().filter(|w| wb.contains(w)).count() as f64;
    let union = (wa.len() + wb.len()) as f64 - inter;
    if union == 0.0 { 0.0 } else { inter / union }
}

// ---------------------------------------------------------------- meta + rows

pub fn get_meta(c: &Connection, key: &str) -> R<Option<Value>> {
    let v: Option<String> = c
        .query_row("SELECT value FROM map_meta WHERE key=?1", [key], |r| r.get(0))
        .optional()?;
    Ok(v.and_then(|t| serde_json::from_str(&t).ok()))
}

pub fn set_meta(c: &Connection, key: &str, v: &Value) -> R<()> {
    // The server adds the Google key to settings on every read; older
    // dashboards saved it back into map.json. It is never stored here.
    let mut v = v.clone();
    if key == "settings" {
        if let Some(o) = v.as_object_mut() {
            o.remove("googleMapsKey");
        }
    }
    let v = &v;
    c.execute(
        "INSERT INTO map_meta(key,value,updated_at) VALUES(?1,?2,?3)
         ON CONFLICT(key) DO UPDATE SET value=excluded.value, updated_at=excluded.updated_at",
        params![key, v.to_string(), now()],
    )?;
    Ok(())
}

/// Every pin in creation order, each with its `rev`.
pub fn all_pins(c: &Connection) -> R<Vec<Value>> {
    let mut st = c.prepare_cached("SELECT doc, rev FROM map_pins ORDER BY created_at, rowid")?;
    let rows = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
    let mut out = Vec::new();
    for row in rows {
        let (doc, rev) = row?;
        if let Ok(mut v) = serde_json::from_str::<Value>(&doc) {
            v["rev"] = json!(rev);
            out.push(v);
        }
    }
    Ok(out)
}

pub fn get_pin(c: &Connection, id: &str) -> R<Option<(Value, i64)>> {
    let row: Option<(String, i64)> = c
        .query_row("SELECT doc, rev FROM map_pins WHERE id=?1", [id], |r| Ok((r.get(0)?, r.get(1)?)))
        .optional()?;
    Ok(row.and_then(|(d, rev)| serde_json::from_str(&d).ok().map(|v| (v, rev))))
}

fn record(c: &Connection, pin_id: &str, actor: &str, action: &str, before: Option<&Value>, after: Option<&Value>) -> R<()> {
    c.execute(
        "INSERT INTO map_pin_history(pin_id,at,actor,action,before,after) VALUES(?1,?2,?3,?4,?5,?6)",
        params![pin_id, now(), actor, action, before.map(Value::to_string), after.map(Value::to_string)],
    )?;
    Ok(())
}

pub fn history(c: &Connection, id: &str) -> R<Vec<Value>> {
    let mut st = c.prepare_cached(
        "SELECT seq, at, actor, action, before, after FROM map_pin_history WHERE pin_id=?1 ORDER BY seq DESC LIMIT 200",
    )?;
    let rows = st.query_map([id], |r| {
        Ok(json!({
            "seq": r.get::<_, i64>(0)?, "at": iso(r.get::<_, f64>(1)?), "actor": r.get::<_, String>(2)?,
            "action": r.get::<_, String>(3)?,
            "before": r.get::<_, Option<String>>(4)?.and_then(|t| serde_json::from_str::<Value>(&t).ok()),
            "after": r.get::<_, Option<String>>(5)?.and_then(|t| serde_json::from_str::<Value>(&t).ok()),
        }))
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// The whole map in the shape the SPA has always loaded.
pub fn doc(c: &Connection) -> R<Value> {
    let mut d = json!({ "pins": all_pins(c)? });
    for k in META_KEYS {
        let dflt = if *k == "settings" { json!({}) } else { json!([]) };
        d[*k] = get_meta(c, k)?.unwrap_or(dflt);
    }
    Ok(d)
}

fn new_id() -> String {
    use p256::elliptic_curve::rand_core::{OsRng, RngCore};
    format!("pin_{}_{:04x}", chrono::Utc::now().timestamp_millis(), OsRng.next_u32() & 0xffff)
}

/// Check and fill a pin. Coordinates become numbers; an unknown status or a
/// `myRating` outside 1..=5 is refused rather than stored. Google's decimal
/// `rating` is not checked: refusing it dropped a searched place on import.
pub fn normalize(pin: &Value) -> R<Value> {
    let Some(obj) = pin.as_object() else { return Err(MapError::Bad("a pin must be an object".into())) };
    let mut p: Map<String, Value> = obj.clone();
    p.remove("rev");
    let (lat, lng) = latlng(pin).ok_or_else(|| MapError::Bad("pin requires numeric lat and lng".into()))?;
    if !(-90.0..=90.0).contains(&lat) || !(-180.0..=180.0).contains(&lng) {
        return Err(MapError::Bad("lat/lng out of range".into()));
    }
    p.insert("lat".into(), json!(lat));
    p.insert("lng".into(), json!(lng));
    if p.get("id").and_then(Value::as_str).is_none_or(str::is_empty) {
        p.insert("id".into(), json!(new_id()));
    }
    if !p.contains_key("name") {
        let n = p.get("title").and_then(Value::as_str).filter(|t| !t.is_empty()).unwrap_or("Pin").to_string();
        p.insert("name".into(), json!(n));
    }
    if !p.get("tags").is_some_and(Value::is_array) {
        p.insert("tags".into(), json!([]));
    }
    p.entry("desc").or_insert(json!(""));
    p.entry("createdAt").or_insert(json!(chrono::Utc::now().timestamp_millis()));
    if let Some(st) = p.get("status") {
        let st = st.as_str().unwrap_or("?");
        if !STATUSES.contains(&st) {
            return Err(MapError::Bad(format!("status must be one of want, been, favorite, closed (got {st:?})")));
        }
    }
    match p.get("myRating") {
        None | Some(Value::Null) => {}
        Some(v) => match v.as_f64() {
            Some(r) if (1.0..=5.0).contains(&r) && r.fract() == 0.0 => {
                p.insert("myRating".into(), json!(r as i64));
            }
            _ => return Err(MapError::Bad("myRating must be a whole number from 1 to 5".into())),
        },
    }
    Ok(Value::Object(p))
}

#[derive(Debug, PartialEq)]
pub enum Upsert {
    Created,
    Updated,
    Unchanged,
}

/// Create or replace one pin. `base_rev` (the rev the editor loaded) makes a
/// stale write a 409 instead of silently undoing another device's edit.
pub fn upsert(c: &Connection, pin: &Value, actor: &str, base_rev: Option<i64>) -> R<(Upsert, Value)> {
    let mut p = normalize(pin)?;
    let id = s(&p, "id").to_string();
    let (lat, lng) = latlng(&p).expect("normalized");
    let t = now();
    match get_pin(c, &id)? {
        Some((old, rev)) => {
            if let Some(b) = base_rev {
                if b != rev {
                    return Err(MapError::Conflict(json!({"error": "this pin changed since you opened it", "pin": with_rev(old, rev)})));
                }
            }
            sync_event(c, &old, &mut p)?;
            if old == p {
                return Ok((Upsert::Unchanged, with_rev(p, rev)));
            }
            c.execute(
                "UPDATE map_pins SET doc=?2, lat=?3, lng=?4, updated_at=?5, rev=rev+1 WHERE id=?1",
                params![id, p.to_string(), lat, lng, t],
            )?;
            record(c, &id, actor, "update", Some(&old), Some(&p))?;
            Ok((Upsert::Updated, with_rev(p, rev + 1)))
        }
        None => {
            sync_event(c, &Value::Null, &mut p)?;
            c.execute(
                "INSERT INTO map_pins(id,doc,lat,lng,created_at,updated_at,rev) VALUES(?1,?2,?3,?4,?5,?5,1)",
                params![id, p.to_string(), lat, lng, t],
            )?;
            record(c, &id, actor, "create", None, Some(&p))?;
            Ok((Upsert::Created, with_rev(p, 1)))
        }
    }
}

fn with_rev(mut p: Value, rev: i64) -> Value {
    p["rev"] = json!(rev);
    p
}

pub fn delete(c: &Connection, id: &str, actor: &str) -> R<bool> {
    let Some((old, _)) = get_pin(c, id)? else { return Ok(false) };
    let mut gone = old.clone();
    if let Some(o) = gone.as_object_mut() {
        o.remove("eventAt");
    }
    sync_event(c, &old, &mut gone)?;
    c.execute("DELETE FROM map_pins WHERE id=?1", [id])?;
    record(c, id, actor, "delete", Some(&old), None)?;
    Ok(true)
}

/// The old whole-document write, kept for clients that still send it. Pins
/// are applied one by one (only changed pins are written and recorded); with
/// `replace`, pins missing from the body are deleted. Without it, a body with
/// fewer pins than stored is refused, as before.
pub fn replace_doc(c: &Connection, body: &Value, replace: bool, actor: &str) -> R<Value> {
    let incoming = body.get("pins").and_then(Value::as_array).cloned().unwrap_or_default();
    let existing: Vec<String> = c
        .prepare_cached("SELECT id FROM map_pins")?
        .query_map([], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    if !existing.is_empty() && incoming.len() < existing.len() && !replace {
        return Err(MapError::Conflict(json!({
            "error": format!("refusing to drop pins {}->{}: POST /api/map replaces the whole map. To ADD a pin use POST /api/map/pins; to intentionally replace everything pass ?replace=1", existing.len(), incoming.len()),
            "existing_pins": existing.len(), "submitted_pins": incoming.len(),
        })));
    }
    let (mut created, mut updated, mut deleted, mut skipped) = (0, 0, 0, 0);
    let mut seen = std::collections::HashSet::new();
    for p in &incoming {
        match upsert(c, p, actor, None) {
            Ok((kind, saved)) => {
                seen.insert(s(&saved, "id").to_string());
                match kind {
                    Upsert::Created => created += 1,
                    Upsert::Updated => updated += 1,
                    Upsert::Unchanged => {}
                }
            }
            Err(MapError::Bad(_)) => skipped += 1,
            Err(e) => return Err(e),
        }
    }
    if replace {
        for id in existing.iter().filter(|id| !seen.contains(*id)) {
            if delete(c, id, actor)? {
                deleted += 1;
            }
        }
    }
    for k in META_KEYS {
        if let Some(v) = body.get(*k) {
            set_meta(c, k, v)?;
        }
    }
    Ok(json!({"ok": true, "created": created, "updated": updated, "deleted": deleted, "skipped": skipped}))
}

/// First read after the upgrade: copy ~/.amux/map.json into the tables once.
/// The file is left where it is as the backup. Returns the pin count imported.
pub fn ensure_imported(c: &Connection, json_path: &Path) -> R<Option<usize>> {
    if get_meta(c, "imported")?.is_some() {
        return Ok(None);
    }
    let have: i64 = c.query_row("SELECT count(*) FROM map_pins", [], |r| r.get(0))?;
    if have > 0 {
        set_meta(c, "imported", &json!({"from": "database", "at": iso(now())}))?;
        return Ok(None);
    }
    let body = std::fs::read_to_string(json_path).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok());
    let Some(body) = body else {
        set_meta(c, "imported", &json!({"from": "none", "at": iso(now())}))?;
        return Ok(Some(0));
    };
    let r = replace_doc(c, &body, true, "import:map.json")?;
    let n = r["created"].as_u64().unwrap_or(0) as usize;
    set_meta(c, "imported", &json!({"from": json_path.display().to_string(), "pins": n,
        "skipped": r["skipped"], "at": iso(now())}))?;
    Ok(Some(n))
}

// ---------------------------------------------------------------- bulk + merge

/// One change applied to many pins: status, rating, add_tags, remove_tags or delete.
pub fn bulk(c: &Connection, ids: &[String], op: &str, arg: &Value, actor: &str) -> R<Value> {
    let (mut changed, mut missing) = (0, 0);
    for id in ids {
        let Some((mut p, _)) = get_pin(c, id)? else {
            missing += 1;
            continue;
        };
        match op {
            "delete" => {
                if delete(c, id, actor)? {
                    changed += 1;
                }
                continue;
            }
            "status" => p["status"] = json!(arg.as_str().unwrap_or("")),
            "rating" => p["myRating"] = arg.clone(),
            "add_tags" | "remove_tags" => {
                let want: Vec<String> = arg.as_array().map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect()).unwrap_or_default();
                let mut tags: Vec<String> = p["tags"].as_array().map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect()).unwrap_or_default();
                if op == "add_tags" {
                    for t in want {
                        if !tags.contains(&t) {
                            tags.push(t);
                        }
                    }
                } else {
                    tags.retain(|t| !want.contains(t));
                }
                p["tags"] = json!(tags);
            }
            _ => return Err(MapError::Bad(format!("unknown bulk op {op:?} (status, rating, add_tags, remove_tags, delete)"))),
        }
        if upsert(c, &p, actor, None)?.0 == Upsert::Updated {
            changed += 1;
        }
    }
    Ok(json!({"ok": true, "op": op, "changed": changed, "missing": missing, "n_considered": ids.len()}))
}

fn status_rank(st: &str) -> i32 {
    match st {
        "favorite" => 4,
        "been" => 3,
        "want" => 2,
        "closed" => 1,
        _ => 0,
    }
}

/// Fold `drop` into `keep`: tags and notes are combined, the better status and
/// rating win, visit counts add up, and `drop` is deleted.
pub fn merge(c: &Connection, keep_id: &str, drop_id: &str, actor: &str) -> R<Value> {
    if keep_id == drop_id {
        return Err(MapError::Bad("cannot merge a pin into itself".into()));
    }
    let (mut keep, _) = get_pin(c, keep_id)?.ok_or_else(|| MapError::NotFound(keep_id.into()))?;
    let (drop, _) = get_pin(c, drop_id)?.ok_or_else(|| MapError::NotFound(drop_id.into()))?;
    let mut tags: Vec<Value> = keep["tags"].as_array().cloned().unwrap_or_default();
    for t in drop["tags"].as_array().cloned().unwrap_or_default() {
        if !tags.contains(&t) {
            tags.push(t);
        }
    }
    keep["tags"] = json!(tags);
    for k in ["notes", "desc"] {
        let (a, b) = (s(&keep, k).trim().to_string(), s(&drop, k).trim().to_string());
        if !b.is_empty() && !a.contains(&b) {
            keep[k] = json!(if a.is_empty() { b } else { format!("{a}\n\n{b}") });
        }
    }
    if status_rank(s(&drop, "status")) > status_rank(s(&keep, "status")) {
        keep["status"] = drop["status"].clone();
    }
    if drop["myRating"].as_i64().unwrap_or(0) > keep["myRating"].as_i64().unwrap_or(0) {
        keep["myRating"] = drop["myRating"].clone();
    }
    for k in ["placeId", "place", "photoName", "eventAt"] {
        if keep.get(k).is_none_or(Value::is_null) && drop.get(k).is_some_and(|v| !v.is_null()) {
            keep[k] = drop[k].clone();
        }
    }
    if let Some(dv) = drop.get("visits").filter(|v| v.is_object()) {
        let kv = keep.get("visits").cloned().unwrap_or(json!({}));
        let first = [s(&kv, "first"), s(dv, "first")].into_iter().filter(|x| !x.is_empty()).min().unwrap_or("").to_string();
        let last = [s(&kv, "last"), s(dv, "last")].into_iter().max().unwrap_or("").to_string();
        keep["visits"] = json!({"count": kv["count"].as_i64().unwrap_or(0) + dv["count"].as_i64().unwrap_or(0), "first": first, "last": last});
    }
    keep["mergedFrom"] = json!([drop_id]);
    let (_, saved) = upsert(c, &keep, actor, None)?;
    delete(c, drop_id, actor)?;
    record(c, keep_id, actor, "merge", Some(&drop), Some(&saved))?;
    Ok(saved)
}

/// Pairs of pins that are probably the same place: within DUP_RADIUS_M with
/// similar names, the same Google place, or within 10 m whatever the name.
pub fn duplicates(c: &Connection) -> R<Vec<Value>> {
    let pins = all_pins(c)?;
    let mut out = Vec::new();
    for i in 0..pins.len() {
        let Some((la, lo)) = latlng(&pins[i]) else { continue };
        for j in (i + 1)..pins.len() {
            let Some((lb, lp)) = latlng(&pins[j]) else { continue };
            let d = dist_m(la, lo, lb, lp);
            if d > DUP_RADIUS_M {
                continue;
            }
            let (a, b) = (&pins[i], &pins[j]);
            let same_place = !s(a, "placeId").is_empty() && s(a, "placeId") == s(b, "placeId");
            let sim = name_similarity(s(a, "name"), s(b, "name"));
            let reason = if same_place {
                "same Google place"
            } else if sim >= 0.5 {
                "similar name nearby"
            } else if d <= 10.0 {
                "same spot"
            } else {
                continue;
            };
            out.push(json!({"a": a, "b": b, "distance_m": (d * 10.0).round() / 10.0, "name_similarity": (sim * 100.0).round() / 100.0, "reason": reason}));
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------- visits

/// Mark pins you have been to from the phone's visit reports: a visit of at
/// least VISIT_MIN_S within VISIT_RADIUS_M. A pin with no status or "want"
/// becomes "been"; a status set by hand (favorite, closed, been) is kept.
pub fn match_visits(c: &Connection, actor: &str) -> R<Value> {
    let mut st = c.prepare_cached(
        "SELECT arrival, departure, lat, lon FROM location_visits
         WHERE departure IS NOT NULL AND departure - arrival >= ?1 AND departure - arrival <= 43200",
    )?;
    let visits: Vec<(f64, f64, f64, f64)> = st
        .query_map([VISIT_MIN_S], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
        .collect::<Result<_, _>>()?;
    let pins = all_pins(c)?;
    let (mut updated, mut marked_been, mut with_visits) = (0, 0, 0);
    for p in &pins {
        let Some((la, lo)) = latlng(p) else { continue };
        let hits: Vec<f64> = visits
            .iter()
            .filter(|v| dist_m(la, lo, v.2, v.3) <= VISIT_RADIUS_M)
            .map(|v| v.0)
            .collect();
        if hits.is_empty() {
            continue;
        }
        with_visits += 1;
        let first = hits.iter().cloned().fold(f64::INFINITY, f64::min);
        let last = hits.iter().cloned().fold(0.0, f64::max);
        let mut np = p.clone();
        np["visits"] = json!({"count": hits.len(), "first": iso(first), "last": iso(last), "source": "location_visits"});
        let st = s(p, "status");
        if st.is_empty() || st == "want" {
            np["status"] = json!("been");
            np["statusSource"] = json!("visits");
        }
        if upsert(c, &np, actor, None)?.0 == Upsert::Updated {
            updated += 1;
            if s(&np, "status") == "been" && s(p, "status") != "been" {
                marked_been += 1;
            }
        }
    }
    Ok(json!({"ok": true, "measured": true, "n_considered": pins.len(), "visits_considered": visits.len(),
        "pins_with_visits": with_visits, "updated": updated, "marked_been": marked_been}))
}

// ---------------------------------------------------------------- places

/// The best Google candidate for a pin: within 150 m, and either a similar
/// name or within 25 m. None when nothing is convincing.
pub fn pick_candidate<'a>(pin: &Value, candidates: &'a [Value]) -> Option<&'a Value> {
    let (la, lo) = latlng(pin)?;
    candidates
        .iter()
        .filter_map(|cand| {
            let (cl, cg) = (cand["location"]["latitude"].as_f64()?, cand["location"]["longitude"].as_f64()?);
            let d = dist_m(la, lo, cl, cg);
            let sim = name_similarity(s(pin, "name"), cand["displayName"]["text"].as_str().unwrap_or(""));
            (d <= 150.0 && (sim >= 0.5 || d <= 25.0)).then_some((sim, d, cand))
        })
        .max_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal).then(b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal)))
        .map(|x| x.2)
}

/// Store what Google says about the pin's place. A permanently closed place
/// turns the pin's status to "closed".
pub fn apply_place(pin: &mut Value, place: &Value) {
    let hours: Vec<Value> = place["regularOpeningHours"]["weekdayDescriptions"].as_array().cloned().unwrap_or_default();
    let status = place["businessStatus"].as_str().unwrap_or("");
    pin["placeId"] = place["id"].clone();
    pin["place"] = json!({
        "id": place["id"], "name": place["displayName"]["text"], "businessStatus": status,
        "rating": place["rating"], "ratingCount": place["userRatingCount"], "hours": hours,
        "mapsUri": place["googleMapsUri"], "refreshedAt": iso(now()),
    });
    if status == "CLOSED_PERMANENTLY" && s(pin, "status") != "closed" {
        pin["status"] = json!("closed");
        pin["statusSource"] = json!("google");
    }
}

// ---------------------------------------------------------------- calendar

/// A dated pin (`eventAt`) owns one calendar event: created, moved, renamed,
/// and soft-deleted with the pin. Returns whether the calendar changed.
pub fn sync_event(c: &Connection, old: &Value, new: &mut Value) -> R<bool> {
    let at = s(new, "eventAt").trim().to_string();
    let ev_id = s(new, "calEventId").to_string();
    let ev_id = if ev_id.is_empty() { s(old, "calEventId").to_string() } else { ev_id };
    let t = chrono::Utc::now().timestamp();
    if at.is_empty() {
        if ev_id.is_empty() {
            return Ok(false);
        }
        c.execute("UPDATE cal_events SET deleted=?2, updated=?2 WHERE id=?1 AND deleted IS NULL", params![ev_id, t])?;
        if let Some(o) = new.as_object_mut() {
            o.remove("calEventId");
        }
        return Ok(true);
    }
    let title = s(new, "name").to_string();
    let loc = format!("{:.6},{:.6}", num(new.get("lat")).unwrap_or(0.0), num(new.get("lng")).unwrap_or(0.0));
    let desc = format!("Map pin {}{}", s(new, "id"), if s(new, "desc").is_empty() { String::new() } else { format!("\n{}", s(new, "desc")) });
    let all_day = (at.len() == 10) as i64;
    let alive = !ev_id.is_empty()
        && c.query_row("SELECT 1 FROM cal_events WHERE id=?1 AND deleted IS NULL", [&ev_id], |_| Ok(())).optional()?.is_some();
    if alive {
        if s(old, "eventAt") == at && s(old, "name") == title && latlng(old) == latlng(new) && s(old, "desc") == s(new, "desc") {
            new["calEventId"] = json!(ev_id);
            return Ok(false);
        }
        c.execute(
            "UPDATE cal_events SET title=?2, start=?3, all_day=?4, location=?5, description=?6, updated=?7 WHERE id=?1",
            params![ev_id, title, at, all_day, loc, desc, t],
        )?;
        new["calEventId"] = json!(ev_id);
    } else {
        let id = crate::db::board_store::next_issue_id(c, "EVT")?;
        c.execute(
            "INSERT INTO cal_events (id,title,start,end,all_day,location,description,rrule,created,updated) VALUES (?1,?2,?3,NULL,?4,?5,?6,NULL,?7,?7)",
            params![id, title, at, all_day, loc, desc, t],
        )?;
        new["calEventId"] = json!(id);
    }
    Ok(true)
}

// ---------------------------------------------------------------- lists + shares

fn tag_category(tags: &[Value], tag_id: &str) -> String {
    tags.iter()
        .find(|t| s(t, "id") == tag_id)
        .map(|t| s(t, "category").to_string())
        .unwrap_or_default()
}

/// Does a pin match a saved filter? Tags: any of the selected tags within a
/// category, all categories at once (tags with no category form one group).
/// Status: any of. q: words in name, description or tag names.
pub fn matches(pin: &Value, filter: &Value, tags: &[Value]) -> bool {
    let sel: Vec<&str> = filter["tags"].as_array().map(|a| a.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
    if !sel.is_empty() {
        let pt: Vec<&str> = pin["tags"].as_array().map(|a| a.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
        let mut cats: Vec<String> = sel.iter().map(|t| tag_category(tags, t)).collect();
        cats.sort();
        cats.dedup();
        let ok = cats.iter().all(|cat| sel.iter().filter(|t| &tag_category(tags, t) == cat).any(|t| pt.contains(t)));
        if !ok {
            return false;
        }
    }
    let statuses: Vec<&str> = filter["status"].as_array().map(|a| a.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
    if !statuses.is_empty() && !statuses.contains(&s(pin, "status")) {
        return false;
    }
    let q = s(filter, "q").trim().to_lowercase();
    if !q.is_empty() {
        let tag_names: String = pin["tags"].as_array().map(|a| a.iter().filter_map(Value::as_str).map(|t| tags.iter().find(|x| s(x, "id") == t).map(|x| s(x, "name").to_lowercase()).unwrap_or_default()).collect::<Vec<_>>().join(" ")).unwrap_or_default();
        let hay = format!("{} {} {}", s(pin, "name").to_lowercase(), s(pin, "desc").to_lowercase(), tag_names);
        if !hay.contains(&q) {
            return false;
        }
    }
    true
}

fn find_list(c: &Connection, list_id: &str) -> R<Value> {
    get_meta(c, "lists")?
        .and_then(|l| l.as_array().and_then(|a| a.iter().find(|x| s(x, "id") == list_id).cloned()))
        .ok_or_else(|| MapError::NotFound(format!("no saved list {list_id}")))
}

/// A read-only link for a saved list; the same link comes back until revoked.
pub fn share(c: &Connection, list_id: &str) -> R<String> {
    find_list(c, list_id)?;
    if let Some(t) = c
        .query_row("SELECT token FROM map_shares WHERE list_id=?1 AND revoked_at IS NULL", [list_id], |r| r.get::<_, String>(0))
        .optional()?
    {
        return Ok(t);
    }
    use p256::elliptic_curve::rand_core::{OsRng, RngCore};
    let mut b = [0u8; 18];
    OsRng.fill_bytes(&mut b);
    let token = hex::encode(b);
    c.execute("INSERT INTO map_shares(token,list_id,created_at) VALUES(?1,?2,?3)", params![token, list_id, now()])?;
    Ok(token)
}

pub fn unshare(c: &Connection, list_id: &str) -> R<usize> {
    Ok(c.execute("UPDATE map_shares SET revoked_at=?2 WHERE list_id=?1 AND revoked_at IS NULL", params![list_id, now()])?)
}

pub fn share_token_for(c: &Connection, list_id: &str) -> R<Option<String>> {
    Ok(c.query_row("SELECT token FROM map_shares WHERE list_id=?1 AND revoked_at IS NULL", [list_id], |r| r.get(0)).optional()?)
}

/// What a shared link shows: the list's pins with public fields only. Notes,
/// visits and history never leave the server this way.
pub fn shared_view(c: &Connection, token: &str) -> R<Option<Value>> {
    let list_id: Option<String> = c
        .query_row("SELECT list_id FROM map_shares WHERE token=?1 AND revoked_at IS NULL", [token], |r| r.get(0))
        .optional()?;
    let Some(list_id) = list_id else { return Ok(None) };
    let Ok(list) = find_list(c, &list_id) else { return Ok(None) };
    let tags: Vec<Value> = get_meta(c, "tags")?.and_then(|v| v.as_array().cloned()).unwrap_or_default();
    let pins: Vec<Value> = all_pins(c)?
        .into_iter()
        .filter(|p| matches(p, &list["filter"], &tags))
        .map(|p| {
            let tag_list: Vec<Value> = p["tags"].as_array().cloned().unwrap_or_default().iter()
                .filter_map(|t| tags.iter().find(|x| x["id"] == *t).map(|x| json!({"name": x["name"], "color": x["color"]})))
                .collect();
            json!({"name": p["name"], "lat": p["lat"], "lng": p["lng"], "desc": p["desc"], "status": p["status"],
                   "myRating": p["myRating"], "tags": tag_list, "hours": p["place"]["hours"], "mapsUri": p["place"]["mapsUri"]})
        })
        .collect();
    Ok(Some(json!({"list": {"name": list["name"]}, "pins": pins})))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(include_str!("../../migrations/0128_map_store.sql")).unwrap();
        c.execute_batch(
            "CREATE TABLE location_visits (id TEXT PRIMARY KEY, device TEXT NOT NULL DEFAULT '', arrival REAL NOT NULL,
               departure REAL, lat REAL NOT NULL, lon REAL NOT NULL, h_acc REAL, received REAL NOT NULL);
             CREATE TABLE cal_events (id TEXT PRIMARY KEY, title TEXT NOT NULL, start TEXT NOT NULL, end TEXT,
               all_day INTEGER NOT NULL DEFAULT 0, location TEXT, description TEXT, rrule TEXT, created INTEGER NOT NULL,
               updated INTEGER NOT NULL, deleted INTEGER);
             CREATE TABLE issue_counters (prefix TEXT PRIMARY KEY, next_n INTEGER NOT NULL);",
        )
        .unwrap();
        c
    }

    fn pin(id: &str, name: &str, lat: f64, lng: f64) -> Value {
        json!({"id": id, "name": name, "lat": lat, "lng": lng, "tags": []})
    }

    #[test]
    fn map_json_is_imported_once_and_the_file_is_left_alone() {
        let c = db();
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("map.json");
        let body = json!({"pins": [pin("a", "Joe's", 40.7, -74.0), {"name": "string coords", "lat": "40.71", "lng": "-74.01"}],
            "tags": [{"id": "t1", "name": "Coffee", "color": "#000"}], "settings": {"sortBy": "name", "googleMapsKey": "fake-key"}, "trials": []});
        std::fs::write(&f, body.to_string()).unwrap();
        assert_eq!(ensure_imported(&c, &f).unwrap(), Some(2));
        assert_eq!(ensure_imported(&c, &f).unwrap(), None, "second call does nothing");
        let d = doc(&c).unwrap();
        assert_eq!(d["pins"].as_array().unwrap().len(), 2);
        assert_eq!(d["pins"][1]["lat"], json!(40.71), "string coordinates become numbers");
        assert_eq!(d["tags"][0]["name"], "Coffee");
        assert_eq!(d["settings"]["sortBy"], "name");
        assert!(d["settings"].get("googleMapsKey").is_none(), "the API key saved in old map.json files is not stored");
        assert!(f.exists(), "map.json stays as the backup");
    }

    #[test]
    fn a_stale_edit_is_refused_and_every_change_is_in_the_history() {
        let c = db();
        let (k, p) = upsert(&c, &pin("a", "One", 40.0, -74.0), "t", None).unwrap();
        assert_eq!((k, p["rev"].as_i64()), (Upsert::Created, Some(1)));
        let mut p2 = p.clone();
        p2["name"] = json!("Two");
        assert_eq!(upsert(&c, &p2, "phone", Some(1)).unwrap().0, Upsert::Updated);
        let mut stale = p.clone();
        stale["name"] = json!("Stale");
        assert!(matches!(upsert(&c, &stale, "laptop", Some(1)), Err(MapError::Conflict(_))), "rev 1 is stale after one update");
        assert_eq!(upsert(&c, &p2, "phone", None).unwrap().0, Upsert::Unchanged);
        let h = history(&c, "a").unwrap();
        assert_eq!(h.iter().map(|x| x["action"].as_str().unwrap()).collect::<Vec<_>>(), ["update", "create"]);
        assert_eq!(h[0]["actor"], "phone");
        assert!(matches!(normalize(&json!({"lat": 1, "lng": 1, "status": "visited"})), Err(MapError::Bad(_))));
        assert!(matches!(normalize(&json!({"lat": 1, "lng": 1, "myRating": 6})), Err(MapError::Bad(_))));
        // Google's rating from a search result is a decimal and stays as sent.
        let g = normalize(&json!({"lat": 1, "lng": 1, "rating": 4.7, "ratingCount": 12})).unwrap();
        assert_eq!((g["rating"].as_f64(), g.get("myRating")), (Some(4.7), None));
    }

    #[test]
    fn the_whole_document_write_still_guards_against_dropping_pins() {
        let c = db();
        replace_doc(&c, &json!({"pins": [pin("a", "A", 1.0, 1.0), pin("b", "B", 2.0, 2.0)]}), false, "t").unwrap();
        assert!(matches!(replace_doc(&c, &json!({"pins": [pin("a", "A", 1.0, 1.0)]}), false, "t"), Err(MapError::Conflict(_))));
        let r = replace_doc(&c, &json!({"pins": [pin("a", "A2", 1.0, 1.0)]}), true, "t").unwrap();
        assert_eq!((r["updated"].as_i64(), r["deleted"].as_i64()), (Some(1), Some(1)));
        assert_eq!(history(&c, "b").unwrap()[0]["action"], "delete");
    }

    #[test]
    fn a_long_visit_nearby_marks_a_pin_been_and_a_hand_set_status_is_kept() {
        let c = db();
        upsert(&c, &pin("cafe", "Cafe", 40.7128, -74.0060), "t", None).unwrap();
        let mut fav = pin("fav", "Fav", 40.7300, -74.0000);
        fav["status"] = json!("favorite");
        upsert(&c, &fav, "t", None).unwrap();
        upsert(&c, &pin("far", "Far", 40.8000, -74.0000), "t", None).unwrap();
        let ins = |id: &str, arr: f64, dep: f64, lat: f64, lon: f64| {
            c.execute("INSERT INTO location_visits(id,arrival,departure,lat,lon,received) VALUES(?1,?2,?3,?4,?5,0)", params![id, arr, dep, lat, lon]).unwrap();
        };
        ins("v1", 1_700_000_000.0, 1_700_001_800.0, 40.71285, -74.00605); // 30 min, ~6 m away
        ins("v2", 1_700_100_000.0, 1_700_100_120.0, 40.7128, -74.0060); // 2 min: a walk past
        ins("v3", 1_700_200_000.0, 1_700_203_600.0, 40.7300, -74.0000); // at the favorite
        let r = match_visits(&c, "visits").unwrap();
        assert_eq!((r["pins_with_visits"].as_i64(), r["marked_been"].as_i64()), (Some(2), Some(1)));
        let cafe = get_pin(&c, "cafe").unwrap().unwrap().0;
        assert_eq!((cafe["status"].as_str(), cafe["visits"]["count"].as_i64()), (Some("been"), Some(1)));
        assert_eq!(get_pin(&c, "fav").unwrap().unwrap().0["status"], "favorite");
        assert!(get_pin(&c, "far").unwrap().unwrap().0.get("visits").is_none());
        assert_eq!(match_visits(&c, "visits").unwrap()["updated"], 0, "a second run changes nothing");
    }

    #[test]
    fn duplicates_and_merge_keep_everything_worth_keeping() {
        let c = db();
        let mut a = pin("a", "Joe's Pizza", 40.73060, -73.98880);
        a["notes"] = json!("cash only");
        a["tags"] = json!(["t1"]);
        let mut b = pin("b", "Joe's Pizza Broadway", 40.73075, -73.98870); // ~19 m
        b["notes"] = json!("slice is $4");
        b["tags"] = json!(["t2"]);
        b["status"] = json!("favorite");
        upsert(&c, &a, "t", None).unwrap();
        upsert(&c, &b, "t", None).unwrap();
        upsert(&c, &pin("c", "Totally Different", 40.73062, -73.98882), "t", None).unwrap(); // 2 m from a: same spot
        upsert(&c, &pin("d", "Joe's Pizza", 40.7400, -73.9900), "t", None).unwrap(); // same name, 1 km away
        let dups = duplicates(&c).unwrap();
        let pairs: Vec<(String, String)> = dups.iter().map(|d| (d["a"]["id"].as_str().unwrap().into(), d["b"]["id"].as_str().unwrap().into())).collect();
        assert!(pairs.contains(&("a".into(), "b".into())));
        assert!(pairs.contains(&("a".into(), "c".into())));
        assert!(!pairs.iter().any(|p| p.1 == "d"), "far apart is not a duplicate");
        let m = merge(&c, "a", "b", "t").unwrap();
        assert_eq!(m["tags"], json!(["t1", "t2"]));
        assert_eq!(m["notes"], "cash only\n\nslice is $4");
        assert_eq!(m["status"], "favorite");
        assert!(get_pin(&c, "b").unwrap().is_none());
    }

    #[test]
    fn a_closed_place_closes_the_pin_and_candidates_need_a_similar_name_or_to_be_very_close() {
        let p = pin("a", "Blue Bottle Coffee", 40.7420, -74.0048);
        let near = json!({"id": "g1", "displayName": {"text": "Blue Bottle Coffee"}, "location": {"latitude": 40.7421, "longitude": -74.0049}});
        let wrong = json!({"id": "g2", "displayName": {"text": "Chase Bank"}, "location": {"latitude": 40.7425, "longitude": -74.0048}});
        assert_eq!(pick_candidate(&p, &[wrong.clone(), near.clone()]).unwrap()["id"], "g1");
        assert!(pick_candidate(&p, &[wrong]).is_none(), "a different name 55 m away is not the place");
        let mut q = p.clone();
        apply_place(&mut q, &json!({"id": "g1", "displayName": {"text": "Blue Bottle"}, "businessStatus": "CLOSED_PERMANENTLY",
            "rating": 4.4, "regularOpeningHours": {"weekdayDescriptions": ["Monday: 7 AM – 6 PM"]}}));
        assert_eq!((q["placeId"].as_str(), q["status"].as_str()), (Some("g1"), Some("closed")));
        assert_eq!(q["place"]["hours"][0], "Monday: 7 AM – 6 PM");
    }

    #[test]
    fn categories_or_within_and_across_and_a_share_shows_no_notes() {
        let c = db();
        let tags = json!([{"id": "kbbq", "name": "Korean BBQ", "category": "cuisine"}, {"id": "ramen", "name": "Ramen", "category": "cuisine"},
            {"id": "date", "name": "Date spots", "category": "vibe"}]);
        set_meta(&c, "tags", &tags).unwrap();
        let mut a = pin("a", "Cote", 40.74, -73.99);
        a["tags"] = json!(["kbbq", "date"]);
        a["notes"] = json!("private note");
        let mut b = pin("b", "Ippudo", 40.73, -73.99);
        b["tags"] = json!(["ramen"]);
        upsert(&c, &a, "t", None).unwrap();
        upsert(&c, &b, "t", None).unwrap();
        let t = tags.as_array().unwrap();
        assert!(matches(&a, &json!({"tags": ["kbbq", "ramen"]}), t) && matches(&b, &json!({"tags": ["kbbq", "ramen"]}), t), "any within a category");
        assert!(matches(&a, &json!({"tags": ["kbbq", "ramen", "date"]}), t) && !matches(&b, &json!({"tags": ["kbbq", "ramen", "date"]}), t), "all across categories");
        set_meta(&c, "lists", &json!([{"id": "L1", "name": "Date night", "filter": {"tags": ["date"]}}])).unwrap();
        let tok = share(&c, "L1").unwrap();
        assert_eq!(share(&c, "L1").unwrap(), tok, "same link until revoked");
        let v = shared_view(&c, &tok).unwrap().unwrap();
        assert_eq!(v["pins"].as_array().unwrap().len(), 1);
        assert!(!v.to_string().contains("private note"), "notes never leave through a share");
        unshare(&c, "L1").unwrap();
        assert!(shared_view(&c, &tok).unwrap().is_none(), "a revoked link shows nothing");
    }

    #[test]
    fn a_dated_pin_owns_one_calendar_event() {
        let c = db();
        let mut p = pin("a", "Gala", 40.7, -74.0);
        p["eventAt"] = json!("2026-11-01T19:00");
        let (_, saved) = upsert(&c, &p, "t", None).unwrap();
        let ev = saved["calEventId"].as_str().unwrap().to_string();
        let mut moved = saved.clone();
        moved["eventAt"] = json!("2026-11-02T19:00");
        upsert(&c, &moved, "t", None).unwrap();
        let start: String = c.query_row("SELECT start FROM cal_events WHERE id=?1", [&ev], |r| r.get(0)).unwrap();
        assert_eq!(start, "2026-11-02T19:00");
        assert_eq!(c.query_row("SELECT count(*) FROM cal_events", [], |r| r.get::<_, i64>(0)).unwrap(), 1, "moved, not duplicated");
        delete(&c, "a", "t").unwrap();
        let deleted: Option<i64> = c.query_row("SELECT deleted FROM cal_events WHERE id=?1", [&ev], |r| r.get(0)).unwrap();
        assert!(deleted.is_some(), "deleting the pin removes its event");
    }
}
