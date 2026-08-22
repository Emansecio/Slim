//! TUI adapter boundary. Rendering remains pure and IO-free in M0.

pub mod api;
pub mod app;
pub mod block;
pub mod cache;
pub mod composer;
pub mod fullscreen;
pub mod image;
pub mod input;
pub mod inspector;
pub mod layout;
pub mod reducer;
pub mod render;
pub mod runtime;
pub mod terminal;
pub mod testkit;
pub mod theme;
pub mod view_model;
pub mod welcome;

pub use runtime::run_app;

use slim_core::SessionSnapshot;

pub fn render_snapshot(snapshot: &SessionSnapshot) -> String {
    format!("{}#{}", snapshot.session_id, snapshot.sequence)
}
