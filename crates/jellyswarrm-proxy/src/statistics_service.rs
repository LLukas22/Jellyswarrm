//! Statistics service for tracking usage metrics

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{Row, SqlitePool};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// Real-time statistics counters
#[derive(Debug, Default)]
pub struct RealtimeCounters {
    pub total_requests: AtomicU64,
    pub active_streams: AtomicU64,
    pub auth_attempts: AtomicU64,
    pub auth_failures: AtomicU64,
    pub rate_limited_requests: AtomicU64,
}

impl RealtimeCounters {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn increment_requests(&self) {
        self.total_requests.fetch_add(1, Ordering::Relaxed);
    }

    pub fn increment_streams(&self) {
        self.active_streams.fetch_add(1, Ordering::Relaxed);
    }

    pub fn decrement_streams(&self) {
        self.active_streams.fetch_sub(1, Ordering::Relaxed);
    }

    pub fn increment_auth_attempts(&self) {
        self.auth_attempts.fetch_add(1, Ordering::Relaxed);
    }

    pub fn increment_auth_failures(&self) {
        self.auth_failures.fetch_add(1, Ordering::Relaxed);
    }

    pub fn increment_rate_limited(&self) {
        self.rate_limited_requests.fetch_add(1, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> RealtimeStats {
        RealtimeStats {
            total_requests: self.total_requests.load(Ordering::Relaxed),
            active_streams: self.active_streams.load(Ordering::Relaxed),
            auth_attempts: self.auth_attempts.load(Ordering::Relaxed),
            auth_failures: self.auth_failures.load(Ordering::Relaxed),
            rate_limited_requests: self.rate_limited_requests.load(Ordering::Relaxed),
        }
    }
}

/// Snapshot of realtime statistics
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RealtimeStats {
    pub total_requests: u64,
    pub active_streams: u64,
    pub auth_attempts: u64,
    pub auth_failures: u64,
    pub rate_limited_requests: u64,
}

/// Per-server statistics
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerStats {
    pub server_id: i64,
    pub server_name: String,
    pub total_users: i64,
    pub total_sessions: i64,
    pub total_mappings: i64,
}

/// Per-user statistics
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserStats {
    pub user_id: String,
    pub username: String,
    pub total_sessions: i64,
    pub total_mappings: i64,
    pub last_login: Option<DateTime<Utc>>,
}

/// Overall system statistics
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemStats {
    pub total_users: i64,
    pub total_servers: i64,
    pub total_admins: i64,
    pub total_sessions: i64,
    pub total_mappings: i64,
    pub total_media_mappings: i64,
    pub total_audit_logs: i64,
    pub database_size_bytes: i64,
    pub uptime_seconds: u64,
    pub realtime: RealtimeStats,
}

/// Statistics service
#[derive(Clone)]
pub struct StatisticsService {
    pool: SqlitePool,
    counters: Arc<RealtimeCounters>,
    start_time: std::time::Instant,
}

impl StatisticsService {
    pub fn new(pool: SqlitePool) -> Self {
        Self {
            pool,
            counters: Arc::new(RealtimeCounters::new()),
            start_time: std::time::Instant::now(),
        }
    }

    /// Get the realtime counters for incrementing
    pub fn counters(&self) -> &Arc<RealtimeCounters> {
        &self.counters
    }

    /// Get overall system statistics
    pub async fn get_system_stats(&self) -> Result<SystemStats, sqlx::Error> {
        // Count users
        let users: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM users")
            .fetch_one(&self.pool)
            .await?;

        // Count servers
        let servers: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM servers")
            .fetch_one(&self.pool)
            .await?;

        // Count admins
        let admins: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM admin_users")
            .fetch_one(&self.pool)
            .await
            .unwrap_or((0,));

        // Count sessions
        let sessions: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM authorization_sessions")
            .fetch_one(&self.pool)
            .await?;

        // Count mappings
        let mappings: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM server_mappings")
            .fetch_one(&self.pool)
            .await?;

        // Count media mappings
        let media_mappings: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM media_mappings")
            .fetch_one(&self.pool)
            .await?;

        // Count audit logs
        let audit_logs: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM audit_logs")
            .fetch_one(&self.pool)
            .await
            .unwrap_or((0,));

        // Get database size (approximate via page count)
        let db_size: i64 = sqlx::query("SELECT page_count * page_size as size FROM pragma_page_count(), pragma_page_size()")
            .fetch_one(&self.pool)
            .await
            .map(|row| row.get::<i64, _>("size"))
            .unwrap_or(0);

        Ok(SystemStats {
            total_users: users.0,
            total_servers: servers.0,
            total_admins: admins.0,
            total_sessions: sessions.0,
            total_mappings: mappings.0,
            total_media_mappings: media_mappings.0,
            total_audit_logs: audit_logs.0,
            database_size_bytes: db_size,
            uptime_seconds: self.start_time.elapsed().as_secs(),
            realtime: self.counters.snapshot(),
        })
    }

    /// Get per-server statistics
    pub async fn get_server_stats(&self) -> Result<Vec<ServerStats>, sqlx::Error> {
        let rows = sqlx::query(
            r#"
            SELECT
                s.id as server_id,
                s.name as server_name,
                COUNT(DISTINCT sm.user_id) as total_users,
                COUNT(DISTINCT auth.id) as total_sessions,
                COUNT(DISTINCT sm.id) as total_mappings
            FROM servers s
            LEFT JOIN server_mappings sm ON RTRIM(s.url, '/') = RTRIM(sm.server_url, '/')
            LEFT JOIN authorization_sessions auth ON RTRIM(s.url, '/') = RTRIM(auth.server_url, '/')
            GROUP BY s.id, s.name
            ORDER BY s.priority DESC, s.name ASC
            "#,
        )
        .fetch_all(&self.pool)
        .await?;

        let stats = rows
            .into_iter()
            .map(|row| ServerStats {
                server_id: row.get("server_id"),
                server_name: row.get("server_name"),
                total_users: row.get("total_users"),
                total_sessions: row.get("total_sessions"),
                total_mappings: row.get("total_mappings"),
            })
            .collect();

        Ok(stats)
    }

    /// Get top users by session count
    pub async fn get_top_users(&self, limit: i64) -> Result<Vec<UserStats>, sqlx::Error> {
        let rows = sqlx::query(
            r#"
            SELECT
                u.id as user_id,
                u.original_username as username,
                COUNT(DISTINCT auth.id) as total_sessions,
                COUNT(DISTINCT sm.id) as total_mappings,
                u.last_login_at
            FROM users u
            LEFT JOIN server_mappings sm ON u.id = sm.user_id
            LEFT JOIN authorization_sessions auth ON u.id = auth.user_id
            GROUP BY u.id, u.original_username
            ORDER BY total_sessions DESC
            LIMIT ?
            "#,
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;

        let stats = rows
            .into_iter()
            .map(|row| UserStats {
                user_id: row.get("user_id"),
                username: row.get("username"),
                total_sessions: row.get("total_sessions"),
                total_mappings: row.get("total_mappings"),
                last_login: row.get("last_login_at"),
            })
            .collect();

        Ok(stats)
    }

    /// Get activity statistics for the last N hours
    pub async fn get_hourly_activity(&self, hours: i64) -> Result<Vec<(String, i64)>, sqlx::Error> {
        let rows = sqlx::query(
            r#"
            SELECT
                strftime('%Y-%m-%d %H:00', created_at) as hour,
                COUNT(*) as count
            FROM audit_logs
            WHERE created_at > datetime('now', ? || ' hours')
            GROUP BY hour
            ORDER BY hour ASC
            "#,
        )
        .bind(-hours)
        .fetch_all(&self.pool)
        .await
        .unwrap_or_default();

        let activity = rows
            .into_iter()
            .map(|row| (row.get::<String, _>("hour"), row.get::<i64, _>("count")))
            .collect();

        Ok(activity)
    }
}
