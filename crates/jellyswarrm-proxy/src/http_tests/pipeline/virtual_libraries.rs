//! HTTP regressions for virtual-library grouping, discovery, access, and item details.
use super::*;

const LIBRARY: &str = "11111111111111111111111111111111";

async fn configured(f: &Fixture) -> String {
    healthy(f).await;
    let group = f
        .state
        .virtual_library_service
        .create_group("My Movies")
        .await
        .unwrap();
    for server in f.state.server_storage.list_servers().await.unwrap() {
        f.state
            .virtual_library_service
            .add_member(&group.virtual_id, server.id, LIBRARY, "Movies", "movies")
            .await
            .unwrap();
    }
    group.virtual_id
}

fn library(kind: &str) -> Value {
    json!({"Id": LIBRARY, "Type": "CollectionFolder", "CollectionType": kind, "Name": "Movies", "ChildCount": 2, "ImageTags": {"Primary": "art"}})
}

fn list(items: Vec<Value>) -> ResponseTemplate {
    ResponseTemplate::new(200)
        .set_body_json(json!({"TotalRecordCount": items.len(), "StartIndex": 0, "Items": items}))
}

#[tokio::test]
async fn configured_and_automatic_libraries_survive_batch_lookup() {
    for automatic in [false, true] {
        let f = Fixture::new().await;
        let configured_id = if automatic {
            f.state.config.write().await.merge_libraries = true;
            healthy(&f).await;
            None
        } else {
            Some(configured(&f).await)
        };
        for upstream in &f.upstreams {
            Mock::given(path("/UserViews"))
                .respond_with(list(vec![library("movies")]))
                .mount(upstream)
                .await;
            Mock::given(path("/Items"))
                .respond_with(list(vec![library("movies")]))
                .mount(upstream)
                .await;
            Mock::given(path(format!("/Items/{LIBRARY}")))
                .respond_with(ResponseTemplate::new(200).set_body_json(library("movies")))
                .mount(upstream)
                .await;
        }
        let root = catalog(&f, "/UserViews").await;
        let id = root["Items"][0]["Id"].as_str().unwrap();
        if let Some(configured_id) = configured_id {
            assert_eq!(id, configured_id);
        }
        assert_eq!(catalog(&f, &format!("/Items/{id}")).await["Id"], id);
        let batch = catalog(&f, &format!("/Items?Ids={id},{id}&Fields=ChildCount")).await;
        assert_eq!(batch["TotalRecordCount"], 1);
        for field in [
            "Id",
            "Name",
            "DisplayPreferencesId",
            "ChildCount",
            "PrimaryImageItemId",
            "PrimaryImageTag",
        ] {
            assert_eq!(
                batch["Items"][0][field], root["Items"][0][field],
                "{field}, automatic={automatic}"
            );
        }
        // The second viewer has no mapped servers; server authorization is
        // never inferred from another viewer's library inventory.
        let foreign = f
            .request(
                reqwest::Method::GET,
                &format!("/Items?Ids={id}"),
                &f.other,
                "tv",
            )
            .send()
            .await
            .unwrap();
        assert!(!foreign.status().is_success());
    }
}

#[tokio::test]
async fn configured_detail_uses_visible_member_and_falls_back_before_discovery() {
    for (discover_first, status) in [
        (true, 404),
        (false, 404),
        (false, 403),
        (false, 401),
        (false, 503),
    ] {
        let f = Fixture::new().await;
        let id = configured(&f).await;
        for (index, upstream) in f.upstreams.iter().enumerate() {
            Mock::given(path("/UserViews"))
                .respond_with(list(if index == 0 {
                    vec![]
                } else {
                    vec![library("movies")]
                }))
                .mount(upstream)
                .await;
            Mock::given(path(format!("/Items/{LIBRARY}")))
                .respond_with(if index == 0 {
                    ResponseTemplate::new(status)
                } else {
                    ResponseTemplate::new(200).set_body_json(library("movies"))
                })
                .mount(upstream)
                .await;
            Mock::given(path("/Items")).respond_with(if index == 0 { ResponseTemplate::new(404) } else { list(vec![json!({"Id":"22222222222222222222222222222222","Type":"Movie","Name":"Visible"})]) }).mount(upstream).await;
        }
        if discover_first {
            assert_eq!(catalog(&f, "/UserViews").await["Items"][0]["Id"], id);
            assert_eq!(
                catalog(&f, &format!("/Items?ParentId={id}")).await["TotalRecordCount"],
                1
            );
        }
        let detail = catalog(&f, &format!("/Items/{id}")).await;
        assert_eq!(detail["Id"], id);
        assert_eq!(detail["Name"], "My Movies");
        let attempted = f.upstreams[0]
            .received_requests()
            .await
            .unwrap()
            .into_iter()
            .filter(|request| request.url.path().starts_with("/Items"))
            .count();
        assert_eq!(attempted, usize::from(!discover_first));
    }
}

#[tokio::test]
async fn type_changed_member_excluded_from_browse() {
    let f = Fixture::new().await;
    let id = configured(&f).await;
    for (index, upstream) in f.upstreams.iter().enumerate() {
        Mock::given(path("/UserViews"))
            .respond_with(list(vec![library(if index == 0 {
                "tvshows"
            } else {
                "movies"
            })]))
            .mount(upstream)
            .await;
        Mock::given(path("/Items")).respond_with(list(vec![json!({"Id":format!("{:032x}", index+2),"Type":if index == 0 { "Series" } else { "Movie" },"Name":"Content"})])).mount(upstream).await;
    }
    let root = catalog(&f, "/UserViews").await;
    assert_eq!(root["Items"].as_array().unwrap().len(), 2);
    let browse = catalog(&f, &format!("/Items?ParentId={id}&Recursive=true")).await;
    assert_eq!(browse["TotalRecordCount"], 1);
    assert_eq!(browse["Items"][0]["Type"], "Movie");
}

#[tokio::test]
async fn server_deletion_resets_empty_group_type() {
    for by_name in [false, true] {
        let f = Fixture::new().await;
        let group = f
            .state
            .virtual_library_service
            .create_group("Reusable")
            .await
            .unwrap();
        let servers = f.state.server_storage.list_servers().await.unwrap();
        f.state
            .virtual_library_service
            .add_member(
                &group.virtual_id,
                servers[0].id,
                LIBRARY,
                "Movies",
                "movies",
            )
            .await
            .unwrap();
        if by_name {
            f.state
                .server_storage
                .delete_server_by_name(&servers[0].name)
                .await
                .unwrap();
        } else {
            f.state
                .server_storage
                .delete_server(servers[0].id)
                .await
                .unwrap();
        }
        assert!(f
            .state
            .virtual_library_service
            .list_members(&group.virtual_id)
            .await
            .unwrap()
            .is_empty());
        assert!(f
            .state
            .virtual_library_service
            .get_group(&group.virtual_id)
            .await
            .unwrap()
            .unwrap()
            .collection_type
            .is_none());
        f.state
            .virtual_library_service
            .add_member(
                &group.virtual_id,
                servers[1].id,
                LIBRARY,
                "Shows",
                "tvshows",
            )
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn failed_viewer_refresh_preserves_members_and_recovery_updates_them() {
    let f = Fixture::new().await;
    let id = configured(&f).await;
    let fail = Arc::new(AtomicBool::new(false));
    let removed = Arc::new(AtomicBool::new(false));
    for (index, upstream) in f.upstreams.iter().enumerate() {
        let fail = fail.clone();
        let removed = removed.clone();
        Mock::given(path("/UserViews"))
            .respond_with(move |_: &wiremock::Request| {
                if index == 0 && fail.load(Ordering::SeqCst) {
                    ResponseTemplate::new(503)
                } else {
                    list(if index == 0 && removed.load(Ordering::SeqCst) {
                        vec![]
                    } else {
                        vec![library("movies")]
                    })
                }
            })
            .mount(upstream)
            .await;
    }
    let scope = crate::libraries::service::VirtualLibraryAccessScope::new(
        &f.user.id,
        f.state
            .server_storage
            .list_servers()
            .await
            .unwrap()
            .into_iter()
            .map(|server| server.id),
    );
    for (failing, missing, expected) in [
        (false, false, 2),
        (true, false, 2),
        (false, true, 1),
        (false, false, 2),
    ] {
        fail.store(failing, Ordering::SeqCst);
        removed.store(missing, Ordering::SeqCst);
        assert_eq!(catalog(&f, "/UserViews").await["Items"][0]["Id"], id);
        let crate::libraries::service::VirtualLibraryResolution::Resolved(library) = f
            .state
            .virtual_library_service
            .resolve(&id, Some(&scope))
            .await
            .unwrap()
        else {
            panic!()
        };
        assert_eq!(library.members.len(), expected);
    }
}

#[tokio::test]
async fn incompatible_assignments_do_not_block_new_automatic_memberships() {
    let f = Fixture::new().await;
    let configured_id = configured(&f).await;
    f.state.config.write().await.merge_libraries = true;
    for upstream in &f.upstreams {
        Mock::given(path("/UserViews"))
            .respond_with(list(vec![library("tvshows")]))
            .mount(upstream)
            .await;
        Mock::given(path("/Items"))
            .respond_with(list(vec![library("tvshows")]))
            .mount(upstream)
            .await;
    }
    let root = catalog(&f, "/UserViews").await;
    let automatic = root["Items"][0]["Id"].as_str().unwrap();
    assert_ne!(automatic, configured_id);
    let lookup = catalog(&f, &format!("/Items?Ids={automatic}")).await;
    assert_eq!(lookup["Items"][0]["Id"], automatic);
    assert_eq!(lookup["Items"][0]["ChildCount"], 4);
    assert_eq!(
        catalog(&f, &format!("/Items?ParentId={configured_id}")).await["TotalRecordCount"],
        0
    );
}
