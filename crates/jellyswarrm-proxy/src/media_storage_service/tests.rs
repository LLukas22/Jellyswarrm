use std::str::FromStr;

use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};

use crate::config::MIGRATOR;

use super::*;

async fn foreign_key_pool() -> SqlitePool {
    let options = SqliteConnectOptions::from_str("sqlite::memory:")
        .unwrap()
        .foreign_keys(true);
    SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .unwrap()
}

async fn migrated_service() -> (SqlitePool, MediaStorageService) {
    let pool = foreign_key_pool().await;
    MIGRATOR.run(&pool).await.unwrap();
    let service = MediaStorageService::new(pool.clone());
    (pool, service)
}

fn tmdb_movie_observation(virtual_media_id: &str, provider_id: &str) -> MediaObservation {
    MediaObservation {
        virtual_media_id: virtual_media_id.to_string(),
        aliases: BTreeSet::from([MediaAlias {
            provider: crate::media_identity::MediaProvider::Tmdb,
            kind: crate::media_identity::MediaKind::Movie,
            provider_id: provider_id.to_string(),
        }]),
    }
}

async fn assert_no_foreign_key_violations(pool: &SqlitePool) {
    let violations = sqlx::query("PRAGMA foreign_key_check")
        .fetch_all(pool)
        .await
        .unwrap();
    assert!(violations.is_empty());
}

async fn create_test_server(pool: &SqlitePool) -> Server {
    create_test_server_with_url(pool, "http://localhost:8096").await
}

async fn create_test_server_with_url(pool: &SqlitePool, url: &str) -> Server {
    let now = chrono::Utc::now();
    let result = sqlx::query(
        r#"
        INSERT INTO servers (name, url, priority, created_at, updated_at)
        VALUES (?, ?, ?, ?, ?)
        "#,
    )
    .bind(url)
    .bind(url)
    .bind(100)
    .bind(now)
    .bind(now)
    .execute(pool)
    .await
    .unwrap();

    Server {
        id: ServerId::new(result.last_insert_rowid()),
        name: "Test Server".to_string(),
        url: ServerUrl::parse(url).unwrap(),
        priority: 100,
        media_streaming_mode: MediaStreamingMode::Redirect,
        created_at: now,
        updated_at: now,
    }
}

#[tokio::test]
async fn test_media_storage_service() {
    let (pool, service) = migrated_service().await;
    let server = create_test_server(&pool).await;

    // Create media mapping
    let mapping = service
        .get_or_create_media_mapping("original-media-123", &server)
        .await
        .unwrap();

    assert_eq!(mapping.original_media_id, "original-media-123");
    assert_eq!(mapping.server_url, "http://localhost:8096");

    // Get mapping by virtual ID
    let retrieved_mapping = service
        .get_media_mapping_by_virtual(&mapping.virtual_media_id)
        .await
        .unwrap()
        .unwrap();

    assert_eq!(retrieved_mapping.virtual_media_id, mapping.virtual_media_id);
    assert_eq!(retrieved_mapping.original_media_id, "original-media-123");
}

#[tokio::test]
async fn test_get_media_mapping_with_server() {
    let (pool, service) = migrated_service().await;
    let server = create_test_server(&pool).await;

    // Create media mapping
    let mapping = service
        .get_or_create_media_mapping("original-media-123", &server)
        .await
        .unwrap();

    // Get mapping with server info
    let (retrieved_mapping, server) = service
        .get_media_mapping_with_server(&mapping.virtual_media_id)
        .await
        .unwrap()
        .unwrap();

    assert_eq!(retrieved_mapping.virtual_media_id, mapping.virtual_media_id);
    assert_eq!(retrieved_mapping.original_media_id, "original-media-123");
    assert_eq!(server.name, "http://localhost:8096");
    assert_eq!(server.url.as_str(), "http://localhost:8096");
}

#[tokio::test]
async fn test_delete_operations() {
    let (pool, service) = migrated_service().await;
    let server = create_test_server(&pool).await;

    // Create media mapping
    let mapping = service
        .get_or_create_media_mapping("media-123", &server)
        .await
        .unwrap();

    // Verify mapping exists
    assert!(service
        .get_media_mapping_by_virtual(&mapping.virtual_media_id)
        .await
        .unwrap()
        .is_some());

    // Delete mapping
    let deleted = service
        .delete_media_mapping(&mapping.virtual_media_id)
        .await
        .unwrap();

    assert!(deleted);

    // Verify mapping is gone
    assert!(service
        .get_media_mapping_by_virtual(&mapping.virtual_media_id)
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn older_full_catalog_merges_after_newer_filtered_catalog() {
    let (pool, service) = migrated_service().await;
    let server = create_test_server(&pool).await;
    let mut observations = Vec::new();
    for id in ["shared", "full-only", "filtered-only", "removed"] {
        let mapping = service
            .get_or_create_media_mapping(id, &server)
            .await
            .unwrap();
        observations.push(tmdb_movie_observation(&mapping.virtual_media_id, id));
    }
    let snapshot = |observations, complete| MediaCatalogSnapshot {
        source_key: "source".to_string(),
        server_id: server.id,
        complete,
        observations,
    };
    let initial = service.begin_media_reconciliation().await.unwrap();
    service
        .reconcile_media_catalog(
            "scope",
            initial,
            &[snapshot(vec![observations[3].clone()], true)],
            true,
        )
        .await
        .unwrap();
    let older = service.begin_media_reconciliation().await.unwrap();
    let newer = service.begin_media_reconciliation().await.unwrap();
    let mut changed = observations[0].clone();
    changed.aliases.clear();
    service
        .reconcile_media_catalog(
            "scope",
            newer,
            &[snapshot(vec![changed, observations[2].clone()], false)],
            false,
        )
        .await
        .unwrap();

    let merged = service
        .reconcile_media_catalog(
            "scope",
            older,
            &[snapshot(observations[..2].to_vec(), true)],
            true,
        )
        .await
        .unwrap();
    assert!(
        !merged.contains_key(&observations[0].virtual_media_id),
        "older aliases must not replace newer empty aliases"
    );
    assert!(merged.contains_key(&observations[1].virtual_media_id));
    assert!(
        merged.contains_key(&observations[2].virtual_media_id),
        "older full inventory must preserve newer sightings it omits"
    );
    assert!(!merged.contains_key(&observations[3].virtual_media_id));
    let generations: Vec<(String, i64, i64)> = sqlx::query_as(
        "SELECT mapping.original_media_id, sighting.generation, member.aliases_generation FROM movie_catalog_sightings sighting JOIN media_mappings mapping ON mapping.id = sighting.media_mapping_id JOIN movie_version_members member ON member.scope_id = sighting.scope_id AND member.media_mapping_id = sighting.media_mapping_id ORDER BY mapping.original_media_id",
    ).fetch_all(&pool).await.unwrap();
    assert_eq!(
        generations,
        vec![
            ("filtered-only".to_string(), newer, newer),
            ("full-only".to_string(), older, older),
            ("shared".to_string(), newer, newer)
        ]
    );
    let watermarks: (i64, i64) = sqlx::query_as("SELECT scope.committed_generation, source.committed_generation FROM movie_catalog_scopes scope JOIN movie_catalog_sources source ON source.scope_id = scope.id WHERE scope.scope_key = 'scope'").fetch_one(&pool).await.unwrap();
    assert_eq!(watermarks, (newer, older));
}

#[tokio::test]
async fn stale_catalogs_cannot_revive_newer_full_removals() {
    let (pool, service) = migrated_service().await;
    let server = create_test_server(&pool).await;
    let mapping = service
        .get_or_create_media_mapping("removed", &server)
        .await
        .unwrap();
    let mut snapshot = MediaCatalogSnapshot {
        source_key: "source".to_string(),
        server_id: server.id,
        complete: true,
        observations: vec![tmdb_movie_observation(&mapping.virtual_media_id, "42")],
    };
    let initial = service.begin_media_reconciliation().await.unwrap();
    service
        .reconcile_media_catalog("scope", initial, &[snapshot.clone()], true)
        .await
        .unwrap();
    let older = service.begin_media_reconciliation().await.unwrap();
    let newer = service.begin_media_reconciliation().await.unwrap();
    let empty = MediaCatalogSnapshot {
        observations: vec![],
        ..snapshot.clone()
    };
    service
        .reconcile_media_catalog("scope", newer, &[empty], true)
        .await
        .unwrap();
    for complete in [true, false] {
        snapshot.complete = complete;
        let groups = service
            .reconcile_media_catalog("scope", older, &[snapshot.clone()], false)
            .await
            .unwrap();
        assert!(groups.is_empty());
    }
    let member_generation: (i64,) = sqlx::query_as(
        "SELECT aliases_generation FROM movie_version_members WHERE media_mapping_id = ?",
    )
    .bind(mapping.id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(member_generation.0, initial);
    let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM movie_catalog_sightings")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count.0, 0);
}

#[tokio::test]
async fn overlapping_catalog_sources_have_independent_freshness() {
    let (pool, service) = migrated_service().await;
    let mut snapshots = Vec::new();
    for url in ["http://primary.example:8096", "http://sibling.example:8096"] {
        let server = create_test_server_with_url(&pool, url).await;
        let mapping = service
            .get_or_create_media_mapping("movie", &server)
            .await
            .unwrap();
        snapshots.push(MediaCatalogSnapshot {
            source_key: server.id.to_string(),
            server_id: server.id,
            complete: true,
            observations: vec![tmdb_movie_observation(&mapping.virtual_media_id, "42")],
        });
    }
    let older = service.begin_media_reconciliation().await.unwrap();
    let middle = service.begin_media_reconciliation().await.unwrap();
    let newer = service.begin_media_reconciliation().await.unwrap();
    service
        .reconcile_media_catalog("scope", newer, &snapshots[1..], false)
        .await
        .unwrap();
    let merged = service
        .reconcile_media_catalog("scope", older, &snapshots[..1], true)
        .await
        .unwrap();
    assert_eq!(merged.len(), 2);
    assert!(merged
        .values()
        .all(|group| group.published && group.active_member_count == 2));

    // A different source may add a sighting, but cannot roll back shared aliases.
    let mut alternate = snapshots[1].clone();
    alternate.source_key = "alternate".to_string();
    alternate.observations[0].aliases.clear();
    let merged = service
        .reconcile_media_catalog("scope", middle, &[alternate], false)
        .await
        .unwrap();
    assert_eq!(merged.len(), 2);
    let source_count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM movie_catalog_sources")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(source_count.0, 3);
    let stale = service.begin_media_reconciliation().await.unwrap();
    let prune = service.begin_media_reconciliation().await.unwrap();
    service
        .reconcile_media_catalog("scope", prune, &snapshots[..1], true)
        .await
        .unwrap();
    let merged = service
        .reconcile_media_catalog("scope", stale, &snapshots, true)
        .await
        .unwrap();
    assert_eq!(merged.len(), 1);
    assert!(merged.contains_key(&snapshots[0].observations[0].virtual_media_id));
    let source_count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM movie_catalog_sources")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(source_count.0, 1);
}

#[tokio::test]
async fn detail_routes_require_equivalent_viewer_scopes() {
    let (pool, service) = migrated_service().await;
    let primary = create_test_server_with_url(&pool, "http://primary.example:8096").await;
    let sibling = create_test_server_with_url(&pool, "http://sibling.example:8096").await;
    let owner = service
        .get_or_create_media_mapping("owner", &sibling)
        .await
        .unwrap();
    let source = service
        .get_or_create_media_mapping("source", &sibling)
        .await
        .unwrap();
    let mut snapshots = Vec::new();
    for server in [&primary, &sibling] {
        let mapping = service
            .get_or_create_media_mapping("owner", server)
            .await
            .unwrap();
        snapshots.push(MediaCatalogSnapshot {
            source_key: server.url.to_string(),
            server_id: server.id,
            complete: true,
            observations: vec![tmdb_movie_observation(&mapping.virtual_media_id, "42")],
        });
    }
    let mut first_viewer_group_id = None;
    #[derive(PartialEq)]
    enum Phase {
        OtherViewer,
        FirstViewerScope,
        EquivalentViewerScope,
    }
    for (phase, scope) in [
        (Phase::OtherViewer, "configured:library:other"),
        (Phase::FirstViewerScope, "automatic:library:viewer"),
        (Phase::EquivalentViewerScope, "latest:viewer:"),
    ] {
        let generation = service.begin_media_reconciliation().await.unwrap();
        let assignments = service
            .reconcile_media_catalog(scope, generation, &snapshots, true)
            .await
            .unwrap();
        let group = service
            .get_media_version_group(&assignments[&owner.virtual_media_id].virtual_media_id)
            .await
            .unwrap()
            .unwrap();
        if phase == Phase::FirstViewerScope {
            first_viewer_group_id = Some(group.id);
            // Membership alone must not expose another viewer's routes.
            assert!(service
                .get_media_version_detail_route(&owner.virtual_media_id, "viewer")
                .await
                .unwrap()
                .is_none());
        }
        let generation = service.begin_media_reconciliation().await.unwrap();
        assert!(service
            .replace_media_version_sources(
                group.id,
                generation,
                &[owner.id],
                &[MediaVersionSourceObservation {
                    member_mapping_id: owner.id,
                    source_virtual_id: source.virtual_media_id.clone(),
                }]
            )
            .await
            .unwrap());
        for id in [&owner.virtual_media_id, &source.virtual_media_id] {
            let route = service
                .get_media_version_detail_route(id, "viewer")
                .await
                .unwrap();
            if phase != Phase::OtherViewer {
                let (resolved_group, resolved_owner) = route.unwrap();
                assert_eq!(resolved_group.id, first_viewer_group_id.unwrap());
                assert_eq!(resolved_owner.id, owner.id);
            } else {
                assert!(route.is_none());
            }
            assert!(service.get_media_version_group(id).await.unwrap().is_none());
            assert!(service
                .get_media_version_detail_route(id, "view%")
                .await
                .unwrap()
                .is_none());
        }
        if phase == Phase::FirstViewerScope {
            for column in ["ambiguous", "published"] {
                let disabled = if column == "ambiguous" { 1 } else { 0 };
                sqlx::query(&format!(
                    "UPDATE movie_version_groups SET {column} = ? WHERE id = ?"
                ))
                .bind(disabled)
                .bind(group.id)
                .execute(&pool)
                .await
                .unwrap();
                assert!(service
                    .get_media_version_detail_route(&source.virtual_media_id, "viewer")
                    .await
                    .unwrap()
                    .is_none());
                sqlx::query(&format!(
                    "UPDATE movie_version_groups SET {column} = ? WHERE id = ?"
                ))
                .bind(1 - disabled)
                .bind(group.id)
                .execute(&pool)
                .await
                .unwrap();
            }
        }
        if phase == Phase::EquivalentViewerScope {
            // A subset is not equivalent: do not combine scopes or choose
            // the larger group merely because it was encountered first.
            sqlx::query(
                "DELETE FROM movie_catalog_sightings WHERE scope_id = ? AND media_mapping_id != ?",
            )
            .bind(group.scope_id)
            .bind(owner.id)
            .execute(&pool)
            .await
            .unwrap();
            for id in [&owner.virtual_media_id, &source.virtual_media_id] {
                assert!(service
                    .get_media_version_detail_route(id, "viewer")
                    .await
                    .unwrap()
                    .is_none());
            }

            // A different owner for the same source must also be refused.
            let other_owner = service
                .get_or_create_media_mapping("other-owner", &sibling)
                .await
                .unwrap();
            snapshots[1].observations[0].virtual_media_id = other_owner.virtual_media_id.clone();
            let generation = service.begin_media_reconciliation().await.unwrap();
            let assignments = service
                .reconcile_media_catalog(scope, generation, &snapshots, true)
                .await
                .unwrap();
            let other_group = service
                .get_media_version_group(
                    &assignments[&other_owner.virtual_media_id].virtual_media_id,
                )
                .await
                .unwrap()
                .unwrap();
            let generation = service.begin_media_reconciliation().await.unwrap();
            assert!(service
                .replace_media_version_sources(
                    other_group.id,
                    generation,
                    &[other_owner.id],
                    &[MediaVersionSourceObservation {
                        member_mapping_id: other_owner.id,
                        source_virtual_id: source.virtual_media_id.clone(),
                    }]
                )
                .await
                .unwrap());
            assert!(service
                .get_media_version_detail_route(&source.virtual_media_id, "viewer")
                .await
                .unwrap()
                .is_none());
        }
    }
}

#[tokio::test]
async fn media_groups_have_stable_ids_and_exact_source_routes() {
    let (pool, service) = migrated_service().await;
    let primary = create_test_server_with_url(&pool, "http://primary.example:8096").await;
    let sibling = create_test_server_with_url(&pool, "http://sibling.example:8096").await;

    let primary_mapping = service
        .get_or_create_media_mapping("media-1", &primary)
        .await
        .unwrap();
    let sibling_mapping = service
        .get_or_create_media_mapping("media-1", &sibling)
        .await
        .unwrap();

    let primary_observation = tmdb_movie_observation(&primary_mapping.virtual_media_id, "42");
    let sibling_observation = tmdb_movie_observation(&sibling_mapping.virtual_media_id, "42");
    let generation = service.begin_media_reconciliation().await.unwrap();
    let assignments = service
        .reconcile_media_catalog(
            "configured:library:user",
            generation,
            &[
                MediaCatalogSnapshot {
                    source_key: "primary:library".to_string(),
                    server_id: primary.id,
                    complete: true,
                    observations: vec![primary_observation.clone()],
                },
                MediaCatalogSnapshot {
                    source_key: "sibling:library".to_string(),
                    server_id: sibling.id,
                    complete: true,
                    observations: vec![sibling_observation.clone()],
                },
            ],
            true,
        )
        .await
        .unwrap();
    let assignment = assignments
        .get(&primary_mapping.virtual_media_id)
        .unwrap()
        .clone();
    let aggregate_id = assignment.virtual_media_id;
    assert_eq!(assignment.active_member_count, 2);
    assert!(assignment.published);
    assert!(!assignment.ambiguous);

    // Parent navigation does not require playback source routes, and must
    // not borrow an aggregate from another viewer's catalog.
    assert_eq!(
        service
            .get_media_parent_group_id(&primary_mapping.virtual_media_id, "user")
            .await
            .unwrap(),
        Some(aggregate_id.clone())
    );
    assert_eq!(
        service
            .get_media_parent_group_id(&sibling_mapping.virtual_media_id, "user")
            .await
            .unwrap(),
        Some(aggregate_id.clone())
    );
    assert!(service
        .get_media_parent_group_id(&primary_mapping.virtual_media_id, "other")
        .await
        .unwrap()
        .is_none());

    let group = service
        .get_media_version_group(&aggregate_id)
        .await
        .unwrap()
        .unwrap();
    assert!(group.published);
    assert_eq!(
        service
            .get_media_version_members(group.id)
            .await
            .unwrap()
            .len(),
        2
    );

    let source_mapping = service
        .get_or_create_media_mapping("source-1", &sibling)
        .await
        .unwrap();
    let source_generation = service.begin_media_reconciliation().await.unwrap();
    service
        .replace_media_version_sources(
            group.id,
            source_generation,
            &[sibling_mapping.id],
            &[MediaVersionSourceObservation {
                member_mapping_id: sibling_mapping.id,
                source_virtual_id: source_mapping.virtual_media_id.clone(),
            }],
        )
        .await
        .unwrap();
    let route = service
        .get_media_version_source_route(group.id, &source_mapping.virtual_media_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(route.source_mapping.id, source_mapping.id);
    assert_eq!(route.member_mapping.id, sibling_mapping.id);

    let same_server_copy = service
        .get_or_create_media_mapping("media-copy", &sibling)
        .await
        .unwrap();
    let copy_observation = tmdb_movie_observation(&same_server_copy.virtual_media_id, "42");
    let ambiguous_generation = service.begin_media_reconciliation().await.unwrap();
    let assignments = service
        .reconcile_media_catalog(
            "configured:library:user",
            ambiguous_generation,
            &[MediaCatalogSnapshot {
                source_key: "sibling:library".to_string(),
                server_id: sibling.id,
                complete: false,
                observations: vec![sibling_observation.clone(), copy_observation],
            }],
            false,
        )
        .await
        .unwrap();
    assert!(assignments[&sibling_mapping.virtual_media_id].ambiguous);

    let recovery_generation = service.begin_media_reconciliation().await.unwrap();
    let recovered = service
        .reconcile_media_catalog(
            "configured:library:user",
            recovery_generation,
            &[MediaCatalogSnapshot {
                source_key: "sibling:library".to_string(),
                server_id: sibling.id,
                complete: true,
                observations: vec![sibling_observation.clone()],
            }],
            false,
        )
        .await
        .unwrap();
    let recovered = &recovered[&sibling_mapping.virtual_media_id];
    assert!(!recovered.ambiguous);
    assert_eq!(recovered.virtual_media_id, aggregate_id);

    let stale_generation = service.begin_media_reconciliation().await.unwrap();
    let fresh_generation = service.begin_media_reconciliation().await.unwrap();
    service
        .reconcile_media_catalog(
            "configured:library:user",
            fresh_generation,
            &[MediaCatalogSnapshot {
                source_key: "sibling:library".to_string(),
                server_id: sibling.id,
                complete: true,
                observations: Vec::new(),
            }],
            false,
        )
        .await
        .unwrap();
    let stale = service
        .reconcile_media_catalog(
            "configured:library:user",
            stale_generation,
            &[MediaCatalogSnapshot {
                source_key: "sibling:library".to_string(),
                server_id: sibling.id,
                complete: true,
                observations: vec![sibling_observation],
            }],
            false,
        )
        .await
        .unwrap();
    assert!(!stale.contains_key(&sibling_mapping.virtual_media_id));

    let replacement_primary = service
        .get_or_create_media_mapping("replacement-primary", &primary)
        .await
        .unwrap();
    let replacement_sibling = service
        .get_or_create_media_mapping("replacement-sibling", &sibling)
        .await
        .unwrap();
    let replacement_generation = service.begin_media_reconciliation().await.unwrap();
    let replacements = service
        .reconcile_media_catalog(
            "configured:library:user",
            replacement_generation,
            &[
                MediaCatalogSnapshot {
                    source_key: "primary:library".to_string(),
                    server_id: primary.id,
                    complete: true,
                    observations: vec![tmdb_movie_observation(
                        &replacement_primary.virtual_media_id,
                        "42",
                    )],
                },
                MediaCatalogSnapshot {
                    source_key: "sibling:library".to_string(),
                    server_id: sibling.id,
                    complete: true,
                    observations: vec![tmdb_movie_observation(
                        &replacement_sibling.virtual_media_id,
                        "42",
                    )],
                },
            ],
            true,
        )
        .await
        .unwrap();
    assert_eq!(
        replacements[&replacement_primary.virtual_media_id].virtual_media_id,
        aggregate_id
    );

    assert_no_foreign_key_violations(&pool).await;
}

#[tokio::test]
async fn scoped_media_migration_preserves_legacy_aggregate_routes() {
    let pool = foreign_key_pool().await;
    sqlx::raw_sql(
        r#"
        CREATE TABLE servers (id INTEGER PRIMARY KEY);
        CREATE TABLE media_mappings (
            id INTEGER PRIMARY KEY,
            virtual_media_id TEXT NOT NULL UNIQUE,
            original_media_id TEXT NOT NULL,
            server_id INTEGER NOT NULL,
            server_url TEXT NOT NULL,
            created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
        );
        INSERT INTO servers (id) VALUES (1), (2);
        INSERT INTO media_mappings (
            id, virtual_media_id, original_media_id, server_id, server_url
        ) VALUES
            (1, 'member-a', 'media-a', 1, 'http://a'),
            (2, 'member-b', 'media-b', 2, 'http://b'),
            (3, 'source-b', 'source-b', 2, 'http://b');
        "#,
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::raw_sql(include_str!(
        "../../migrations/20260827130000_movie_versions.up.sql"
    ))
    .execute(&pool)
    .await
    .unwrap();
    sqlx::raw_sql(
        r#"
        INSERT INTO movie_version_groups (
            id, virtual_media_id, provider, provider_id, ambiguous
        ) VALUES (7, 'aggregate-7', 'tmdb', '42', 0);
        INSERT INTO movie_version_members (
            group_id, media_mapping_id, server_id
        ) VALUES (7, 1, 1), (7, 2, 2);
        INSERT INTO movie_version_sources (
            group_id, member_mapping_id, source_mapping_id
        ) VALUES (7, 2, 3);
        "#,
    )
    .execute(&pool)
    .await
    .unwrap();

    sqlx::raw_sql(include_str!(
        "../../migrations/20260827140000_scoped_movie_versions.up.sql"
    ))
    .execute(&pool)
    .await
    .unwrap();

    let migrated_group: (i64, String, bool) = sqlx::query_as(
        r#"
        SELECT version_group.id, group_id.virtual_media_id, version_group.published
        FROM movie_version_groups version_group
        JOIN movie_version_group_ids group_id ON group_id.group_id = version_group.id
        WHERE version_group.id = 7 AND group_id.canonical = 1
        "#,
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(migrated_group, (7, "aggregate-7".to_string(), true));
    let route_count: (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM movie_version_sources WHERE group_id = 7")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(route_count.0, 1);
    assert_no_foreign_key_violations(&pool).await;

    sqlx::raw_sql(include_str!(
        "../../migrations/20260827140000_scoped_movie_versions.down.sql"
    ))
    .execute(&pool)
    .await
    .unwrap();
    let restored: (String, String, String) = sqlx::query_as(
        "SELECT virtual_media_id, provider, provider_id FROM movie_version_groups WHERE id = 7",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        restored,
        (
            "aggregate-7".to_string(),
            "tmdb".to_string(),
            "42".to_string()
        )
    );
    assert_no_foreign_key_violations(&pool).await;
}

#[tokio::test]
async fn removing_provider_ids_deletes_recorded_sources_only_in_the_current_scope() {
    let (pool, service) = migrated_service().await;
    let server = create_test_server(&pool).await;
    let mapping = service
        .get_or_create_media_mapping("member", &server)
        .await
        .unwrap();
    let source = service
        .get_or_create_media_mapping("source", &server)
        .await
        .unwrap();
    let mut snapshots = vec![MediaCatalogSnapshot {
        source_key: "library".to_string(),
        server_id: server.id,
        complete: true,
        observations: vec![tmdb_movie_observation(&mapping.virtual_media_id, "42")],
    }];
    let mut groups = Vec::new();
    for scope in ["scope-a", "scope-b"] {
        let generation = service.begin_media_reconciliation().await.unwrap();
        let assignments = service
            .reconcile_media_catalog(scope, generation, &snapshots, true)
            .await
            .unwrap();
        let group = service
            .get_media_version_group(&assignments[&mapping.virtual_media_id].virtual_media_id)
            .await
            .unwrap()
            .unwrap();
        let generation = service.begin_media_reconciliation().await.unwrap();
        assert!(service
            .replace_media_version_sources(
                group.id,
                generation,
                &[mapping.id],
                &[MediaVersionSourceObservation {
                    member_mapping_id: mapping.id,
                    source_virtual_id: source.virtual_media_id.clone(),
                }],
            )
            .await
            .unwrap());
        assert!(service
            .get_media_version_source_route(group.id, &source.virtual_media_id)
            .await
            .unwrap()
            .is_some());
        groups.push(group);
    }

    snapshots[0].observations[0].aliases.clear();
    let generation = service.begin_media_reconciliation().await.unwrap();
    let assignments = service
        .reconcile_media_catalog("scope-a", generation, &snapshots, true)
        .await
        .unwrap();
    assert!(!assignments.contains_key(&mapping.virtual_media_id));
    let (group_id,): (Option<i64>,) = sqlx::query_as(
        "SELECT group_id FROM movie_version_members WHERE scope_id = ? AND media_mapping_id = ?",
    )
    .bind(groups[0].scope_id)
    .bind(mapping.id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(group_id, None);
    let (source_count,): (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM movie_version_sources WHERE scope_id = ?")
            .bind(groups[0].scope_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(source_count, 0);
    assert!(service
        .get_media_version_source_route(groups[1].id, &source.virtual_media_id)
        .await
        .unwrap()
        .is_some());
    assert_no_foreign_key_violations(&pool).await;
}

#[tokio::test]
async fn rebuilding_one_scope_does_not_move_another_scopes_source_routes() {
    let (pool, service) = migrated_service().await;
    let primary = create_test_server_with_url(&pool, "http://scope-a.example:8096").await;
    let sibling = create_test_server_with_url(&pool, "http://scope-b.example:8096").await;
    let primary_mapping = service
        .get_or_create_media_mapping("media-primary", &primary)
        .await
        .unwrap();
    let sibling_mapping = service
        .get_or_create_media_mapping("media-sibling", &sibling)
        .await
        .unwrap();
    let source_mapping = service
        .get_or_create_media_mapping("source-sibling", &sibling)
        .await
        .unwrap();
    let snapshots = || {
        vec![
            MediaCatalogSnapshot {
                source_key: "primary:library".to_string(),
                server_id: primary.id,
                complete: true,
                observations: vec![tmdb_movie_observation(
                    &primary_mapping.virtual_media_id,
                    "42",
                )],
            },
            MediaCatalogSnapshot {
                source_key: "sibling:library".to_string(),
                server_id: sibling.id,
                complete: true,
                observations: vec![tmdb_movie_observation(
                    &sibling_mapping.virtual_media_id,
                    "42",
                )],
            },
        ]
    };
    let mut groups = Vec::new();
    for scope in ["configured:library:user-a", "configured:library:user-b"] {
        let generation = service.begin_media_reconciliation().await.unwrap();
        let assignments = service
            .reconcile_media_catalog(scope, generation, &snapshots(), true)
            .await
            .unwrap();
        let group = service
            .get_media_version_group(
                &assignments[&sibling_mapping.virtual_media_id].virtual_media_id,
            )
            .await
            .unwrap()
            .unwrap();
        let source_generation = service.begin_media_reconciliation().await.unwrap();
        assert!(service
            .replace_media_version_sources(
                group.id,
                source_generation,
                &[sibling_mapping.id],
                &[MediaVersionSourceObservation {
                    member_mapping_id: sibling_mapping.id,
                    source_virtual_id: source_mapping.virtual_media_id.clone(),
                }],
            )
            .await
            .unwrap());
        groups.push(group);
    }

    let split_generation = service.begin_media_reconciliation().await.unwrap();
    let mut split_snapshots = snapshots();
    split_snapshots[1].observations[0].aliases = BTreeSet::from([MediaAlias {
        provider: crate::media_identity::MediaProvider::Tmdb,
        kind: crate::media_identity::MediaKind::Movie,
        provider_id: "99".to_string(),
    }]);
    service
        .reconcile_media_catalog(
            "configured:library:user-a",
            split_generation,
            &split_snapshots,
            true,
        )
        .await
        .unwrap();

    assert!(service
        .get_media_version_source_route(groups[1].id, &source_mapping.virtual_media_id)
        .await
        .unwrap()
        .is_some());
    assert_no_foreign_key_violations(&pool).await;
}
