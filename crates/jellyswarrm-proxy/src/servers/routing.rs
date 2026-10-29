//! Stable backend preference shared by media, libraries, and routing workflows.
use super::Server;
use std::cmp::Ordering;

pub(crate) fn compare_routes(
    left_server: &Server,
    left_original_id: &str,
    right_server: &Server,
    right_original_id: &str,
) -> Ordering {
    left_server
        .priority
        .cmp(&right_server.priority)
        .then_with(|| right_server.name.cmp(&left_server.name))
        .then_with(|| right_server.id.as_i64().cmp(&left_server.id.as_i64()))
        .then_with(|| left_original_id.cmp(right_original_id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{config::MediaStreamingMode, domain::server_id::ServerId, servers::url::ServerUrl};

    fn server(priority: i32, name: &str, id: i64) -> Server {
        Server {
            priority,
            name: name.into(),
            id: ServerId::new(id),
            url: ServerUrl::parse("http://backend.invalid").unwrap(),
            media_streaming_mode: MediaStreamingMode::Redirect,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        }
    }

    #[test]
    fn route_order_retains_all_tie_breakers() {
        for (left, left_id, right, right_id, expected) in [
            (
                server(1, "z", 9),
                "z",
                server(0, "a", 1),
                "a",
                Ordering::Greater,
            ),
            (
                server(0, "a", 9),
                "z",
                server(0, "b", 1),
                "a",
                Ordering::Greater,
            ),
            (
                server(0, "a", 1),
                "z",
                server(0, "a", 2),
                "a",
                Ordering::Greater,
            ),
            (
                server(0, "a", 1),
                "a",
                server(0, "a", 1),
                "b",
                Ordering::Less,
            ),
            (
                server(0, "a", 1),
                "a",
                server(0, "a", 1),
                "a",
                Ordering::Equal,
            ),
        ] {
            assert_eq!(compare_routes(&left, left_id, &right, right_id), expected);
            assert_eq!(
                compare_routes(&right, right_id, &left, left_id),
                expected.reverse()
            );
        }
    }
}
