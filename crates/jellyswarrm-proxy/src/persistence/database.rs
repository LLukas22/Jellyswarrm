//! Open SQLite with production durability settings and upgrade legacy identities before migrations.
use super::legacy_server_identity::canonicalize_legacy_server_identity;
use crate::config::{DATA_DIR, MIGRATOR};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use std::{str::FromStr, time::Duration};
use tracing::error;
pub(crate) async fn connect() -> Result<sqlx::SqlitePool, Box<dyn std::error::Error>> {
    // Resolve database path inside DATA_DIR
    let db_path = DATA_DIR.join("jellyswarrm.db");
    let db_url = format!("sqlite://{}", db_path.to_string_lossy());
    let options = SqliteConnectOptions::from_str(&db_url)?
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .synchronous(SqliteSynchronous::Normal)
        .foreign_keys(true)
        .busy_timeout(Duration::from_secs(30));

    let pool = SqlitePoolOptions::new()
        .after_connect(|connection, _| {
            Box::pin(async move {
                sqlx::query("PRAGMA wal_autocheckpoint = 1000;")
                    .execute(connection)
                    .await?;
                Ok(())
            })
        })
        .connect_with(options)
        .await?;

    canonicalize_legacy_server_identity(&pool)
        .await
        .unwrap_or_else(|e| {
            error!(
                "Failed to canonicalize legacy server identity data: {:#}",
                e
            );
            std::process::exit(1);
        });

    MIGRATOR.run(&pool).await.unwrap_or_else(|e| {
        error!("Failed to run database migrations: {}", e);
        std::process::exit(1);
    });

    Ok(pool)
}
