//! A deployed snapshot replaces the embedded dashboard for the process
//! (`use_snapshot`). Its own test binary, because the source is chosen once
//! per process.
use std::path::Path;

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), &target).unwrap();
        }
    }
}

#[test]
fn a_snapshot_serves_its_own_bytes_and_version() {
    if std::env::var_os(amux_dashboard::DASHBOARD_DIR_ENV).is_some() {
        return; // CI's override wins over a snapshot by design.
    }
    let snap = std::env::temp_dir().join(format!("amux-dashboard-snapshot-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&snap);
    copy_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("static"), &snap);
    let app = snap.join("app.js");
    let mut text = std::fs::read_to_string(&app).unwrap();
    text.push_str("\n// changed in the snapshot only\n");
    std::fs::write(&app, text).unwrap();

    // A directory that cannot be read is refused and chooses nothing.
    assert!(amux_dashboard::use_snapshot(&snap.join("missing")).is_err());

    let o = amux_dashboard::use_snapshot(&snap).expect("snapshot taken");
    assert_eq!(o.version, amux_dashboard::dir_asset_version(&snap).unwrap());
    assert_ne!(amux_dashboard::asset_version(), amux_dashboard::ASSET_VER);
    let served = String::from_utf8(amux_dashboard::asset("app.js").unwrap().into_owned()).unwrap();
    assert!(served.contains("changed in the snapshot only"));
    assert!(served.contains(&format!("const APP_VER = '{}';", o.version)));
    // The source is fixed once chosen.
    assert!(amux_dashboard::use_snapshot(&snap).is_err());
    let _ = std::fs::remove_dir_all(&snap);
}
