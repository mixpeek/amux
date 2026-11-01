//! amux server binary. Phase 0 entry point — grows the full router/runtime
//! across phases per docs/rust-rebuild-plan.md.

/// Cap glibc at two arenas before any thread starts (AF-1134). glibc's default
/// keeps freed memory in up to 8 x ncores per-thread arenas. The 240-minute
/// soak measured 12692 KB/h uncapped against 1295 KB/h at two, with peak RSS
/// 207 MB against 121 MB, on a 2 MB/h gate. An explicit MALLOC_ARENA_MAX still
/// wins, so the soak can A/B it. glibc-only: macOS's allocator has no arenas.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
fn cap_malloc_arenas() {
    if std::env::var_os("MALLOC_ARENA_MAX").is_none() {
        // SAFETY: called from `main` before any other thread exists.
        if unsafe { libc::mallopt(libc::M_ARENA_MAX, 2) } == 0 {
            eprintln!("amux-server: verdict=malloc_arena_cap_failed mallopt refused M_ARENA_MAX=2");
        }
    }
}

fn main() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    cap_malloc_arenas();
    amux_server::run();
}
