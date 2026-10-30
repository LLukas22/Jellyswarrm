//! Resolve authorized media-source detail routes and project backend item details.
use crate::models::enums::BaseItemKind;
use std::collections::HashSet;

use serde_json::Value;
use tokio::task::JoinSet;
use tracing::{error, warn};

use crate::application::services::MediaServices;
use crate::{
    application::context::{RequestContext, TargetError},
    application::error::ApplicationError,
    application::upstream::response_json_to_payload,
    auth::service::AuthorizationSession,
    libraries::service::VirtualLibraryAccessScope,
    media::storage::{
        MediaMapping, MediaVersionGroup, MediaVersionMember, MediaVersionSourceObservation,
    },
    models::{MediaItem, MediaSource},
    processors::response_processor::ResponseProcessingProfile,
    protocol::identity::JellyfinAuthorization,
    protocol::url::{ensure_query_list_value, replace_path_id},
    servers::compare_routes,
    servers::storage::Server,
};

const MEDIA_SOURCES_FIELD: &str = "MediaSources";
const GROUPING_SOURCE_TYPE: &str = "Grouping";

/// Static inventories may replace refreshed member sources, but cannot revoke
/// a dynamically negotiated source still bound to this viewer's active player.
struct PreserveActiveNegotiatedSources;

impl PreserveActiveNegotiatedSources {
    fn retain(
        session: &crate::playback::PlaybackSession,
        route: &crate::media::storage::MediaVersionSourceRoute,
        refreshed_members: &[i64],
    ) -> bool {
        route.member_mapping.server_id == session.server_id
            && session.original_item_id.as_deref() == Some(&route.member_mapping.original_media_id)
            && refreshed_members.contains(&route.member_mapping.id)
    }
}

/// Select the owning item in client identity space, before backend translation.
/// Neither a source's ID nor a backend ID belongs in this path.
pub(super) fn normalize_detail_member(
    request: &mut reqwest::Request,
    mapping: &MediaMapping,
) -> Result<(), ApplicationError> {
    *request.url_mut() = replace_path_id(request.url(), "Items", &mapping.virtual_media_id)
        .ok_or(ApplicationError::InvalidInput("missing item path"))?;
    Ok(())
}

pub(crate) struct DetailMergeContext<'a> {
    pub(crate) requested_item_id: &'a str,
    pub(crate) selected_group: Option<MediaVersionGroup>,
    pub(crate) base_server: &'a Server,
    pub(crate) auth: &'a Option<JellyfinAuthorization>,
    pub(crate) access_scope: Option<&'a VirtualLibraryAccessScope>,
    pub(crate) sessions: Option<&'a [(AuthorizationSession, Server)]>,
    pub(crate) original_request: &'a reqwest::Request,
    pub(crate) source_generation: crate::media::storage::ReconciliationGeneration,
}

#[derive(Debug)]
struct VersionHost {
    member: MediaVersionMember,
    session: AuthorizationSession,
    server: Server,
}

#[derive(Debug)]
struct HostedMediaSources {
    member_mapping: MediaMapping,
    server: Server,
    sources: Vec<MediaSource>,
    user_data: Option<crate::models::UserData>,
    is_primary: bool,
}

/// Merges all reachable members of a stable media group into one typed item
/// response. Unknown Jellyfin fields survive through the flattened DTO fields.
pub(crate) async fn merge_media_detail(
    state: &MediaServices,
    context: DetailMergeContext<'_>,
    proxy_api_key: Option<&str>,
    payload: &mut Value,
) -> Result<(), ApplicationError> {
    if let Some(scope) = context.access_scope {
        let mut item: MediaItem = response_json_to_payload(payload.clone())?;
        preserve_media_parent_groups(state, &mut item, scope.user_id(), None).await?;
        *payload = serde_json::to_value(item).map_err(ApplicationError::Serialization)?;
    }
    let aggregate_group = state
        .media_storage
        .get_media_version_group(context.requested_item_id)
        .await
        .map_err(storage_error)?;
    let is_aggregate = aggregate_group.is_some();
    let Some(group) = aggregate_group.or_else(|| context.selected_group.clone()) else {
        return Ok(());
    };

    let mut item: MediaItem = response_json_to_payload(payload.clone())?;
    let Some(base_mapping) = state
        .media_storage
        .get_media_mapping_by_virtual(&item.id)
        .await
        .map_err(storage_error)?
    else {
        return Err(ApplicationError::MissingIdentity("base media mapping"));
    };

    let members = state
        .media_storage
        .get_media_version_members(group.id)
        .await
        .map_err(storage_error)?;
    let mut hosts = context
        .sessions
        .map(|sessions| authorized_hosts(members, sessions, context.access_scope, base_mapping.id))
        .unwrap_or_default();
    hosts.sort_by(compare_hosts);

    let mut hosted_sources = fetch_member_sources(state, &context, proxy_api_key, hosts).await;
    hosted_sources.sort_by(compare_hosted_sources);
    hosted_sources.insert(
        0,
        HostedMediaSources {
            member_mapping: base_mapping,
            server: context.base_server.clone(),
            sources: item.media_sources.take().unwrap_or_default(),
            user_data: item.user_data.clone(),
            is_primary: true,
        },
    );

    let refreshed_member_mapping_ids = hosted_sources
        .iter()
        .map(|hosted| hosted.member_mapping.id)
        .collect::<Vec<_>>();
    let (sources, mut observations) = merge_sources(&hosted_sources);
    if !sources.is_empty() {
        item.media_source_count = Some(sources.len().min(i32::MAX as usize) as i32);
        item.media_sources = Some(sources);
    }
    if is_aggregate {
        item.id = group.virtual_media_id.clone();
        if crate::media::identity::MediaKind::from_item_kind(&item.item_type)
            .is_some_and(|kind| kind.has_media_sources())
        {
            crate::media::catalog::merge_playback_user_data(
                &mut item.user_data,
                hosted_sources
                    .iter()
                    .filter_map(|hosted| hosted.user_data.as_ref()),
            );
        }
        if let Some(data) = &mut item.user_data {
            data.item_id = item.id.clone();
        }
    }

    // A static detail inventory cannot revoke a source negotiated by an active
    // player (for example the dynamic source returned by LiveStreams/Open).
    if let Some(scope) = context.access_scope {
        for session in state.play_sessions.sessions_for_user(scope.user_id()).await {
            let Some(route) = state
                .media_storage
                .get_media_version_source_route(group.id, &session.item_id)
                .await
                .map_err(storage_error)?
            else {
                continue;
            };
            if PreserveActiveNegotiatedSources::retain(
                &session,
                &route,
                &refreshed_member_mapping_ids,
            ) {
                observations.push(MediaVersionSourceObservation {
                    member_mapping_id: route.member_mapping.id,
                    source_virtual_id: session.item_id,
                });
            }
        }
    }
    let sources_replaced = state
        .media_storage
        .replace_media_version_sources(
            group.id,
            context.source_generation,
            &refreshed_member_mapping_ids,
            &observations,
        )
        .await
        .map_err(storage_error)?;
    if !sources_replaced {
        let mut routable_sources = Vec::new();
        for source in item.media_sources.take().unwrap_or_default() {
            if state
                .media_storage
                .get_media_version_source_route(group.id, &source.id)
                .await
                .map_err(storage_error)?
                .is_some()
            {
                routable_sources.push(source);
            }
        }
        item.media_source_count = Some(routable_sources.len().min(i32::MAX as usize) as i32);
        item.media_sources = Some(routable_sources);
    }
    *payload = serde_json::to_value(item).map_err(ApplicationError::Serialization)?;

    Ok(())
}

pub(crate) async fn preserve_media_parent_groups(
    state: &MediaServices,
    item: &mut MediaItem,
    viewer: &str,
    catalog_scope: Option<&str>,
) -> Result<(), ApplicationError> {
    if !matches!(item.item_type, BaseItemKind::Season | BaseItemKind::Episode) {
        return Ok(());
    }
    for parent in [&mut item.series_id, &mut item.season_id] {
        if let Some(id) = parent.as_deref() {
            let aggregate = match catalog_scope {
                Some(scope) => {
                    state
                        .media_storage
                        .get_media_parent_group_id_for_scope(id, viewer, Some(scope))
                        .await
                }
                None => {
                    state
                        .media_storage
                        .get_media_parent_group_id(id, viewer)
                        .await
                }
            }
            .map_err(storage_error)?;
            if let Some(aggregate) = aggregate {
                if item.parent_id.as_deref() == Some(id) {
                    item.parent_id = Some(aggregate.clone());
                }
                *parent = Some(aggregate);
            }
        }
    }
    Ok(())
}

fn authorized_hosts(
    members: Vec<MediaVersionMember>,
    sessions: &[(AuthorizationSession, Server)],
    access_scope: Option<&VirtualLibraryAccessScope>,
    base_mapping_id: i64,
) -> Vec<VersionHost> {
    members
        .into_iter()
        .filter(|member| member.mapping.id != base_mapping_id)
        .filter(|member| access_scope.is_none_or(|scope| scope.allows(member.mapping.server_id)))
        .filter_map(|member| {
            sessions
                .iter()
                .find(|(_session, server)| server.id == member.mapping.server_id)
                .map(|(session, server)| VersionHost {
                    member,
                    session: session.clone(),
                    server: server.clone(),
                })
        })
        .collect()
}

fn compare_hosts(left: &VersionHost, right: &VersionHost) -> std::cmp::Ordering {
    compare_routes(
        &left.server,
        &left.member.mapping.original_media_id,
        &right.server,
        &right.member.mapping.original_media_id,
    )
    .reverse()
}

fn compare_hosted_sources(
    left: &HostedMediaSources,
    right: &HostedMediaSources,
) -> std::cmp::Ordering {
    compare_routes(
        &left.server,
        &left.member_mapping.original_media_id,
        &right.server,
        &right.member_mapping.original_media_id,
    )
    .reverse()
}

async fn fetch_member_sources(
    state: &MediaServices,
    context: &DetailMergeContext<'_>,
    proxy_api_key: Option<&str>,
    hosts: Vec<VersionHost>,
) -> Vec<HostedMediaSources> {
    let mut join_set = JoinSet::new();

    for host in hosts {
        let Some(original_request) = context.original_request.try_clone() else {
            warn!("Failed to clone item detail request for media version fetch");
            continue;
        };
        join_set.spawn(fetch_member_source(
            state.clone(),
            original_request,
            context.auth.clone(),
            context.access_scope.cloned(),
            proxy_api_key.map(str::to_string),
            host,
        ));
    }

    let mut sources = Vec::new();
    while let Some(result) = join_set.join_next().await {
        match result {
            Ok(Ok(hosted)) => sources.push(hosted),
            Ok(Err(error)) => warn!(
                error = %error,
                "Media version detail fetch failed"
            ),
            Err(join_error) => error!("Media version detail task aborted: {join_error}"),
        }
    }
    sources
}

async fn fetch_member_source(
    state: MediaServices,
    mut original_request: reqwest::Request,
    auth: Option<JellyfinAuthorization>,
    access_scope: Option<VirtualLibraryAccessScope>,
    proxy_api_key: Option<String>,
    host: VersionHost,
) -> Result<HostedMediaSources, ApplicationError> {
    if !state
        .server_storage
        .server_status(host.server.id)
        .await
        .is_healthy()
    {
        return Err(ApplicationError::UnavailableOwner);
    }

    normalize_detail_member(&mut original_request, &host.member.mapping)?;
    ensure_query_list_value(original_request.url_mut(), "Fields", MEDIA_SOURCES_FIELD);

    let context = RequestContext::new(
        &original_request,
        auth,
        None,
        Some(vec![(host.session, host.server.clone())]),
        access_scope,
    );
    let target = context
        .target(host.server.id)
        .map_err(|error| match error {
            TargetError::AccessDenied | TargetError::ViewerMismatch => {
                ApplicationError::AccessDenied
            }
            TargetError::UnavailableOwner => ApplicationError::UnavailableOwner,
        })?;
    let request = state
        .request_builder()
        .build(&target, &original_request)
        .await?;

    let response = state
        .execute_processed_json(
            request,
            &host.server,
            ResponseProcessingProfile::Media,
            proxy_api_key.as_deref(),
        )
        .await?;
    let item: MediaItem = response_json_to_payload(response)?;

    Ok(HostedMediaSources {
        member_mapping: host.member.mapping,
        server: host.server,
        sources: item.media_sources.unwrap_or_default(),
        user_data: item.user_data,
        is_primary: false,
    })
}

fn merge_sources(
    hosted_sources: &[HostedMediaSources],
) -> (Vec<MediaSource>, Vec<MediaVersionSourceObservation>) {
    let mut seen_source_ids = HashSet::new();
    let mut sources = Vec::new();
    let mut observations = Vec::new();

    for hosted in hosted_sources {
        for mut source in hosted.sources.clone() {
            if !seen_source_ids.insert(source.id.to_ascii_lowercase()) {
                continue;
            }

            source.name = Some(version_name(&source, &hosted.server));
            if !hosted.is_primary {
                source.source_type = Some(GROUPING_SOURCE_TYPE.to_string());
            }
            observations.push(MediaVersionSourceObservation {
                member_mapping_id: hosted.member_mapping.id,
                source_virtual_id: source.id.clone(),
            });
            sources.push(source);
        }
    }

    (sources, observations)
}

pub(crate) fn version_name(source: &MediaSource, server: &Server) -> String {
    let name = source
        .name
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty());

    // Jellyfin uses the entire filename when it has no local alternate versions.
    // Do not repeat that title in the version selector. For its standard layout
    // `Movie (2026)/Movie (2026) - theatrical.mkv`, keep only the version suffix.
    // An already shortened upstream name is a version label and stays intact.
    let version = match source.path.as_deref().and_then(|path| {
        let (directory, filename) = path.rsplit_once(['/', '\\'])?;
        let stem = filename.rsplit_once('.').map_or(filename, |(stem, _)| stem);
        let folder = directory.rsplit(['/', '\\']).next()?;
        Some((stem, folder))
    }) {
        Some((stem, folder)) if name.is_none_or(|name| name.eq_ignore_ascii_case(stem)) => stem
            .get(..folder.len())
            .filter(|prefix| prefix.eq_ignore_ascii_case(folder))
            .and_then(|_| stem.get(folder.len()..))
            .filter(|suffix| suffix.starts_with([' ', '-', '_', '.']))
            .map(|suffix| suffix.trim_start_matches([' ', '-', '_', '.'])),
        _ => name,
    }
    .map(str::trim)
    .map(|version| {
        version
            .strip_prefix('[')
            .and_then(|version| version.strip_suffix(']'))
            .unwrap_or(version)
            .trim()
    })
    .filter(|version| !version.is_empty());

    match version {
        Some(version) => format!("[{}][{version}]", server.name),
        None => format!("[{}]", server.name),
    }
}

fn storage_error(error: sqlx::Error) -> ApplicationError {
    ApplicationError::Storage(error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{config::MediaStreamingMode, domain::ids::ServerId, servers::url::ServerUrl};
    use serde_json::json;

    fn server(server_id: i64, name: &str) -> Server {
        Server {
            id: ServerId::new(server_id),
            name: name.to_string(),
            url: ServerUrl::parse("http://example:8096").unwrap(),
            priority: 100,
            media_streaming_mode: MediaStreamingMode::Redirect,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        }
    }

    fn mapping(id: i64, server_id: i64) -> MediaMapping {
        MediaMapping {
            id,
            virtual_media_id: format!("virtual-{id}"),
            original_media_id: format!("original-{id}"),
            server_id: crate::domain::ids::ServerId::new(server_id),
            server_url: "http://example:8096".to_string(),
            created_at: chrono::Utc::now(),
        }
    }

    fn source(id: &str, name: Option<&str>, height: Option<i32>) -> MediaSource {
        serde_json::from_value(json!({
            "Id": id,
            "Name": name,
            "MediaStreams": height.map(|height| vec![json!({
                "Type": "Video",
                "Index": 0,
                "Height": height
            })])
        }))
        .unwrap()
    }

    #[test]
    fn version_labels_omit_filenames_but_preserve_editions() {
        for (name, path, expected) in [
            (Some("theatrical"), None, "[svr1][theatrical]"),
            (Some(" directors "), None, "[svr1][directors]"),
            (Some("[theatrical]"), None, "[svr1][theatrical]"),
            (Some("4K HDR"), Some("/media/Movie.mkv"), "[svr1][4K HDR]"),
            (
                Some("Movie (2026) - theatrical"),
                Some("/media/Movie (2026)/Movie (2026) - theatrical.mkv"),
                "[svr1][theatrical]",
            ),
            (
                Some("Movie (2026) - [directors]"),
                Some("/media/Movie (2026)/Movie (2026) - [directors].mkv"),
                "[svr1][directors]",
            ),
            (
                Some("Movie (2026)"),
                Some("/media/Movie (2026)/Movie (2026).mkv"),
                "[svr1]",
            ),
            (
                Some("Movie.2026.1080p.BluRay"),
                Some("/media/Movie (2026)/Movie.2026.1080p.BluRay.mkv"),
                "[svr1]",
            ),
            (
                Some("Movie (2026) - directors"),
                Some(r"D:\Movies\Movie (2026)\Movie (2026) - directors.mkv"),
                "[svr1][directors]",
            ),
            (
                Some("movie (2026) - theatrical"),
                Some("/media/Movie (2026)/movie (2026) - theatrical.mkv"),
                "[svr1][theatrical]",
            ),
            (
                Some("Movie sequel"),
                Some("/media/Movie/Movie sequel.mkv"),
                "[svr1][sequel]",
            ),
            (Some("Movies"), Some("/media/Movie/Movies.mkv"), "[svr1]"),
            (
                None,
                Some("/media/Movie/Movie - theatrical.mkv"),
                "[svr1][theatrical]",
            ),
            (None, None, "[svr1]"),
            (Some("  "), None, "[svr1]"),
        ] {
            let mut source = source("source", name, Some(1080));
            source.path = path.map(str::to_string);
            assert_eq!(
                version_name(&source, &server(1, "svr1")),
                expected,
                "{name:?}, {path:?}"
            );
        }
    }

    #[test]
    fn two_editions_on_each_server_keep_distinct_source_routes() {
        let hosted = (1..=2)
            .map(|id| HostedMediaSources {
                member_mapping: mapping(id, id),
                server: server(id, &format!("svr{id}")),
                sources: ["theatrical", "directors"]
                    .map(|edition| source(&format!("source-{id}-{edition}"), Some(edition), None))
                    .to_vec(),
                user_data: None,
                is_primary: id == 1,
            })
            .collect::<Vec<_>>();
        let (sources, routes) = merge_sources(&hosted);
        assert_eq!(
            sources
                .iter()
                .map(|source| source.name.as_deref().unwrap())
                .collect::<Vec<_>>(),
            [
                "[svr1][theatrical]",
                "[svr1][directors]",
                "[svr2][theatrical]",
                "[svr2][directors]"
            ]
        );
        for (index, (source, route)) in sources.iter().zip(&routes).enumerate() {
            assert_eq!(route.source_virtual_id, source.id);
            assert_eq!(route.member_mapping_id, (index / 2 + 1) as i64);
        }
    }

    #[test]
    fn active_source_preservation_requires_exact_owner_item_and_refreshed_member() {
        let member = mapping(1, 1);
        let route = crate::media::storage::MediaVersionSourceRoute {
            source_mapping: mapping(2, 1),
            member_mapping: member.clone(),
        };
        let mut session = crate::playback::PlaybackSession {
            session_id: "player".into(),
            item_id: route.source_mapping.virtual_media_id.clone(),
            user_id: "viewer".into(),
            server_id: member.server_id,
            original_item_id: Some(member.original_media_id.clone()),
        };
        assert!(PreserveActiveNegotiatedSources::retain(
            &session,
            &route,
            &[member.id]
        ));
        assert!(!PreserveActiveNegotiatedSources::retain(
            &session,
            &route,
            &[]
        ));
        session.original_item_id = Some("another-item".into());
        assert!(!PreserveActiveNegotiatedSources::retain(
            &session,
            &route,
            &[member.id]
        ));
        session.original_item_id = Some(member.original_media_id.clone());
        session.server_id = crate::domain::ids::ServerId::new(99);
        assert!(!PreserveActiveNegotiatedSources::retain(
            &session,
            &route,
            &[member.id]
        ));
    }

    #[test]
    fn member_detail_normalization_uses_item_virtual_identity_not_source_or_backend_id() {
        let mut member = mapping(1, 1);
        member.virtual_media_id = "11111111111111111111111111111111".into();
        // An upstream ID can itself look like another proxy ID. Feeding it to
        // the builder would translate that identity for a second time.
        member.original_media_id = "22222222222222222222222222222222".into();
        let mut request = reqwest::Request::new(
            reqwest::Method::GET,
            "http://proxy.example/jelly/Users/viewer/Items/33333333333333333333333333333333?Extension=one%2Ftwo&Extension=three"
                .parse()
                .unwrap(),
        );
        *request.body_mut() = Some("opaque body".into());
        normalize_detail_member(&mut request, &member).unwrap();
        assert_eq!(
            request.url().path(),
            "/jelly/Users/viewer/Items/11111111111111111111111111111111"
        );
        assert_eq!(
            request.url().query(),
            Some("Extension=one%2Ftwo&Extension=three")
        );
        assert_eq!(request.body().unwrap().as_bytes().unwrap(), b"opaque body");
    }

    #[test]
    fn preserves_source_ids_and_records_exact_routes() {
        let hosted = vec![
            HostedMediaSources {
                member_mapping: mapping(1, 1),
                server: server(1, "Primary"),
                sources: vec![source("source-a", Some("4K"), None)],
                user_data: None,
                is_primary: true,
            },
            HostedMediaSources {
                member_mapping: mapping(2, 2),
                server: server(2, "Backup"),
                sources: vec![source("source-b", None, Some(1080))],
                user_data: None,
                is_primary: false,
            },
        ];

        let (sources, observations) = merge_sources(&hosted);

        assert_eq!(sources[0].id, "source-a");
        assert_eq!(sources[1].id, "source-b");
        assert_eq!(sources[0].name.as_deref(), Some("[Primary][4K]"));
        assert_eq!(sources[1].name.as_deref(), Some("[Backup]"));
        assert_eq!(sources[0].source_type.as_deref(), None);
        assert_eq!(sources[1].source_type.as_deref(), Some("Grouping"));
        assert_eq!(observations[0].member_mapping_id, 1);
        assert_eq!(observations[0].source_virtual_id, "source-a");
        assert_eq!(observations[1].member_mapping_id, 2);
        assert_eq!(observations[1].source_virtual_id, "source-b");
    }

    #[test]
    fn selected_version_keeps_its_source_metadata_and_can_switch_back() {
        let selected: MediaSource = serde_json::from_value(json!({
            "Id": "selected-source",
            "Name": "Selected",
            "Type": "Default",
            "Path": "/selected/movie.mkv",
            "DirectStreamUrl": "/Videos/selected-owner/stream?MediaSourceId=selected-source",
            "MediaStreams": [{"Type": "Audio", "Index": 3, "Language": "deu"}],
            "DefaultAudioStreamIndex": 3,
            "SupportsDirectPlay": true
        }))
        .unwrap();
        let before = serde_json::to_value(&selected).unwrap();
        let (sources, routes) = merge_sources(&[
            HostedMediaSources {
                member_mapping: mapping(2, 2),
                server: server(2, "Selected"),
                sources: vec![selected],
                user_data: None,
                is_primary: true,
            },
            HostedMediaSources {
                member_mapping: mapping(1, 1),
                server: server(1, "Original"),
                sources: vec![source("original-source", None, None)],
                user_data: None,
                is_primary: false,
            },
        ]);
        let after = serde_json::to_value(&sources[0]).unwrap();
        for field in [
            "Id",
            "Type",
            "Path",
            "DirectStreamUrl",
            "MediaStreams",
            "DefaultAudioStreamIndex",
            "SupportsDirectPlay",
        ] {
            assert_eq!(after[field], before[field], "{field}");
        }
        assert_eq!(sources[1].id, "original-source");
        assert_eq!(sources[1].source_type.as_deref(), Some("Grouping"));
        assert_eq!(routes[0].member_mapping_id, 2);
        assert_eq!(routes[0].source_virtual_id, "selected-source");
    }

    #[test]
    fn duplicate_source_ids_are_not_exposed_twice() {
        let hosted = vec![
            HostedMediaSources {
                member_mapping: mapping(1, 1),
                server: server(1, "Primary"),
                sources: vec![source("same-source", None, None)],
                user_data: None,
                is_primary: true,
            },
            HostedMediaSources {
                member_mapping: mapping(2, 2),
                server: server(2, "Backup"),
                sources: vec![source("same-source", None, None)],
                user_data: None,
                is_primary: false,
            },
        ];

        let (sources, observations) = merge_sources(&hosted);

        assert_eq!(sources.len(), 1);
        assert_eq!(observations.len(), 1);
    }
}
