//! Global database write coordinator for SQLite
//!
//! SQLite only allows ONE writer at a time. This module provides a global semaphore
//! that ALL services must use when performing write operations (INSERT, UPDATE, DELETE).
//!
//! Without this coordination, multiple services competing for writes cause severe
//! lock contention, resulting in 30-60 second query times and pool timeouts.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{Semaphore, SemaphorePermit};
use tracing::{debug, trace, warn};

/// Maximum time to wait for the write semaphore before giving up
const DEFAULT_WRITE_TIMEOUT: Duration = Duration::from_secs(30);

/// Number of retries when waiting for write access
const MAX_WRITE_RETRIES: u32 = 5;

/// Delay between retry attempts
const RETRY_DELAY: Duration = Duration::from_millis(200);

/// Global write guard that serializes all database writes across the application.
///
/// # Usage
/// ```rust
/// let guard = DbWriteGuard::new();
///
/// // In your service:
/// {
///     let _permit = guard.acquire_write().await?;
///     // Perform write operation while holding permit
///     sqlx::query("INSERT ...").execute(&pool).await?;
/// } // permit dropped, next writer can proceed
/// ```
#[derive(Clone, Debug)]
pub struct DbWriteGuard {
    /// Single-permit semaphore ensures only one writer at a time
    semaphore: Arc<Semaphore>,
    /// Maximum time to wait for write access
    timeout: Duration,
}

impl DbWriteGuard {
    /// Create a new write guard with default timeout
    pub fn new() -> Self {
        Self {
            semaphore: Arc::new(Semaphore::new(1)),
            timeout: DEFAULT_WRITE_TIMEOUT,
        }
    }

    /// Create a new write guard with custom timeout
    pub fn with_timeout(timeout: Duration) -> Self {
        Self {
            semaphore: Arc::new(Semaphore::new(1)),
            timeout,
        }
    }

    /// Acquire exclusive write access.
    ///
    /// Returns a permit that must be held while performing write operations.
    /// The permit is automatically released when dropped.
    ///
    /// Returns an error if the timeout is exceeded.
    pub async fn acquire_write(&self) -> Result<SemaphorePermit<'_>, WriteGuardError> {
        for attempt in 0..MAX_WRITE_RETRIES {
            match tokio::time::timeout(self.timeout / MAX_WRITE_RETRIES as u32, self.semaphore.acquire()).await {
                Ok(Ok(permit)) => {
                    if attempt > 0 {
                        debug!("Acquired write lock after {} attempts", attempt + 1);
                    } else {
                        trace!("Acquired write lock immediately");
                    }
                    return Ok(permit);
                }
                Ok(Err(_)) => {
                    // Semaphore closed - shouldn't happen
                    return Err(WriteGuardError::SemaphoreClosed);
                }
                Err(_) => {
                    // Timeout on this attempt
                    if attempt < MAX_WRITE_RETRIES - 1 {
                        trace!(
                            "Write lock attempt {} timed out, retrying in {:?}...",
                            attempt + 1,
                            RETRY_DELAY
                        );
                        tokio::time::sleep(RETRY_DELAY).await;
                    }
                }
            }
        }

        warn!(
            "Failed to acquire write lock after {:?} ({} attempts)",
            self.timeout, MAX_WRITE_RETRIES
        );
        Err(WriteGuardError::Timeout)
    }

    /// Try to acquire write access without waiting.
    ///
    /// Returns None if the semaphore is currently held.
    pub fn try_acquire_write(&self) -> Option<SemaphorePermit<'_>> {
        self.semaphore.try_acquire().ok()
    }

    /// Check if a write operation is currently in progress
    pub fn is_write_in_progress(&self) -> bool {
        self.semaphore.available_permits() == 0
    }
}

impl Default for DbWriteGuard {
    fn default() -> Self {
        Self::new()
    }
}

/// Errors that can occur when acquiring write access
#[derive(Debug, Clone)]
pub enum WriteGuardError {
    /// Timed out waiting for write access
    Timeout,
    /// The semaphore was closed (internal error)
    SemaphoreClosed,
}

impl std::fmt::Display for WriteGuardError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WriteGuardError::Timeout => write!(f, "Timed out waiting for database write access"),
            WriteGuardError::SemaphoreClosed => write!(f, "Database write semaphore was closed"),
        }
    }
}

impl std::error::Error for WriteGuardError {}

impl From<WriteGuardError> for sqlx::Error {
    fn from(err: WriteGuardError) -> Self {
        match err {
            WriteGuardError::Timeout => sqlx::Error::PoolTimedOut,
            WriteGuardError::SemaphoreClosed => sqlx::Error::PoolClosed,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_single_writer() {
        let guard = DbWriteGuard::new();

        let permit = guard.acquire_write().await.unwrap();
        assert!(guard.is_write_in_progress());
        drop(permit);
        assert!(!guard.is_write_in_progress());
    }

    #[tokio::test]
    async fn test_try_acquire() {
        let guard = DbWriteGuard::new();

        let permit1 = guard.try_acquire_write();
        assert!(permit1.is_some());

        let permit2 = guard.try_acquire_write();
        assert!(permit2.is_none());

        drop(permit1);

        let permit3 = guard.try_acquire_write();
        assert!(permit3.is_some());
    }

    #[tokio::test]
    async fn test_concurrent_writers_serialize() {
        use std::sync::atomic::{AtomicU32, Ordering};
        use tokio::time::sleep;

        let guard = Arc::new(DbWriteGuard::with_timeout(Duration::from_secs(5)));
        let counter = Arc::new(AtomicU32::new(0));
        let max_concurrent = Arc::new(AtomicU32::new(0));

        let mut handles = vec![];

        for _ in 0..5 {
            let guard = guard.clone();
            let counter = counter.clone();
            let max_concurrent = max_concurrent.clone();

            handles.push(tokio::spawn(async move {
                let _permit = guard.acquire_write().await.unwrap();

                // Increment counter
                let current = counter.fetch_add(1, Ordering::SeqCst) + 1;
                max_concurrent.fetch_max(current, Ordering::SeqCst);

                // Simulate write operation
                sleep(Duration::from_millis(10)).await;

                // Decrement counter
                counter.fetch_sub(1, Ordering::SeqCst);
            }));
        }

        for handle in handles {
            handle.await.unwrap();
        }

        // Max concurrent should never exceed 1 (serialized writes)
        assert_eq!(max_concurrent.load(Ordering::SeqCst), 1);
    }
}
