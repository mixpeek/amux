//! amux server binary. Phase 0 entry point — grows the full router/runtime
//! across phases per docs/rust-rebuild-plan.md.

fn main() {
    let mut args = std::env::args_os().skip(1);
    if args.next().as_deref() == Some(std::ffi::OsStr::new("--storage-vacuum-helper")) {
        let Some(path) = args.next().filter(|_| args.next().is_none()) else {
            eprintln!("storage vacuum helper requires exactly one database path");
            std::process::exit(2);
        };
        if let Err(error) =
            amux_server::runtime_jobs::storage::storage_vacuum_helper(std::path::Path::new(&path))
        {
            eprintln!("storage vacuum helper failed: {error}");
            std::process::exit(1);
        }
        return;
    }
    amux_server::run();
}
