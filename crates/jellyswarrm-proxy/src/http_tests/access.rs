//! Negative and transport variants shared by the library/streaming contracts.
use super::*;
use wiremock::{
    matchers::{method, path, query_param},
    Mock, ResponseTemplate,
};

#[tokio::test]
async fn source_owned_requests_reject_foreign_viewers_before_upstream() {
    let f = Fixture::new().await;
    f.healthy().await;
    for (verb, route, body) in [
        (
            reqwest::Method::GET,
            format!("/Items/{}", f.items[1]),
            Value::Null,
        ),
        (
            reqwest::Method::GET,
            format!("/Videos/{}/stream.mp4", f.items[1]),
            Value::Null,
        ),
        (
            reqwest::Method::GET,
            format!("/Items/{}/Images/Primary", f.items[1]),
            Value::Null,
        ),
        (
            reqwest::Method::POST,
            format!("/Items/{}/PlaybackInfo", f.items[1]),
            json!({"DeviceProfile":{}}),
        ),
        (
            reqwest::Method::DELETE,
            format!("/UserPlayedItems/{}?UserId={}", f.items[1], f.other.id),
            Value::Null,
        ),
        (
            reqwest::Method::POST,
            format!("/Playlists/{}/Items?Ids={}", f.items[1], f.items[1]),
            Value::Null,
        ),
    ] {
        let mut request = f.request(verb, &route, &f.other, "tv");
        if !body.is_null() {
            request = request.json(&body);
        }
        let response = request.send().await.unwrap();
        assert!(
            !response.status().is_success(),
            "foreign request unexpectedly accepted: {route}"
        );
    }
    f.no_upstream_requests().await;
}

#[tokio::test]
async fn source_owned_read_propagates_upstream_errors_without_other_backend_fallback() {
    for status in [401, 403, 404, 500, 503] {
        let f = Fixture::new().await;
        f.healthy().await;
        let original = f.original(1).await;
        Mock::given(method("GET"))
            .and(path(format!("/Items/{original}")))
            .respond_with(ResponseTemplate::new(status))
            .expect(1)
            .mount(&f.upstreams[1])
            .await;
        let response = f
            .request(
                reqwest::Method::GET,
                &format!("/Items/{}", f.items[1]),
                &f.user,
                "tv",
            )
            .send()
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), status);
        assert!(f.upstreams[0].received_requests().await.unwrap().is_empty());
    }
}

#[tokio::test]
async fn header_and_query_authentication_translate_to_the_same_backend_identity() {
    let f = Fixture::new().await;
    f.healthy().await;
    let original = f.original(1).await;
    Mock::given(method("GET"))
        .and(path(format!("/Videos/{original}/stream.mp4")))
        .and(query_param("Static", "true"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"media"))
        .expect(3)
        .mount(&f.upstreams[1])
        .await;
    for mode in ["Authorization", "X-MediaBrowser-Token", "ApiKey"] {
        let mut request = reqwest::Client::new().get(format!(
            "{}/Videos/{}/stream.mp4?Static=true&DeviceId=tv",
            f.url, f.items[1]
        ));
        request = match mode {
            "Authorization" => request.header(mode, auth(&f.user, "tv")),
            "ApiKey" => request.query(&[(mode, &f.user.virtual_key)]),
            _ => request.header(mode, &f.user.virtual_key),
        };
        let response = request.send().await.unwrap();
        assert_eq!(response.status(), StatusCode::OK, "{mode}");
        assert_eq!(response.bytes().await.unwrap().as_ref(), b"media");
    }
    let requests = f.upstreams[1].received_requests().await.unwrap();
    for request in requests {
        let identity = format!("{:?} {}", request.headers, request.url);
        assert!(identity.contains("backend-token-1"));
        assert!(!identity.contains(&f.user.virtual_key));
    }
    assert!(f.upstreams[0].received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn normalized_uuid_and_lowercase_route_have_the_same_owner() {
    let f = Fixture::new().await;
    f.healthy().await;
    let original = f.original(1).await;
    let id = uuid::Uuid::parse_str(&f.items[1])
        .unwrap()
        .hyphenated()
        .to_string()
        .to_uppercase();
    for (prefix, item) in [("Items", f.items[1].as_str()), ("items", id.as_str())] {
        Mock::given(method("GET"))
            .and(path(format!("/{prefix}/{original}")))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"Id":original,"Type":"Movie","Name":"Movie"})),
            )
            .expect(1)
            .mount(&f.upstreams[1])
            .await;
        let response = f
            .request(
                reqwest::Method::GET,
                &format!("/{prefix}/{item}?userid={}", f.user.id),
                &f.user,
                "tv",
            )
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.json::<Value>().await.unwrap()["Id"], f.items[1]);
    }
    assert!(f.upstreams[0].received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn head_on_image_resource_preserves_metadata_without_response_bytes() {
    let f = Fixture::new().await;
    f.healthy().await;
    let original = f.original(1).await;
    Mock::given(method("HEAD"))
        .and(path(format!("/Items/{original}/Images/Primary")))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Content-Type", "image/jpeg")
                .insert_header("ETag", "image-tag"),
        )
        .expect(1)
        .mount(&f.upstreams[1])
        .await;
    let response = f
        .request(
            reqwest::Method::HEAD,
            &format!("/Items/{}/Images/Primary", f.items[1]),
            &f.user,
            "tv",
        )
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["ETag"], "image-tag");
    assert!(response.bytes().await.unwrap().is_empty());
    assert!(f.upstreams[0].received_requests().await.unwrap().is_empty());
}
