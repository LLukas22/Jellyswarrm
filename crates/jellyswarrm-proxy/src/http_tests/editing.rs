//! Library editing requests that the bundled client exposes. These are transport
//! contracts: exact verb/path/query/body, backend credentials and response status.
use super::*;
use wiremock::{
    matchers::{body_json, method, path, query_param},
    Mock, ResponseTemplate,
};

macro_rules! contracts {
    ($($name:ident: $verb:literal,$route:literal,$body:expr;)+) => {
        pub(super) const ROUTES: &[(&str,&str)] = &[$(($verb,$route)),+];
        $(#[tokio::test] async fn $name() { editing_contract($verb,$route,$body).await; })+
    };
}
contracts! {
    metadata_fields: "GET","/Items/{item}/MetadataEditor",Value::Null;
    external_id_fields: "GET","/Items/{item}/ExternalIdInfos",Value::Null;
    remote_search: "POST","/Items/RemoteSearch/Movie?ItemId={item}",json!({"ItemId":"{item}","SearchInfo":{"Name":"Movie","Year":2026}});
    content_type: "POST","/Items/{item}/ContentType?ContentType=movies",Value::Null;
    refresh: "POST","/Items/{item}/Refresh?MetadataRefreshMode=FullRefresh&ImageRefreshMode=ValidationOnly&ReplaceAllMetadata=false",Value::Null;
    image_delete: "DELETE","/Items/{item}/Images/Primary/0",Value::Null;
    image_move: "POST","/Items/{item}/Images/Backdrop/0/Index?NewIndex=1",Value::Null;
    subtitle_search: "GET","/Items/{item}/RemoteSearch/Subtitles/eng?IsPerfectMatch=true",Value::Null;
    lyric_search: "GET","/Audio/{item}/RemoteSearch/Lyrics",Value::Null;
    lyric_download: "POST","/Audio/{item}/RemoteSearch/Lyrics/provider-id",Value::Null;
    lyric_upload: "POST","/Audio/{item}/Lyrics",json!({"FileName":"lyrics.lrc","Lyrics":"[00:01.00]Line"});
    delete_item: "DELETE","/Items/{item}",Value::Null;
}

pub(super) async fn editing_contract(verb: &str, route: &str, body: Value) {
    let f = Fixture::new().await;
    f.healthy().await;
    let original = f.original(1).await;
    let target = url::Url::parse(&format!(
        "http://backend{}",
        route.replace("{item}", &original)
    ))
    .unwrap();
    let mut mock = Mock::given(method(verb)).and(path(target.path()));
    for (key, value) in target.query_pairs() {
        mock = mock.and(query_param(key.as_ref(), value.as_ref()));
    }
    if !body.is_null() {
        mock = mock.and(body_json(
            serde_json::from_str::<Value>(&body.to_string().replace("{item}", &original)).unwrap(),
        ));
    }
    let reads = verb == "GET" || route.contains("/RemoteSearch/Movie");
    let expected = if route.contains("/MetadataEditor") {
        json!({"ParentalRatingOptions":[],"Countries":[],"Cultures":[]})
    } else {
        json!([])
    };
    let template = if reads {
        ResponseTemplate::new(200).set_body_json(expected.clone())
    } else {
        ResponseTemplate::new(204)
    };
    mock.respond_with(template)
        .expect(1)
        .mount(&f.upstreams[1])
        .await;
    let mut request = f.request(
        verb.parse().unwrap(),
        &route.replace("{item}", &f.items[1]),
        &f.user,
        "tv",
    );
    if !body.is_null() {
        request = request.json(
            &serde_json::from_str::<Value>(&body.to_string().replace("{item}", &f.items[1]))
                .unwrap(),
        );
    }
    let response = request.send().await.unwrap();
    assert_eq!(
        response.status(),
        if reads {
            StatusCode::OK
        } else {
            StatusCode::NO_CONTENT
        },
        "{verb} {route}"
    );
    if reads {
        assert_eq!(response.json::<Value>().await.unwrap(), expected);
    }
    assert!(f.upstreams[0].received_requests().await.unwrap().is_empty());
}
