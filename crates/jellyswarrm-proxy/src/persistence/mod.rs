//! SQLite initialization and compatibility upgrades for persisted legacy data.
pub(crate) mod database;
mod legacy_server_identity;
mod value_codecs;
pub(crate) static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!();
