//! Render transport-neutral delivery results at the HTTP boundary.

use axum::{body::Body, response::Response};
use hyper::StatusCode;

use crate::upstream::jellyfin::delivery::{DeliveryBody, DeliveryResponse};

pub(super) fn response(delivery: DeliveryResponse) -> Result<Response, StatusCode> {
    let body = match delivery.body {
        DeliveryBody::Empty => Body::empty(),
        DeliveryBody::Stream(stream) => Body::from_stream(stream),
        DeliveryBody::Playlist(text) => Body::from(text),
    };
    let mut response = Response::builder()
        .status(delivery.status)
        .body(body)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    *response.headers_mut() = delivery.headers;
    Ok(response)
}
