//! What the host records about itself and its sessions: live status, stats captures, the
//! per-minute health lines, and the logs the host and its clients keep.

// Client log bundles uploaded over the management API.
pub(crate) mod client_logs;
// Who else holds an NVENC session (NVML); names the neighbour when a stream falls behind.
pub(crate) mod encoder_sessions;
#[forbid(unsafe_code)]
pub(crate) mod link_health;
pub(crate) mod log_capture;
pub(crate) mod net_health;
pub(crate) mod session_status;
pub(crate) mod stats_recorder;
