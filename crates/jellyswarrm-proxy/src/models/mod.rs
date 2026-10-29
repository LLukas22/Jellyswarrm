//! Jellyfin response DTOs. Header parsing belongs to `protocol::authorization`.
mod jellyfin;

#[cfg(test)]
mod tests;

pub(crate) use jellyfin::*;
