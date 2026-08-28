use std::collections::HashSet;

use hyper::StatusCode;
use serde_json::Value;
use tokio::task::JoinSet;
use tracing::{debug, error, warn};

use crate::{
    handlers::common::{execute_processed_json_request, response_json_to_payload},
    media_storage_service::{MediaMapping, MovieVersionMember, MovieVersionSourceObservation},
    models::{MediaItem, MediaSource},
    processors::response_processor::ResponseProcessingProfile,
    request_preprocessing::{
        apply_to_request, remap_authorization, JellyfinAuthorization, PreprocessedRequest,
    },
    server_storage::Server,
    url_helper::{ensure_query_list_value, replace_path_id},
    user_authorization_service::AuthorizationSession,
    virtual_library_service::{compare_virtual_library_routes, VirtualLibraryAccessScope},
    AppState,
};

const MEDIA_SOURCES_FIELD: &str = "MediaSources";
const GROUPING_SOURCE_TYPE: &str = "Grouping";

pub(super) struct DetailMergeContext<'a> {
    pub requested_item_id: &'a str,
    pub base_server: &'a Server,
    pub auth: &'a Option<JellyfinAuthorization>,
    pub access_scope: Option<&'a VirtualLibraryAccessScope>,
    pub sessions: Option<&'a [(AuthorizationSession, Server)]>,
    pub original_request: &'a reqwest::Request,
}

#[derive(Debug)]
struct VersionHost {
    member: MovieVersionMember,
    session: AuthorizationSession,
    server: Server,
}

#[derive(Debug)]
struct HostedMediaSources {
    member_mapping: MediaMapping,
    server: Server,
    sources: Vec<MediaSource>,
    is_primary: bool,
}

/// Merges all reachable members of a stable movie group into one typed item
/// response. Unknown Jellyfin fields survive through the flattened DTO fields.
pub(super) async fn merge_movie_detail(
    state: &AppState,
    context: DetailMergeContext<'_>,
    proxy_api_key: Option<&str>,
    payload: &mut Value,
) -> Result<(), StatusCode> {
    if !state.deduplicate_movies_enabled().await {
        return Ok(());
    }

    let Some(group) = state
        .media_storage
        .get_movie_version_group(context.requested_item_id)
        .await
        .map_err(storage_error)?
    else {
        return Ok(());
    };

    let mut item: MediaItem = response_json_to_payload(payload.clone())?;
    let Some(base_mapping) = state
        .media_storage
        .get_media_mapping_by_virtual(&item.id)
        .await
        .map_err(storage_error)?
    else {
        return Err(StatusCode::INTERNAL_SERVER_ERROR);
    };

    let members = state
        .media_storage
        .get_movie_version_members(group.id)
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
            is_primary: true,
        },
    );

    let refreshed_member_mapping_ids = hosted_sources
        .iter()
        .map(|hosted| hosted.member_mapping.id)
        .collect::<Vec<_>>();
    let (sources, observations) = merge_sources(&hosted_sources);
    if !sources.is_empty() {
        item.media_source_count = Some(sources.len().min(i32::MAX as usize) as i32);
        item.media_sources = Some(sources);
    }
    item.id = group.virtual_media_id.clone();

    let sources_replaced = state
        .media_storage
        .replace_movie_version_sources(group.id, &refreshed_member_mapping_ids, &observations)
        .await
        .map_err(storage_error)?;
    if !sources_replaced {
        return Err(StatusCode::CONFLICT);
    }
    *payload = serde_json::to_value(item).map_err(|conversion_error| {
        error!("Failed to serialize merged movie detail: {conversion_error}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    Ok(())
}

fn authorized_hosts(
    members: Vec<MovieVersionMember>,
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
    compare_virtual_library_routes(
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
    compare_virtual_library_routes(
        &left.server,
        &left.member_mapping.original_media_id,
        &right.server,
        &right.member_mapping.original_media_id,
    )
    .reverse()
}

async fn fetch_member_sources(
    state: &AppState,
    context: &DetailMergeContext<'_>,
    proxy_api_key: Option<&str>,
    hosts: Vec<VersionHost>,
) -> Vec<HostedMediaSources> {
    let mut join_set = JoinSet::new();

    for host in hosts {
        let Some(original_request) = context.original_request.try_clone() else {
            warn!("Failed to clone item detail request for movie version fetch");
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
            Ok(Err(status)) => warn!("Movie version detail fetch failed with status {status}"),
            Err(join_error) => error!("Movie version detail task aborted: {join_error}"),
        }
    }
    sources
}

async fn fetch_member_source(
    state: AppState,
    mut original_request: reqwest::Request,
    auth: Option<JellyfinAuthorization>,
    access_scope: Option<VirtualLibraryAccessScope>,
    proxy_api_key: Option<String>,
    host: VersionHost,
) -> Result<HostedMediaSources, StatusCode> {
    if !state
        .server_storage
        .server_status(host.server.id)
        .await
        .is_healthy()
    {
        return Err(StatusCode::SERVICE_UNAVAILABLE);
    }

    let Some(url) = replace_path_id(
        original_request.url(),
        "Items",
        &host.member.mapping.virtual_media_id,
    ) else {
        return Err(StatusCode::BAD_REQUEST);
    };
    *original_request.url_mut() = url;
    ensure_query_list_value(original_request.url_mut(), "Fields", MEDIA_SOURCES_FIELD);

    let new_auth = remap_authorization(&auth, &Some(host.session.clone()))
        .await
        .map_err(unexpected_error)?;
    apply_to_request(
        &mut original_request,
        &host.server,
        &Some(host.session),
        &new_auth,
        &state,
        access_scope.as_ref(),
    )
    .await;

    let response = execute_processed_json_request(
        &state,
        original_request,
        &host.server,
        ResponseProcessingProfile::Media,
        false,
        proxy_api_key.as_deref(),
    )
    .await?;
    let item: MediaItem = response_json_to_payload(response)?;

    Ok(HostedMediaSources {
        member_mapping: host.member.mapping,
        server: host.server,
        sources: item.media_sources.unwrap_or_default(),
        is_primary: false,
    })
}

fn merge_sources(
    hosted_sources: &[HostedMediaSources],
) -> (Vec<MediaSource>, Vec<MovieVersionSourceObservation>) {
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
            observations.push(MovieVersionSourceObservation {
                member_mapping_id: hosted.member_mapping.id,
                source_virtual_id: source.id.clone(),
            });
            sources.push(source);
        }
    }

    (sources, observations)
}

fn version_name(source: &MediaSource, server: &Server) -> String {
    let upstream_name = source
        .name
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty());
    let resolution = source.media_streams.as_ref().and_then(|streams| {
        streams.iter().find_map(|stream| {
            stream
                .stream_type
                .as_deref()
                .is_some_and(|stream_type| stream_type.eq_ignore_ascii_case("Video"))
                .then_some(stream.height)
                .flatten()
                .map(|height| format!("{height}p"))
        })
    });

    match upstream_name.or(resolution.as_deref()) {
        Some(name) => format!("{name} [{}]", server.name),
        None => server.name.clone(),
    }
}

// ---------------------------------------------------------------------------
// Playback routing
// ---------------------------------------------------------------------------

pub(super) enum PlaybackRouteDecision {
    Original,
    Rerouted(Box<ReroutedPlaybackRoute>),
    InvalidSelectedSource,
    SelectedSourceUnavailable,
}

pub(super) struct ReroutedPlaybackRoute {
    pub server: Server,
    pub session: AuthorizationSession,
    pub request: reqwest::Request,
}

pub(super) async fn record_playback_sources(
    state: &AppState,
    aggregate_id: &str,
    server: &Server,
    sources: &[MediaSource],
) -> Result<(), StatusCode> {
    if !state.deduplicate_movies_enabled().await {
        return Ok(());
    }
    let Some(group) = state
        .media_storage
        .get_movie_version_group(aggregate_id)
        .await
        .map_err(storage_error)?
    else {
        return Ok(());
    };
    let Some(member) = state
        .media_storage
        .get_movie_version_members(group.id)
        .await
        .map_err(storage_error)?
        .into_iter()
        .find(|member| member.mapping.server_id == server.id)
    else {
        return Ok(());
    };
    let observations = sources
        .iter()
        .map(|source| MovieVersionSourceObservation {
            member_mapping_id: member.mapping.id,
            source_virtual_id: source.id.clone(),
        })
        .collect::<Vec<_>>();
    let sources_replaced = state
        .media_storage
        .replace_movie_version_sources(group.id, &[member.mapping.id], &observations)
        .await
        .map_err(storage_error)?;
    sources_replaced.then_some(()).ok_or(StatusCode::CONFLICT)
}

/// Resolves an explicit source selection through the source routes recorded by
/// the item detail response. Requests without an explicit source need no
/// special handling: aggregate URL preprocessing already selects the best
/// healthy group member.
pub(super) async fn resolve_playback_route(
    state: &AppState,
    preprocessed: &PreprocessedRequest,
    requested_item_id: &str,
    selected_source_id: Option<&str>,
) -> Result<PlaybackRouteDecision, StatusCode> {
    if !state.deduplicate_movies_enabled().await {
        return Ok(PlaybackRouteDecision::Original);
    }

    let Some(group) = state
        .media_storage
        .get_movie_version_group(requested_item_id)
        .await
        .map_err(storage_error)?
    else {
        return Ok(PlaybackRouteDecision::Original);
    };
    let Some(selected_source_id) = selected_source_id.filter(|id| !id.trim().is_empty()) else {
        return Ok(PlaybackRouteDecision::Original);
    };
    let Some(route) = state
        .media_storage
        .get_movie_version_source_route(group.id, selected_source_id)
        .await
        .map_err(storage_error)?
    else {
        return Ok(PlaybackRouteDecision::InvalidSelectedSource);
    };
    if preprocessed
        .access_scope
        .as_ref()
        .is_some_and(|scope| !scope.allows(route.member_mapping.server_id))
    {
        return Ok(PlaybackRouteDecision::InvalidSelectedSource);
    }
    if !state
        .server_storage
        .server_status(route.member_mapping.server_id)
        .await
        .is_healthy()
    {
        return Ok(PlaybackRouteDecision::SelectedSourceUnavailable);
    }

    if route.member_mapping.server_id == preprocessed.server.id {
        return Ok(PlaybackRouteDecision::Original);
    }
    let Some((session, server)) = preprocessed.sessions.as_ref().and_then(|sessions| {
        sessions
            .iter()
            .find(|(_session, server)| server.id == route.member_mapping.server_id)
    }) else {
        return Ok(PlaybackRouteDecision::SelectedSourceUnavailable);
    };

    let mut request = preprocessed
        .original_request
        .try_clone()
        .ok_or(StatusCode::BAD_REQUEST)?;
    let Some(url) = replace_path_id(
        request.url(),
        "Items",
        &route.member_mapping.virtual_media_id,
    ) else {
        return Err(StatusCode::BAD_REQUEST);
    };
    *request.url_mut() = url;

    let new_auth = remap_authorization(&preprocessed.auth, &Some(session.clone()))
        .await
        .map_err(unexpected_error)?;
    apply_to_request(
        &mut request,
        server,
        &Some(session.clone()),
        &new_auth,
        state,
        preprocessed.access_scope.as_ref(),
    )
    .await;

    debug!(
        "Routing aggregate movie {} source {} to server {}",
        group.virtual_media_id, route.source_mapping.virtual_media_id, server.name
    );
    Ok(PlaybackRouteDecision::Rerouted(Box::new(
        ReroutedPlaybackRoute {
            server: server.clone(),
            session: session.clone(),
            request,
        },
    )))
}

fn storage_error(error: sqlx::Error) -> StatusCode {
    error!("Movie version storage operation failed: {error}");
    StatusCode::INTERNAL_SERVER_ERROR
}

fn unexpected_error(error: anyhow::Error) -> StatusCode {
    error!("Movie version request preparation failed: {error}");
    StatusCode::INTERNAL_SERVER_ERROR
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{config::MediaStreamingMode, server_id::ServerId, server_url::ServerUrl};
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
            server_id: crate::server_id::ServerId::new(server_id),
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
    fn preserves_source_ids_and_records_exact_routes() {
        let hosted = vec![
            HostedMediaSources {
                member_mapping: mapping(1, 1),
                server: server(1, "Primary"),
                sources: vec![source("source-a", Some("4K"), None)],
                is_primary: true,
            },
            HostedMediaSources {
                member_mapping: mapping(2, 2),
                server: server(2, "Backup"),
                sources: vec![source("source-b", None, Some(1080))],
                is_primary: false,
            },
        ];

        let (sources, observations) = merge_sources(&hosted);

        assert_eq!(sources[0].id, "source-a");
        assert_eq!(sources[1].id, "source-b");
        assert_eq!(sources[0].name.as_deref(), Some("4K [Primary]"));
        assert_eq!(sources[1].name.as_deref(), Some("1080p [Backup]"));
        assert_eq!(sources[0].source_type.as_deref(), None);
        assert_eq!(sources[1].source_type.as_deref(), Some("Grouping"));
        assert_eq!(observations[0].member_mapping_id, 1);
        assert_eq!(observations[0].source_virtual_id, "source-a");
        assert_eq!(observations[1].member_mapping_id, 2);
        assert_eq!(observations[1].source_virtual_id, "source-b");
    }

    #[test]
    fn duplicate_source_ids_are_not_exposed_twice() {
        let hosted = vec![
            HostedMediaSources {
                member_mapping: mapping(1, 1),
                server: server(1, "Primary"),
                sources: vec![source("same-source", None, None)],
                is_primary: true,
            },
            HostedMediaSources {
                member_mapping: mapping(2, 2),
                server: server(2, "Backup"),
                sources: vec![source("same-source", None, None)],
                is_primary: false,
            },
        ];

        let (sources, observations) = merge_sources(&hosted);

        assert_eq!(sources.len(), 1);
        assert_eq!(observations.len(), 1);
    }
}
