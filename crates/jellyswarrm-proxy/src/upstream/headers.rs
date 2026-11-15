//! Filter hop-by-hop and proxy-inappropriate headers for outgoing and streamed HTTP traffic.
use http::{HeaderMap, HeaderName};

/// Client transport preferences do not describe the proxy-to-backend connection.
/// Let reqwest derive Host and advertise only encodings its client can decode.
/// Media delivery overrides Accept-Encoding with identity before sending.
pub(crate) fn prepare_request_headers(headers: &mut HeaderMap) {
    remove_hop_by_hop_headers(headers);
    headers.remove(http::header::HOST);
    headers.remove(http::header::ACCEPT_ENCODING);
}

pub(crate) fn remove_hop_by_hop_headers(headers: &mut HeaderMap) {
    // Connection can nominate additional hop-by-hop fields, across multiple lines.
    let nominated: Vec<HeaderName> = headers
        .get_all(http::header::CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .filter_map(|name| HeaderName::from_bytes(name.trim().as_bytes()).ok())
        .collect();
    for name in nominated {
        headers.remove(name);
    }
    headers.remove(hyper::header::CONNECTION);
    headers.remove(HeaderName::from_static("keep-alive"));
    headers.remove(hyper::header::PROXY_AUTHENTICATE);
    headers.remove(hyper::header::PROXY_AUTHORIZATION);
    headers.remove(hyper::header::TE);
    headers.remove(hyper::header::TRAILER);
    headers.remove(HeaderName::from_static("trailers"));
    headers.remove(hyper::header::TRANSFER_ENCODING);
    headers.remove(hyper::header::UPGRADE);
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::header::{HeaderValue, CONNECTION};

    #[test]
    fn removes_all_connection_nominated_fields_and_preserves_end_to_end_headers() {
        let mut headers = HeaderMap::new();
        headers.append(
            CONNECTION,
            HeaderValue::from_static("keep-alive, X-Transport"),
        );
        headers.append(
            CONNECTION,
            HeaderValue::from_static("x-other, , invalid name"),
        );
        for name in [
            "keep-alive",
            "x-transport",
            "x-other",
            "transfer-encoding",
            "upgrade",
        ] {
            headers.insert(name, HeaderValue::from_static("hop"));
        }
        headers.insert("x-extension", HeaderValue::from_static("retained"));
        headers.insert("content-encoding", HeaderValue::from_static("gzip"));
        remove_hop_by_hop_headers(&mut headers);
        assert_eq!(headers.len(), 2);
        assert_eq!(headers["x-extension"], "retained");
        assert_eq!(headers["content-encoding"], "gzip");
    }
}
