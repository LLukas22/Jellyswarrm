//! Rate limiting service for protecting authentication endpoints

use governor::{
    clock::DefaultClock,
    state::{InMemoryState, NotKeyed},
    Quota, RateLimiter,
};
use std::{net::IpAddr, num::NonZeroU32, sync::Arc, time::Duration};
use moka::future::Cache;
use tracing::warn;

/// Rate limiter for authentication endpoints
/// Uses a per-IP rate limit to prevent brute force attacks
#[derive(Clone)]
pub struct AuthRateLimiter {
    /// Per-IP rate limiters cached for efficiency
    limiters: Cache<IpAddr, Arc<RateLimiter<NotKeyed, InMemoryState, DefaultClock>>>,
    /// Maximum requests per window
    max_requests: NonZeroU32,
    /// Time window for rate limiting
    window: Duration,
}

impl AuthRateLimiter {
    /// Create a new rate limiter
    ///
    /// # Arguments
    /// * `max_requests` - Maximum number of requests allowed per window
    /// * `window_secs` - Time window in seconds
    pub fn new(max_requests: u32, window_secs: u64) -> Self {
        let max_requests = NonZeroU32::new(max_requests).unwrap_or(NonZeroU32::new(5).unwrap());

        Self {
            limiters: Cache::builder()
                .time_to_idle(Duration::from_secs(window_secs * 10)) // Keep limiters around for 10x window
                .max_capacity(10_000) // Max 10k unique IPs tracked
                .build(),
            max_requests,
            window: Duration::from_secs(window_secs),
        }
    }

    /// Create with default settings (5 requests per 10 seconds)
    pub fn default_auth_limiter() -> Self {
        Self::new(5, 10)
    }

    /// Check if a request from the given IP should be allowed
    /// Returns true if allowed, false if rate limited
    pub async fn check(&self, ip: IpAddr) -> bool {
        let limiter = self.get_or_create_limiter(ip).await;

        match limiter.check() {
            Ok(_) => true,
            Err(_) => {
                warn!("Rate limit exceeded for IP: {}", ip);
                false
            }
        }
    }

    /// Get remaining requests for an IP (for headers)
    pub async fn remaining(&self, ip: IpAddr) -> u32 {
        let limiter = self.get_or_create_limiter(ip).await;
        limiter.check().map(|_| self.max_requests.get()).unwrap_or(0)
    }

    async fn get_or_create_limiter(
        &self,
        ip: IpAddr,
    ) -> Arc<RateLimiter<NotKeyed, InMemoryState, DefaultClock>> {
        if let Some(limiter) = self.limiters.get(&ip).await {
            return limiter;
        }

        let quota = Quota::with_period(self.window)
            .unwrap()
            .allow_burst(self.max_requests);

        let limiter = Arc::new(RateLimiter::direct(quota));
        self.limiters.insert(ip, limiter.clone()).await;
        limiter
    }
}

/// Extract client IP from request headers
/// Checks X-Forwarded-For, X-Real-IP, then falls back to peer address
pub fn extract_client_ip(headers: &axum::http::HeaderMap, peer_addr: Option<std::net::SocketAddr>) -> Option<IpAddr> {
    // Try X-Forwarded-For first (common for proxies)
    if let Some(xff) = headers.get("x-forwarded-for") {
        if let Ok(xff_str) = xff.to_str() {
            // Take the first IP in the chain (original client)
            if let Some(first_ip) = xff_str.split(',').next() {
                if let Ok(ip) = first_ip.trim().parse::<IpAddr>() {
                    return Some(ip);
                }
            }
        }
    }

    // Try X-Real-IP
    if let Some(real_ip) = headers.get("x-real-ip") {
        if let Ok(ip_str) = real_ip.to_str() {
            if let Ok(ip) = ip_str.trim().parse::<IpAddr>() {
                return Some(ip);
            }
        }
    }

    // Fall back to peer address
    peer_addr.map(|addr| addr.ip())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    #[tokio::test]
    async fn test_rate_limiter_allows_within_limit() {
        let limiter = AuthRateLimiter::new(3, 10);
        let ip = IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1));

        // First 3 requests should be allowed
        assert!(limiter.check(ip).await);
        assert!(limiter.check(ip).await);
        assert!(limiter.check(ip).await);
    }

    #[tokio::test]
    async fn test_rate_limiter_blocks_over_limit() {
        let limiter = AuthRateLimiter::new(2, 10);
        let ip = IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1));

        // First 2 requests should be allowed
        assert!(limiter.check(ip).await);
        assert!(limiter.check(ip).await);

        // Third request should be blocked
        assert!(!limiter.check(ip).await);
    }

    #[tokio::test]
    async fn test_different_ips_have_separate_limits() {
        let limiter = AuthRateLimiter::new(1, 10);
        let ip1 = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1));
        let ip2 = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 2));

        // Both IPs should be allowed their first request
        assert!(limiter.check(ip1).await);
        assert!(limiter.check(ip2).await);

        // Both should be blocked on second request
        assert!(!limiter.check(ip1).await);
        assert!(!limiter.check(ip2).await);
    }
}
