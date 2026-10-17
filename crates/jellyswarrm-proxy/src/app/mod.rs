//! Process lifecycle and concrete service composition, including shared application state.
mod bootstrap;
#[cfg(debug_assertions)]
mod debug_initialization;
mod logging;
mod shutdown;
mod state;
pub(crate) use bootstrap::run;
pub use state::{AppState, DataContext};
