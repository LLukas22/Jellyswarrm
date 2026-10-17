//! Jellyfin protocol DTOs, authorization headers, and shared token generation.
mod authorization;
mod jellyfin;

#[cfg(test)]
mod tests;

pub use authorization::{generate_token, Authorization};
pub use jellyfin::*;
