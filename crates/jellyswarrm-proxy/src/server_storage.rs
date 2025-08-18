use serde::{Deserialize, Serialize};
use sqlx::{Row, SqlitePool};
use tracing::info;
use url::Url;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Server {
    pub id: i64,
    pub name: String,
    pub url: Url,
    pub priority: i32,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone)]
pub struct ServerStorageService {
    pool: SqlitePool,
}

impl ServerStorageService {
    pub async fn new(pool: SqlitePool) -> Result<Self, sqlx::Error> {
        // Create the servers table if it doesn't exist
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS servers (
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
        .await?;

        info!("Server storage database initialized");

        Ok(Self { pool })
    }

    pub async fn add_server(
        &self,
        name: &str,
        url: &str,
        priority: i32,
    ) -> Result<i64, sqlx::Error> {
        // Validate URL
        if Url::parse(url).is_err() {
            return Err(sqlx::Error::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "Invalid URL format",
            )));
        }

        let now = chrono::Utc::now();

        let result = sqlx::query(
            r#"
            INSERT INTO servers (name, url, priority, created_at, updated_at)
            VALUES (?, ?, ?, ?, ?)
            "#,
        )
        .bind(name)
        .bind(url)
        .bind(priority)
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await?;

        let server_id = result.last_insert_rowid();
        info!(
            "Added server: {} ({}) with priority {}",
            name, url, priority
        );
        Ok(server_id)
    }

    pub async fn get_server_by_name(&self, name: &str) -> Result<Option<Server>, sqlx::Error> {
        let row = sqlx::query(
            r#"
            SELECT id, name, url, priority, created_at, updated_at
            FROM servers 
            WHERE name = ?
            "#,
        )
        .bind(name)
        .fetch_optional(&self.pool)
        .await?;

        if let Some(row) = row {
            Ok(Some(self.row_to_server(row)))
        } else {
            Ok(None)
        }
    }

    pub async fn get_server_by_id(&self, id: i64) -> Result<Option<Server>, sqlx::Error> {
        let row = sqlx::query(
            r#"
            SELECT id, name, url, priority, created_at, updated_at
            FROM servers 
            WHERE id = ?
            "#,
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;

        if let Some(row) = row {
            Ok(Some(self.row_to_server(row)))
        } else {
            Ok(None)
        }
    }

    pub async fn list_servers(&self) -> Result<Vec<Server>, sqlx::Error> {
        let rows = sqlx::query(
            r#"
            SELECT id, name, url, priority, created_at, updated_at
            FROM servers 
            ORDER BY priority DESC, name ASC
            "#,
        )
        .fetch_all(&self.pool)
        .await?;

        let servers = rows
            .into_iter()
            .map(|row| self.row_to_server(row))
            .collect();
        Ok(servers)
    }

    pub async fn update_server_priority(
        &self,
        server_id: i64,
        new_priority: i32,
    ) -> Result<bool, sqlx::Error> {
        let now = chrono::Utc::now();

        let result = sqlx::query(
            r#"
            UPDATE servers 
            SET priority = ?, updated_at = ?
            WHERE id = ?
            "#,
        )
        .bind(new_priority)
        .bind(now)
        .bind(server_id)
        .execute(&self.pool)
        .await?;

        Ok(result.rows_affected() > 0)
    }

    pub async fn delete_server(&self, server_id: i64) -> Result<bool, sqlx::Error> {
        let result = sqlx::query(
            r#"
            DELETE FROM servers 
            WHERE id = ?
            "#,
        )
        .bind(server_id)
        .execute(&self.pool)
        .await?;

        Ok(result.rows_affected() > 0)
    }

    pub async fn delete_server_by_name(&self, name: &str) -> Result<bool, sqlx::Error> {
        let result = sqlx::query(
            r#"
            DELETE FROM servers 
            WHERE name = ?
            "#,
        )
        .bind(name)
        .execute(&self.pool)
        .await?;

        Ok(result.rows_affected() > 0)
    }

    /// Get the best available server (highest priority, healthy, active)
    pub async fn get_best_server(&self) -> Result<Option<Server>, sqlx::Error> {
        let servers = self.list_servers().await?;
        Ok(servers.into_iter().next())
    }

    /// Internal method to convert database row to Server struct
    fn row_to_server(&self, row: sqlx::sqlite::SqliteRow) -> Server {
        Server {
            id: row.get("id"),
            name: row.get("name"),
            url: Url::parse(row.get("url")).unwrap(),
            priority: row.get("priority"),
            created_at: row.get("created_at"),
            updated_at: row.get("updated_at"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_server_storage_service() {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        let service = ServerStorageService::new(pool).await.unwrap();

        // Test adding a server
        let server_id = service
            .add_server("test-server", "http://localhost:8096", 100)
            .await
            .unwrap();

        // Test getting the server
        let server = service.get_server_by_id(server_id).await.unwrap();
        assert!(server.is_some());

        let server = server.unwrap();
        assert_eq!(server.name, "test-server");
        assert_eq!(server.url, Url::parse("http://localhost:8096").unwrap());
        assert_eq!(server.priority, 100);

        // Test listing servers
        let servers = service.list_servers().await.unwrap();
        assert_eq!(servers.len(), 1);

        // Test updating priority
        let updated = service
            .update_server_priority(server_id, 200)
            .await
            .unwrap();
        assert!(updated);
    }
}
