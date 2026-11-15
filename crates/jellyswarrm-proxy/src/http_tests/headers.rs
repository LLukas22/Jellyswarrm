//! Upstream compression is negotiated by the proxy, independently of the browser.
use super::*;
use std::io::Write;
use wiremock::{matchers::path, Mock, ResponseTemplate};

fn compressed_response(document: Value, encoding: &str) -> ResponseTemplate {
    let json = serde_json::to_vec(&document).unwrap();
    let compressed = match encoding {
        "gzip" => {
            let mut encoder =
                flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
            encoder.write_all(&json).unwrap();
            encoder.finish().unwrap()
        }
        "zstd" => zstd::stream::encode_all(json.as_slice(), 0).unwrap(),
        _ => panic!("unsupported test encoding"),
    };
    ResponseTemplate::new(200)
        .insert_header("Content-Type", "application/json")
        .insert_header("Content-Encoding", encoding)
        .set_body_bytes(compressed)
}

async fn compression_contract(catalog: bool, encoding: &str) {
    let f = Fixture::for_scenario(Scenario::NormalServers).await;
    f.healthy().await;
    let route = if catalog {
        "/Items"
    } else {
        "/Custom/Compression"
    };
    for index in 0..2 {
        let document = if catalog {
            json!({"Items":[{"Id":f.original(index).await,"Name":format!("Compressed {index}"),"Type":"Movie"}],"TotalRecordCount":1,"StartIndex":0})
        } else {
            json!({"Message":"Decoded upstream JSON"})
        };
        let compressed = compressed_response(document, encoding);
        Mock::given(path(route))
            .respond_with(move |request: &wiremock::Request| {
                let encoding = request
                    .headers
                    .get("accept-encoding")
                    .unwrap()
                    .to_str()
                    .unwrap();
                let encodings: Vec<_> = encoding.split(',').map(str::trim).collect();
                assert!(
                    encodings.contains(&"gzip"),
                    "proxy must advertise its decoder"
                );
                assert!(encodings.contains(&"zstd"));
                assert!(!encodings.contains(&"future-encoding"));
                assert!(!request.headers.contains_key("x-client-connection"));
                assert_eq!(request.headers["x-extension"], "retained");
                compressed.clone()
            })
            .mount(&f.upstreams[index])
            .await;
    }
    // Also exercise a caller that requests identity: it constrains only the
    // client-facing connection, while the proxy can still decode upstream compression.
    let client = reqwest::Client::builder()
        .no_gzip()
        .no_brotli()
        .no_deflate()
        .no_zstd()
        .build()
        .unwrap();
    for client_encoding in ["gzip, deflate, br, zstd, future-encoding", "identity"] {
        f.state
            .services
            .catalog_snapshots
            .invalidate_viewer(&f.user.id);
        let response = client
            .get(format!("{}{route}", f.url))
            .header("Authorization", auth(&f.user, "tv"))
            .header("Accept-Encoding", client_encoding)
            .header("Connection", "x-client-connection")
            .header("x-client-connection", "hop-only")
            .header("x-extension", "retained")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(!response.headers().contains_key("content-encoding"));
        let result: Value = response.json().await.unwrap();
        if catalog {
            assert_eq!(result["TotalRecordCount"], 2);
            let ids: Vec<_> = result["Items"]
                .as_array()
                .unwrap()
                .iter()
                .map(|item| item["Id"].as_str().unwrap())
                .collect();
            for item in &f.items {
                assert!(ids.contains(&item.as_str()));
            }
        } else {
            assert_eq!(result["Message"], "Decoded upstream JSON");
        }
    }
    let requests: Vec<_> = futures_util::future::join_all(
        f.upstreams
            .iter()
            .map(|upstream| upstream.received_requests()),
    )
    .await;
    assert!(requests.iter().any(|requests| requests
        .as_ref()
        .unwrap()
        .iter()
        .any(|request| request.url.path() == route)));
}

#[tokio::test]
async fn catalog_decodes_compression_negotiated_by_proxy() {
    compression_contract(true, "gzip").await;
}

#[tokio::test]
async fn forwarding_decodes_compression_negotiated_by_proxy() {
    compression_contract(false, "gzip").await;
}

#[tokio::test]
async fn catalog_decodes_zstd_negotiated_by_proxy() {
    compression_contract(true, "zstd").await;
}

#[tokio::test]
async fn forwarding_decodes_zstd_negotiated_by_proxy() {
    compression_contract(false, "zstd").await;
}
