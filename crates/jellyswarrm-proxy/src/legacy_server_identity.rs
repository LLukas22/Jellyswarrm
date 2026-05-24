use std::collections::HashMap;

use anyhow::{anyhow, Context, Result};
use sqlx::{Row, Sqlite, SqlitePool, Transaction};

use crate::server_url::ServerUrl;

#[derive(Debug, Clone)]
struct LegacyServer {
    id: i64,
    canonical_url: String,
}

#[derive(Debug)]
struct LegacyMapping {
    id: i64,
    server_id: Option<i64>,
    server_url: String,
}

#[derive(Debug)]
struct LegacyMediaMapping {
    id: i64,
    server_id: Option<i64>,
    server_url: String,
}

pub async fn canonicalize_legacy_server_identity(pool: &SqlitePool) -> Result<()> {
    let mut tx = pool.begin().await?;

    if !table_exists(&mut tx, "servers").await? {
        tx.commit().await?;
        return Ok(());
    }

    let servers = load_servers(&mut tx).await?;
    if servers.is_empty() {
        tx.commit().await?;
        return Ok(());
    }

    let canonical_server_ids = canonical_server_ids(&servers);
    let canonical_server_urls = canonical_server_urls(&servers);

    if table_exists(&mut tx, "server_admins").await? {
        merge_server_admins(&mut tx, &canonical_server_ids).await?;
    }

    if table_exists(&mut tx, "server_mappings").await? {
        let has_server_id = column_exists(&mut tx, "server_mappings", "server_id").await?;
        canonicalize_server_mappings(
            &mut tx,
            has_server_id,
            &canonical_server_ids,
            &canonical_server_urls,
        )
        .await?;
    }

    if table_exists(&mut tx, "media_mappings").await? {
        let has_server_id = column_exists(&mut tx, "media_mappings", "server_id").await?;
        canonicalize_media_mappings(
            &mut tx,
            has_server_id,
            &canonical_server_ids,
            &canonical_server_urls,
        )
        .await?;
    }

    if table_exists(&mut tx, "authorization_sessions").await?
        && table_exists(&mut tx, "server_mappings").await?
    {
        sqlx::query(
            r#"
            UPDATE authorization_sessions
            SET server_url = (
                SELECT server_mappings.server_url
                FROM server_mappings
                WHERE server_mappings.id = authorization_sessions.mapping_id
            )
            WHERE EXISTS (
                SELECT 1
                FROM server_mappings
                WHERE server_mappings.id = authorization_sessions.mapping_id
            )
            "#,
        )
        .execute(&mut *tx)
        .await?;
    }

    merge_servers(&mut tx, &servers).await?;

    tx.commit().await?;
    Ok(())
}

async fn table_exists(tx: &mut Transaction<'_, Sqlite>, table: &str) -> Result<bool> {
    let count = sqlx::query_scalar::<_, i64>(
        r#"
        SELECT COUNT(*)
        FROM sqlite_master
        WHERE type = 'table' AND name = ?
        "#,
    )
    .bind(table)
    .fetch_one(&mut **tx)
    .await?;

    Ok(count > 0)
}

async fn column_exists(
    tx: &mut Transaction<'_, Sqlite>,
    table: &str,
    column: &str,
) -> Result<bool> {
    let query = format!("SELECT COUNT(*) FROM pragma_table_info('{table}') WHERE name = ?");
    let count = sqlx::query_scalar::<_, i64>(&query)
        .bind(column)
        .fetch_one(&mut **tx)
        .await?;

    Ok(count > 0)
}

async fn load_servers(tx: &mut Transaction<'_, Sqlite>) -> Result<Vec<LegacyServer>> {
    let rows = sqlx::query("SELECT id, url FROM servers ORDER BY id ASC")
        .fetch_all(&mut **tx)
        .await?;

    rows.into_iter()
        .map(|row| {
            let id = row.get("id");
            let url: String = row.get("url");
            let canonical_url = ServerUrl::canonicalize(&url).with_context(|| {
                format!("Invalid server URL in database for server id {id}: {url}")
            })?;

            Ok(LegacyServer { id, canonical_url })
        })
        .collect()
}

fn canonical_server_ids(servers: &[LegacyServer]) -> HashMap<i64, i64> {
    let mut canonical_by_url = HashMap::<&str, i64>::new();
    for server in servers {
        canonical_by_url
            .entry(server.canonical_url.as_str())
            .and_modify(|id| *id = (*id).min(server.id))
            .or_insert(server.id);
    }

    servers
        .iter()
        .map(|server| {
            let canonical_id = canonical_by_url[server.canonical_url.as_str()];
            (server.id, canonical_id)
        })
        .collect()
}

fn canonical_server_urls(servers: &[LegacyServer]) -> HashMap<i64, String> {
    servers
        .iter()
        .map(|server| (server.id, server.canonical_url.clone()))
        .collect()
}

async fn merge_server_admins(
    tx: &mut Transaction<'_, Sqlite>,
    canonical_server_ids: &HashMap<i64, i64>,
) -> Result<()> {
    let rows = sqlx::query("SELECT id, server_id FROM server_admins ORDER BY id ASC")
        .fetch_all(&mut **tx)
        .await?;

    let mut admin_by_server = HashMap::<i64, i64>::new();
    let mut duplicate_admin_ids = Vec::new();

    for row in rows {
        let admin_id = row.get("id");
        let server_id = row.get("server_id");
        let Some(&canonical_server_id) = canonical_server_ids.get(&server_id) else {
            continue;
        };

        if let Some(existing_admin_id) = admin_by_server.get_mut(&canonical_server_id) {
            duplicate_admin_ids.push(admin_id);
            *existing_admin_id = (*existing_admin_id).min(admin_id);
        } else {
            admin_by_server.insert(canonical_server_id, admin_id);
        }
    }

    for admin_id in duplicate_admin_ids {
        sqlx::query("DELETE FROM server_admins WHERE id = ?")
            .bind(admin_id)
            .execute(&mut **tx)
            .await?;
    }

    for (canonical_server_id, admin_id) in admin_by_server {
        sqlx::query("UPDATE server_admins SET server_id = ? WHERE id = ?")
            .bind(canonical_server_id)
            .bind(admin_id)
            .execute(&mut **tx)
            .await?;
    }

    Ok(())
}

async fn canonicalize_server_mappings(
    tx: &mut Transaction<'_, Sqlite>,
    has_server_id: bool,
    canonical_server_ids: &HashMap<i64, i64>,
    canonical_server_urls: &HashMap<i64, String>,
) -> Result<()> {
    let rows = if has_server_id {
        sqlx::query(
            "SELECT id, user_id, server_id, server_url FROM server_mappings ORDER BY id ASC",
        )
        .fetch_all(&mut **tx)
        .await?
    } else {
        sqlx::query("SELECT id, user_id, NULL as server_id, server_url FROM server_mappings ORDER BY id ASC")
            .fetch_all(&mut **tx)
            .await?
    };

    let mut groups = HashMap::<(String, String), Vec<LegacyMapping>>::new();
    for row in rows {
        let server_id = row.get::<Option<i64>, _>("server_id");
        let server_url: String = row.get("server_url");
        let canonical_url = if let Some(server_id) = server_id {
            let canonical_server_id = canonical_server_ids
                .get(&server_id)
                .copied()
                .unwrap_or(server_id);
            canonical_server_urls
                .get(&canonical_server_id)
                .cloned()
                .ok_or_else(|| anyhow!("Missing canonical URL for server id {server_id}"))?
        } else {
            ServerUrl::canonicalize(&server_url).with_context(|| {
                format!(
                    "Invalid server mapping URL in database for mapping {}: {server_url}",
                    row.get::<i64, _>("id")
                )
            })?
        };

        groups
            .entry((row.get("user_id"), canonical_url))
            .or_default()
            .push(LegacyMapping {
                id: row.get("id"),
                server_id,
                server_url,
            });
    }

    for ((_, canonical_url), mut mappings) in groups {
        mappings.sort_by_key(|mapping| mapping.id);
        let canonical_mapping_id = mappings[0].id;
        let duplicate_mapping_ids = mappings
            .iter()
            .skip(1)
            .map(|mapping| mapping.id)
            .collect::<Vec<_>>();

        merge_authorization_sessions(tx, canonical_mapping_id, &duplicate_mapping_ids).await?;

        for duplicate_mapping_id in duplicate_mapping_ids {
            sqlx::query("DELETE FROM server_mappings WHERE id = ?")
                .bind(duplicate_mapping_id)
                .execute(&mut **tx)
                .await?;
        }

        if has_server_id {
            let server_id = mappings[0]
                .server_id
                .and_then(|server_id| canonical_server_ids.get(&server_id).copied())
                .ok_or_else(|| anyhow!("Missing server id for mapping {}", mappings[0].id))?;
            sqlx::query("UPDATE server_mappings SET server_id = ?, server_url = ? WHERE id = ?")
                .bind(server_id)
                .bind(&canonical_url)
                .bind(canonical_mapping_id)
                .execute(&mut **tx)
                .await?;
        } else if mappings[0].server_url != canonical_url {
            sqlx::query("UPDATE server_mappings SET server_url = ? WHERE id = ?")
                .bind(&canonical_url)
                .bind(canonical_mapping_id)
                .execute(&mut **tx)
                .await?;
        }
    }

    Ok(())
}

async fn merge_authorization_sessions(
    tx: &mut Transaction<'_, Sqlite>,
    canonical_mapping_id: i64,
    duplicate_mapping_ids: &[i64],
) -> Result<()> {
    if duplicate_mapping_ids.is_empty() || !table_exists(tx, "authorization_sessions").await? {
        return Ok(());
    }

    let mut mapping_ids = Vec::with_capacity(duplicate_mapping_ids.len() + 1);
    mapping_ids.push(canonical_mapping_id);
    mapping_ids.extend_from_slice(duplicate_mapping_ids);

    let has_updated_at = column_exists(tx, "authorization_sessions", "updated_at").await?;
    let has_created_at = column_exists(tx, "authorization_sessions", "created_at").await?;
    let order_by = match (has_updated_at, has_created_at) {
        (true, true) => "updated_at DESC, created_at DESC, id DESC",
        (true, false) => "updated_at DESC, id DESC",
        (false, true) => "created_at DESC, id DESC",
        (false, false) => "id DESC",
    };

    let mapping_placeholders = placeholders(mapping_ids.len());
    let rows = sqlx::query(&format!(
        "SELECT id, user_id, device_id FROM authorization_sessions WHERE mapping_id IN ({mapping_placeholders}) ORDER BY {order_by}"
    ))
    .try_bind_all(&mapping_ids)?
    .fetch_all(&mut **tx)
    .await?;

    let mut session_by_device = HashMap::<(String, String), i64>::new();
    let mut duplicate_session_ids = Vec::new();

    for row in rows {
        let session_id = row.get("id");
        let key = (row.get("user_id"), row.get("device_id"));
        if session_by_device.insert(key, session_id).is_some() {
            duplicate_session_ids.push(session_id);
        }
    }

    for session_id in duplicate_session_ids {
        sqlx::query("DELETE FROM authorization_sessions WHERE id = ?")
            .bind(session_id)
            .execute(&mut **tx)
            .await?;
    }

    let duplicate_placeholders = placeholders(duplicate_mapping_ids.len());
    sqlx::query(&format!(
        "UPDATE authorization_sessions SET mapping_id = ? WHERE mapping_id IN ({duplicate_placeholders})"
    ))
    .bind(canonical_mapping_id)
    .try_bind_all(duplicate_mapping_ids)?
    .execute(&mut **tx)
    .await?;

    Ok(())
}

async fn canonicalize_media_mappings(
    tx: &mut Transaction<'_, Sqlite>,
    has_server_id: bool,
    canonical_server_ids: &HashMap<i64, i64>,
    canonical_server_urls: &HashMap<i64, String>,
) -> Result<()> {
    let rows = if has_server_id {
        sqlx::query("SELECT id, original_media_id, server_id, server_url FROM media_mappings ORDER BY id ASC")
            .fetch_all(&mut **tx)
            .await?
    } else {
        sqlx::query("SELECT id, original_media_id, NULL as server_id, server_url FROM media_mappings ORDER BY id ASC")
            .fetch_all(&mut **tx)
            .await?
    };

    let mut groups = HashMap::<(String, String), Vec<LegacyMediaMapping>>::new();
    for row in rows {
        let server_id = row.get::<Option<i64>, _>("server_id");
        let server_url: String = row.get("server_url");
        let canonical_url = if let Some(server_id) = server_id {
            let canonical_server_id = canonical_server_ids
                .get(&server_id)
                .copied()
                .unwrap_or(server_id);
            canonical_server_urls
                .get(&canonical_server_id)
                .cloned()
                .ok_or_else(|| anyhow!("Missing canonical URL for server id {server_id}"))?
        } else {
            ServerUrl::canonicalize(&server_url).with_context(|| {
                format!(
                    "Invalid media mapping URL in database for mapping {}: {server_url}",
                    row.get::<i64, _>("id")
                )
            })?
        };

        groups
            .entry((row.get("original_media_id"), canonical_url))
            .or_default()
            .push(LegacyMediaMapping {
                id: row.get("id"),
                server_id,
                server_url,
            });
    }

    for ((_, canonical_url), mut mappings) in groups {
        mappings.sort_by_key(|mapping| mapping.id);
        let canonical_mapping_id = mappings[0].id;

        for duplicate_mapping in mappings.iter().skip(1) {
            sqlx::query("DELETE FROM media_mappings WHERE id = ?")
                .bind(duplicate_mapping.id)
                .execute(&mut **tx)
                .await?;
        }

        if has_server_id {
            let server_id = mappings[0]
                .server_id
                .and_then(|server_id| canonical_server_ids.get(&server_id).copied())
                .ok_or_else(|| anyhow!("Missing server id for media mapping {}", mappings[0].id))?;
            sqlx::query("UPDATE media_mappings SET server_id = ?, server_url = ? WHERE id = ?")
                .bind(server_id)
                .bind(&canonical_url)
                .bind(canonical_mapping_id)
                .execute(&mut **tx)
                .await?;
        } else if mappings[0].server_url != canonical_url {
            sqlx::query("UPDATE media_mappings SET server_url = ? WHERE id = ?")
                .bind(&canonical_url)
                .bind(canonical_mapping_id)
                .execute(&mut **tx)
                .await?;
        }
    }

    Ok(())
}

async fn merge_servers(tx: &mut Transaction<'_, Sqlite>, servers: &[LegacyServer]) -> Result<()> {
    let mut canonical_by_url = HashMap::<&str, i64>::new();
    for server in servers {
        canonical_by_url
            .entry(server.canonical_url.as_str())
            .and_modify(|id| *id = (*id).min(server.id))
            .or_insert(server.id);
    }

    for server in servers {
        let canonical_id = canonical_by_url[server.canonical_url.as_str()];
        if server.id != canonical_id {
            sqlx::query("DELETE FROM servers WHERE id = ?")
                .bind(server.id)
                .execute(&mut **tx)
                .await?;
        }
    }

    for server in servers {
        let canonical_id = canonical_by_url[server.canonical_url.as_str()];
        if server.id == canonical_id {
            sqlx::query("UPDATE servers SET url = ? WHERE id = ?")
                .bind(&server.canonical_url)
                .bind(server.id)
                .execute(&mut **tx)
                .await?;
        }
    }

    Ok(())
}

fn placeholders(count: usize) -> String {
    std::iter::repeat_n("?", count)
        .collect::<Vec<_>>()
        .join(",")
}

trait BindAll<'q> {
    fn try_bind_all(
        self,
        values: &'q [i64],
    ) -> Result<sqlx::query::Query<'q, Sqlite, sqlx::sqlite::SqliteArguments<'q>>>;
}

impl<'q> BindAll<'q> for sqlx::query::Query<'q, Sqlite, sqlx::sqlite::SqliteArguments<'q>> {
    fn try_bind_all(
        mut self,
        values: &'q [i64],
    ) -> Result<sqlx::query::Query<'q, Sqlite, sqlx::sqlite::SqliteArguments<'q>>> {
        for value in values {
            self = self.bind(*value);
        }
        Ok(self)
    }
}

#[cfg(test)]
mod tests {
    use sqlx::SqlitePool;

    use super::*;

    #[tokio::test]
    async fn cleanup_merges_canonical_server_identity_without_orphaning_sessions() {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();

        sqlx::query(
            r#"
            CREATE TABLE servers (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                name TEXT NOT NULL UNIQUE,
                url TEXT NOT NULL,
                priority INTEGER NOT NULL DEFAULT 100,
                created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
                updated_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
            )
            "#,
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            r#"
            CREATE TABLE server_admins (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                server_id INTEGER NOT NULL UNIQUE,
                username TEXT NOT NULL,
                password TEXT NOT NULL
            )
            "#,
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            r#"
            CREATE TABLE server_mappings (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                user_id TEXT NOT NULL,
                server_url TEXT NOT NULL,
                mapped_username TEXT NOT NULL,
                mapped_password TEXT NOT NULL,
                created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
                updated_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
                UNIQUE(user_id, server_url)
            )
            "#,
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            r#"
            CREATE TABLE authorization_sessions (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                user_id TEXT NOT NULL,
                mapping_id INTEGER NOT NULL,
                server_url TEXT NOT NULL,
                client TEXT NOT NULL,
                device TEXT NOT NULL,
                device_id TEXT NOT NULL,
                version TEXT NOT NULL,
                jellyfin_token TEXT,
                original_user_id TEXT NOT NULL,
                created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
                updated_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
                UNIQUE(user_id, mapping_id, device_id)
            )
            "#,
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            r#"
            CREATE TABLE media_mappings (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                virtual_media_id TEXT NOT NULL UNIQUE,
                original_media_id TEXT NOT NULL,
                server_url TEXT NOT NULL,
                created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
                UNIQUE(original_media_id, server_url)
            )
            "#,
        )
        .execute(&pool)
        .await
        .unwrap();

        sqlx::query("INSERT INTO servers (name, url) VALUES (?, ?), (?, ?), (?, ?)")
            .bind("one")
            .bind("https://Example.com/jellyfin///?x=1#fragment")
            .bind("duplicate")
            .bind("https://example.com/jellyfin/")
            .bind("other")
            .bind("https://other.example")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO server_admins (server_id, username, password) VALUES (1, 'a', 'p'), (2, 'b', 'p')")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO server_mappings (user_id, server_url, mapped_username, mapped_password) VALUES (?, ?, 'u', 'p'), (?, ?, 'u', 'p'), (?, ?, 'v', 'p')",
        )
        .bind("user")
        .bind("https://example.com/jellyfin/?query=1")
        .bind("user")
        .bind("https://example.com/jellyfin/")
        .bind("other-user")
        .bind("https://other.example/")
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            r#"
            INSERT INTO authorization_sessions
                (id, user_id, mapping_id, server_url, client, device, device_id, version, jellyfin_token, original_user_id, created_at, updated_at)
            VALUES
                (10, 'user', 1, 'https://example.com/jellyfin/?query=1', 'c', 'd', 'same-device', '1', 'token-old', 'orig', '2026-01-01 00:00:00Z', '2026-01-01 00:00:00Z'),
                (5, 'user', 2, 'https://example.com/jellyfin/', 'c', 'd', 'same-device', '1', 'token-new', 'orig', '2026-01-02 00:00:00Z', '2026-01-02 00:00:00Z'),
                (6, 'user', 2, 'https://example.com/jellyfin/', 'c', 'd', 'other-device', '1', 'token-other', 'orig', '2026-01-02 00:00:00Z', '2026-01-02 00:00:00Z')
            "#,
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO media_mappings (virtual_media_id, original_media_id, server_url) VALUES ('v1', 'item', ?), ('v2', 'item', ?)",
        )
        .bind("https://example.com/jellyfin/?query=1")
        .bind("https://example.com/jellyfin/")
        .execute(&pool)
        .await
        .unwrap();

        canonicalize_legacy_server_identity(&pool).await.unwrap();

        let server_count = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM servers")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(server_count, 2);

        let canonical_url = sqlx::query_scalar::<_, String>("SELECT url FROM servers WHERE id = 1")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(canonical_url, "https://example.com/jellyfin");

        let admin_count =
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM server_admins WHERE server_id = 1")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(admin_count, 1);

        let mapping_count = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM server_mappings WHERE user_id = 'user'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(mapping_count, 1);

        let orphaned_sessions = sqlx::query_scalar::<_, i64>(
            r#"
            SELECT COUNT(*)
            FROM authorization_sessions auth
            LEFT JOIN server_mappings mapping ON mapping.id = auth.mapping_id
            WHERE mapping.id IS NULL
            "#,
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(orphaned_sessions, 0);

        let session_count =
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM authorization_sessions")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(session_count, 2);

        let same_device_token = sqlx::query_scalar::<_, String>(
            "SELECT jellyfin_token FROM authorization_sessions WHERE device_id = 'same-device'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(same_device_token, "token-new");

        let media_count = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM media_mappings")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(media_count, 1);
    }
}
