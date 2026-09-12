//! Connects to a punktfunk host and drives the video/audio hardware pipelines.
//!
//! Video runs on a dedicated thread (`pump`'s video pump), which pulls access units off the
//! transport and hands them to `stage`'s `VideoStage` — PTS anchoring, freeze-until-reanchor and
//! backlog metering, all of it written against `core::media`'s `VideoSink` rather than against
//! any one backend.
//!
//! Audio takes one of two routes (`core::model::AudioRoutePref`) and always on a thread of its
//! own: `audio`'s stage decodes (or forwards) into whichever `AudioSink` the route selected — the
//! SDL device, or NDL's Opus plane. Neither shares the main loop, which carries the UI's software
//! rasterizer.
//!
//! The module is split by phase: `connect` runs the handshake, `pipeline` builds the decode path
//! it settled on, `pump` keeps it fed, and `probe` holds the two handshake-only connections
//! (pairing, speed test). `stage`, `timeline` and `stats` are the shared pieces underneath;
//! the bounded teardown join is `services::join`, shared with the platform layer.
//!
//! Nothing here touches SDL: the pad-feedback drain, which does, lives with the loop that owns
//! the SDL objects (`runtime::session_ext`).
pub mod audio;
mod connect;
pub mod pad_audio;
mod pipeline;
pub mod probe;
mod pump;
mod stage;
mod stats;
mod timeline;

pub use connect::{connect, ConnectParams, Connected};
pub use pump::{join_audio_feed, spawn_audio_feed};
pub use stats::StreamStats;
