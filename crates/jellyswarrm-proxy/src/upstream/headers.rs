//! Filter hop-by-hop and proxy-inappropriate headers for outgoing and streamed HTTP traffic.
use http::{HeaderMap, HeaderName};

pub(crate) fn remove_hop_by_hop_headers(headers: &mut HeaderMap) {
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

pub(crate) fn is_hop_by_hop_header(name: &HeaderName) -> bool {
    matches!(
        name.as_str(),
        "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailer"
            | "trailers"
            | "transfer-encoding"
            | "upgrade"
    )
}
