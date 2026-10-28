//! `recordings-live`: live transcription of open recording sessions
//! (api/recording_live.rs). Each tick transcribes the audio that arrived
//! since the last one, in windows of about 15 s per track.

/// Every 5 s by default (a tick only reads session manifests until a window is due); `AMUX_RECORDINGS_LIVE_SECS=0` disables it. A tick
/// stops starting windows after 30 s, inside the registry's hung line.
pub fn spawn(state: crate::api::AppState) -> super::PeriodicTask {
    let name = super::registry::ids::RECORDINGS_LIVE;
    let secs = std::env::var(super::per_job_disable_var(name))
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|s| *s > 0)
        .unwrap_or(5);
    super::spawn_periodic(name, secs, move || {
        let state = state.clone();
        async move { crate::api::recording_live::live_pending(state).await }
    })
}
