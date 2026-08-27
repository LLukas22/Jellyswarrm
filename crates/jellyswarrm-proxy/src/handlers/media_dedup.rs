use std::collections::HashSet;

use hyper::StatusCode;
use serde_json::Value;
use tokio::task::JoinSet;
use tracing::{debug, error, warn};

use crate::{
    handlers::common::execute_processed_json_request,
    media_storage_service::MediaMapping,
    processors::response_processor::ResponseProcessingProfile,
    request_preprocessing::{
        apply_to_request, remap_authorization, JellyfinAuthorization, PreprocessedRequest,
    },
    server_id::ServerId,
    server_storage::Server,
    user_authorization_service::AuthorizationSession,
    virtual_library_service::VirtualLibraryAccessScope,
    AppState,
};

const MOVIE_ITEM_TYPE: &str = "Movie";
const GROUPING_SOURCE_TYPE: &str = "Grouping";
const PLAYBACK_INFO_SEGMENT: &str = "PlaybackInfo";

/// One alternate version gathered from a sibling server.
#[derive(Debug)]
struct AlternateMediaSources {
    virtual_media_id: String,
    server: Server,
    sources: Vec<Value>,
}

/// A sibling mapping joined with the caller's authorization on its host.
#[derive(Debug)]
struct VersionHost {
    mapping: MediaMapping,
    server: Server,
    session: AuthorizationSession,
}

/// A fully prepared outbound request for a playback negotiation against another
/// host than the one the preprocessor selected.
pub(super) struct ReroutedPlayback {
    pub server: Server,
    pub session: AuthorizationSession,
    pub request: reqwest::Request,
    /// The requested media source does not exist on the rerouting host; serving
    /// the host's default version keeps playback alive while the real source is
    /// unreachable.
    pub drop_media_source: bool,
}

// ---------------------------------------------------------------------------
// Item details: assemble multi-server versions
// ---------------------------------------------------------------------------

/// Borrowed pieces of a preprocessed request required to assemble merged
/// versions. Exists separately so callers that consumed their preprocessed
/// request body can still drive a merge.
pub(super) struct DetailMergeContext<'a> {
    pub auth: &'a Option<JellyfinAuthorization>,
    pub access_scope: Option<&'a VirtualLibraryAccessScope>,
    pub sessions: Option<&'a [(AuthorizationSession, Server)]>,
    pub original_request: &'a reqwest::Request,
}

/// Attaches media sources of duplicate movie copies hosted on other servers to
/// a freshly processed item payload, mimicking Jellyfin's linked versions.
///
/// Clients render their "Version" selection purely from the payload's
/// `MediaSources` array plus its `MediaSourceCount`, so injecting one entry per
/// hosting server makes merged duplicates behave like native versions.
pub(super) async fn attach_alternate_versions(
    state: &AppState,
    context: DetailMergeContext<'_>,
    proxy_api_key: Option<&str>,
    payload: &mut Value,
) -> Result<(), StatusCode> {
    if !state.deduplicate_movies_enabled().await || !is_item_detail_url(context.original_request) {
        return Ok(());
    }

    let Some((mapping, provider_key)) = item_duplicate_identity(state, payload).await? else {
        return Ok(());
    };
    let Some(sessions) = context.sessions else {
        return Ok(());
    };

    let hosts = sibling_hosts(state, &provider_key, mapping.server_id, sessions).await?;
    if hosts.is_empty() {
        return Ok(());
    }
    debug!(
        "Merging {} alternate copy/ies of item {}",
        hosts.len(),
        mapping.virtual_media_id
    );

    let alternates = fetch_alternate_media_sources(state, context, proxy_api_key, hosts).await;
    splice_alternate_sources(payload, &alternates);

    Ok(())
}

async fn item_duplicate_identity(
    state: &AppState,
    payload: &Value,
) -> Result<Option<(MediaMapping, String)>, StatusCode> {
    let is_movie = payload
        .get("Type")
        .and_then(Value::as_str)
        .is_some_and(|item_type| item_type.eq_ignore_ascii_case(MOVIE_ITEM_TYPE));
    if !is_movie {
        return Ok(None);
    }

    let Some(item_id) = payload
        .get("Id")
        .and_then(Value::as_str)
        .map(str::to_string)
    else {
        return Ok(None);
    };

    let Some((mapping, _server)) = state
        .media_storage
        .get_media_mapping_with_server(&item_id)
        .await
        .map_err(storage_error)?
    else {
        return Ok(None);
    };

    let Some(provider_key) = state
        .media_storage
        .get_provider_key(&mapping.virtual_media_id)
        .await
        .map_err(storage_error)?
    else {
        return Ok(None);
    };

    Ok(Some((mapping, provider_key)))
}

fn is_item_detail_url(original_request: &reqwest::Request) -> bool {
    original_request
        .url()
        .path_segments()
        .map(Iterator::collect::<Vec<_>>)
        .is_some_and(|segments| {
            segments.len() >= 2
                && segments[segments.len() - 2].eq_ignore_ascii_case("Items")
                && !segments[segments.len() - 1].eq_ignore_ascii_case(PLAYBACK_INFO_SEGMENT)
        })
}

async fn sibling_hosts(
    state: &AppState,
    provider_key: &str,
    exclude_server_id: ServerId,
    sessions: &[(AuthorizationSession, Server)],
) -> Result<Vec<VersionHost>, StatusCode> {
    let members = state
        .media_storage
        .find_duplicate_group_members(provider_key, exclude_server_id)
        .await
        .map_err(storage_error)?;

    Ok(members
        .into_iter()
        .filter_map(|mapping| {
            sessions
                .iter()
                .find(|(_session, server)| server.id == mapping.server_id)
                .map(|(session, server)| VersionHost {
                    mapping,
                    server: server.clone(),
                    session: session.clone(),
                })
        })
        .collect())
}

async fn fetch_alternate_media_sources(
    state: &AppState,
    context: DetailMergeContext<'_>,
    proxy_api_key: Option<&str>,
    hosts: Vec<VersionHost>,
) -> Vec<AlternateMediaSources> {
    let mut join_set = JoinSet::new();

    for host in hosts {
        let Some(original_request) = context.original_request.try_clone() else {
            warn!("Failed to clone item detail request for sibling fetch");
            continue;
        };
        join_set.spawn(fetch_one_host(
            state.clone(),
            original_request,
            context.auth.clone(),
            context.access_scope.cloned(),
            proxy_api_key.map(str::to_string),
            host,
        ));
    }

    let mut alternates = Vec::new();
    while let Some(result) = join_set.join_next().await {
        match result {
            Ok(Ok(alternate)) => alternates.push(alternate),
            Ok(Err(status)) => warn!("Sibling version fetch failed with status {status}"),
            Err(join_error) => error!("Sibling version task aborted: {join_error}"),
        }
    }
    alternates
}

async fn fetch_one_host(
    state: AppState,
    mut original_request: reqwest::Request,
    auth: Option<JellyfinAuthorization>,
    access_scope: Option<VirtualLibraryAccessScope>,
    proxy_api_key: Option<String>,
    host: VersionHost,
) -> Result<AlternateMediaSources, StatusCode> {
    let virtual_media_id = host.mapping.virtual_media_id.clone();
    if !swap_last_path_segment(original_request.url_mut(), &virtual_media_id) {
        return Err(StatusCode::BAD_REQUEST);
    }
    ensure_query_field(original_request.url_mut(), "Fields", "MediaSources");

    let session = host.session.clone();
    let new_auth = remap_authorization(&auth, &Some(session))
        .await
        .map_err(unexpected_error)?;

    apply_to_request(
        &mut original_request,
        &host.server,
        &Some(host.session.clone()),
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

    let sources = response
        .get("MediaSources")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    Ok(AlternateMediaSources {
        virtual_media_id,
        server: host.server,
        sources,
    })
}

fn splice_alternate_sources(payload: &mut Value, alternates: &[AlternateMediaSources]) {
    let Some(sources) = payload
        .get_mut("MediaSources")
        .and_then(Value::as_array_mut)
    else {
        debug!("Payload carries no media sources; skipping version merge");
        return;
    };

    let mut known_ids = sources
        .iter()
        .filter_map(|source| source.get("Id").and_then(Value::as_str))
        .map(str::to_ascii_lowercase)
        .collect::<HashSet<_>>();

    for alternate in alternates {
        for mut source in alternate.sources.clone() {
            let Some(source_id) = source.get("Id").and_then(Value::as_str).map(str::to_string)
            else {
                continue;
            };
            if !known_ids.insert(source_id.to_ascii_lowercase()) {
                continue;
            }

            source["Id"] = Value::String(alternate.virtual_media_id.clone());
            source["Name"] = Value::String(version_label(&alternate.server, &source));
            source["Type"] = Value::String(GROUPING_SOURCE_TYPE.to_string());
            sources.push(source);
        }
    }

    let count = sources.len();
    payload["MediaSourceCount"] = Value::from(count.min(i32::MAX as usize) as i32);
}

/// Labels a version like `2160p [<server name>]`, falling back to the plain
/// server name when no video stream exposes a height.
fn version_label(server: &Server, source: &Value) -> String {
    let height = source
        .get("MediaStreams")
        .and_then(Value::as_array)
        .and_then(|streams| {
            streams
                .iter()
                .find(|stream| {
                    stream
                        .get("Type")
                        .and_then(Value::as_str)
                        .is_some_and(|stream_type| stream_type.eq_ignore_ascii_case("Video"))
                })
                .and_then(|video| video.get("Height"))
                .and_then(Value::as_i64)
        });

    match height {
        Some(height) => format!("{height}p [{}]", server.name),
        None => server.name.clone(),
    }
}

// ---------------------------------------------------------------------------
// Playback negotiation: use whichever healthy host owns the content
// ---------------------------------------------------------------------------

/// Decides whether a PlaybackInfo negotiation must run against another host
/// than the one the preprocessor chose:
///
/// - the client explicitly picked a version (media source) hosted on another
///   server that carries an identical copy of the requested item, or
/// - the host backing the requested item dropped out of the healthy session
///   set while another server holds an identical copy.
///
/// Returns `None` to keep the preprocessor's routing decision.
pub(super) async fn resolve_rerouted_playback(
    state: &AppState,
    preprocessed: &PreprocessedRequest,
    media_source_id: Option<&str>,
) -> Result<Option<ReroutedPlayback>, StatusCode> {
    let Some(sessions) = preprocessed.sessions.as_ref() else {
        return Ok(None);
    };

    let Some(item_id) = playback_url_item_id(preprocessed.original_request.url()) else {
        return Ok(None);
    };

    let Some((item_mapping, _)) = state
        .media_storage
        .get_media_mapping_with_server(&item_id)
        .await
        .map_err(storage_error)?
    else {
        return Ok(None);
    };

    if let Some(media_source_id) = media_source_id.filter(|id| !id.trim().is_empty()) {
        if let Some(route) = explicit_version_route(
            state,
            preprocessed,
            sessions,
            &item_mapping,
            media_source_id,
        )
        .await?
        {
            return Ok(Some(route));
        }
    }

    // The item's own host still has an authorized, healthy session.
    if authorized_host(sessions, item_mapping.server_id).is_some() {
        return Ok(None);
    }

    fallback_version_route(state, preprocessed, sessions, &item_mapping).await
}

async fn explicit_version_route(
    state: &AppState,
    preprocessed: &PreprocessedRequest,
    sessions: &[(AuthorizationSession, Server)],
    item_mapping: &MediaMapping,
    media_source_id: &str,
) -> Result<Option<ReroutedPlayback>, StatusCode> {
    let Some((source_mapping, source_server)) = state
        .media_storage
        .get_media_mapping_with_server(media_source_id)
        .await
        .map_err(storage_error)?
    else {
        return Ok(None);
    };
    if source_server.id == item_mapping.server_id {
        return Ok(None);
    }

    if !copies_share_identity(state, &source_mapping, item_mapping).await? {
        return Ok(None);
    }

    let Some((session, _)) = authorized_host(sessions, source_server.id) else {
        warn!(
            "Selected version {} lives on unreachable server {}",
            source_mapping.virtual_media_id, source_server.name
        );
        return Ok(None);
    };
    let session = session.clone();

    debug!(
        "Routing playback of selected version {} on server {}",
        source_mapping.virtual_media_id, source_server.name
    );

    build_rerouted(
        state,
        preprocessed,
        source_server,
        session,
        source_mapping,
        false,
    )
    .await
}

async fn fallback_version_route(
    state: &AppState,
    preprocessed: &PreprocessedRequest,
    sessions: &[(AuthorizationSession, Server)],
    item_mapping: &MediaMapping,
) -> Result<Option<ReroutedPlayback>, StatusCode> {
    let Some(provider_key) = state
        .media_storage
        .get_provider_key(&item_mapping.virtual_media_id)
        .await
        .map_err(storage_error)?
    else {
        return Ok(None);
    };

    let members = state
        .media_storage
        .find_duplicate_group_members(&provider_key, item_mapping.server_id)
        .await
        .map_err(storage_error)?;

    // Prefer the candidate whose host ranks highest in the priority-ordered
    // healthy session list.
    let best = members
        .into_iter()
        .filter_map(|member| {
            let rank = sessions
                .iter()
                .position(|(_session, server)| server.id == member.server_id)?;
            Some((rank, member))
        })
        .min_by_key(|(rank, _)| *rank);

    let Some((rank, member)) = best else {
        return Ok(None);
    };
    let Some((session, server)) = sessions.get(rank) else {
        return Ok(None);
    };

    debug!(
        "Falling back playback of {} to identical copy on server {}",
        item_mapping.virtual_media_id, server.name
    );

    build_rerouted(
        state,
        preprocessed,
        server.clone(),
        session.clone(),
        member,
        true,
    )
    .await
}

async fn build_rerouted(
    state: &AppState,
    preprocessed: &PreprocessedRequest,
    server: Server,
    session: AuthorizationSession,
    target_mapping: MediaMapping,
    drop_media_source: bool,
) -> Result<Option<ReroutedPlayback>, StatusCode> {
    let mut request = preprocessed
        .original_request
        .try_clone()
        .ok_or(StatusCode::BAD_REQUEST)?;
    if !swap_playback_item_segment(request.url_mut(), &target_mapping.virtual_media_id) {
        warn!("Unrecognized playback URL shape; keeping original routing");
        return Ok(None);
    }

    let new_auth = remap_authorization(&preprocessed.auth, &Some(session.clone()))
        .await
        .map_err(unexpected_error)?;
    apply_to_request(
        &mut request,
        &server,
        &Some(session.clone()),
        &new_auth,
        state,
        preprocessed.access_scope.as_ref(),
    )
    .await;

    Ok(Some(ReroutedPlayback {
        server,
        session,
        request,
        drop_media_source,
    }))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// The duplicate identity two mappings share, if any. Only items that carry an
/// identical provider identity are interchangeable versions.
async fn copies_share_identity(
    state: &AppState,
    left: &MediaMapping,
    right: &MediaMapping,
) -> Result<bool, StatusCode> {
    match (
        state
            .media_storage
            .get_provider_key(&left.virtual_media_id)
            .await
            .map_err(storage_error)?,
        state
            .media_storage
            .get_provider_key(&right.virtual_media_id)
            .await
            .map_err(storage_error)?,
    ) {
        (Some(left_key), Some(right_key)) => Ok(left_key == right_key),
        _ => Ok(false),
    }
}

fn authorized_host(
    sessions: &[(AuthorizationSession, Server)],
    server_id: ServerId,
) -> Option<&(AuthorizationSession, Server)> {
    sessions
        .iter()
        .find(|(_session, server)| server.id == server_id)
}

/// Extracts the virtual media id a playback negotiation targets from URLs like
/// `/Users/{uid}/Items/{id}/PlaybackInfo`.
fn playback_url_item_id(url: &url::Url) -> Option<String> {
    let segments = url.path_segments()?.map(str::to_string).collect::<Vec<_>>();
    let item_index = playback_item_segment_index(&segments)?;
    segments.get(item_index).cloned()
}

/// Index of the media-id segment preceding a trailing PlaybackInfo segment.
fn playback_item_segment_index(segments: &[String]) -> Option<usize> {
    let playback_index = segments
        .iter()
        .rposition(|segment| segment.eq_ignore_ascii_case(PLAYBACK_INFO_SEGMENT))?;
    playback_index.checked_sub(1)
}

fn swap_last_path_segment(url: &mut url::Url, replacement: &str) -> bool {
    let Some(segments) = url
        .path_segments()
        .map(|segments| segments.map(str::to_string).collect::<Vec<_>>())
    else {
        return false;
    };
    if segments.is_empty() {
        return false;
    }

    let joined = format!(
        "{}/{}",
        segments[..segments.len() - 1].join("/"),
        replacement
    );
    url.set_path(&joined);
    true
}

fn swap_playback_item_segment(url: &mut url::Url, replacement: &str) -> bool {
    let Some(mut segments) = url
        .path_segments()
        .map(|segments| segments.map(str::to_string).collect::<Vec<_>>())
    else {
        return false;
    };

    let Some(item_index) = playback_item_segment_index(&segments) else {
        return false;
    };
    segments[item_index] = replacement.to_string();
    url.set_path(&segments.join("/"));
    true
}

/// Ensures `Fields=...,<value>` without disturbing any other query parameters.
fn ensure_query_field(url: &mut url::Url, key: &str, value: &str) {
    let mut pairs = url
        .query_pairs()
        .map(|(name, entry)| (name.into_owned(), entry.into_owned()))
        .collect::<Vec<_>>();

    if let Some(fields_entry) = pairs
        .iter_mut()
        .find(|(name, _entry)| name.eq_ignore_ascii_case(key))
    {
        let already_present = fields_entry
            .1
            .split(',')
            .any(|field| field.trim().eq_ignore_ascii_case(value));
        if already_present {
            return;
        }
        fields_entry.1 = format!("{},{value}", fields_entry.1);
    } else {
        pairs.push((key.to_string(), value.to_string()));
    }

    url.query_pairs_mut().clear().extend_pairs(pairs);
}

fn storage_error(error: sqlx::Error) -> StatusCode {
    error!("Media storage lookup failed: {error}");
    StatusCode::INTERNAL_SERVER_ERROR
}

fn unexpected_error(error: anyhow::Error) -> StatusCode {
    error!("{error}");
    StatusCode::INTERNAL_SERVER_ERROR
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::MediaStreamingMode;
    use serde_json::json;

    fn server_fixture(server_id: i64) -> Server {
        Server {
            id: ServerId::new(server_id),
            name: format!("Server {server_id}"),
            url: crate::server_url::ServerUrl::parse("http://example:8096").unwrap(),
            priority: 100,
            media_streaming_mode: MediaStreamingMode::Redirect,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        }
    }

    #[test]
    fn version_labels_prefer_video_height_and_fall_back_to_server_name() {
        let server = server_fixture(2);
        let with_height = json!({
            "MediaStreams": [
                {"Type": "Audio", "Codec": "aac"},
                {"Type": "Video", "Height": 2160, "Width": 3840}
            ]
        });
        let without_video = json!({"MediaStreams": [{"Type": "Audio", "Codec": "aac"}]});

        assert_eq!(version_label(&server, &with_height), "2160p [Server 2]");
        assert_eq!(version_label(&server, &without_video), "Server 2");
    }

    #[test]
    fn splicing_rewrites_ids_labels_types_and_count() {
        let alternate = server_fixture(2);

        let mut payload = json!({
            "Id": "primary-virtual",
            "Type": "Movie",
            "MediaSourceCount": 1,
            "MediaSources": [
                {"Id": "primary-source", "Name": "1080p", "Type": "Default"}
            ]
        });
        let alternates = vec![AlternateMediaSources {
            virtual_media_id: "alternate-virtual".to_string(),
            server: alternate,
            sources: vec![json!({
                "Id": "primary-source",
                "Name": "dup-source-id",
                "Path": "/data/movie.mkv"
            })],
        }];

        splice_alternate_sources(&mut payload, &alternates);

        // A source id colliding with the primary's own source must be skipped,
        // otherwise clients would see a phantom version.
        let sources = payload["MediaSources"].as_array().unwrap();
        assert_eq!(sources.len(), 1);
        assert_eq!(payload["MediaSourceCount"], 1);
        assert_eq!(sources[0]["Id"], "primary-source");
    }

    #[test]
    fn splicing_appends_unknown_sibling_sources_as_grouping_type() {
        let alternate = server_fixture(3);
        let mut payload = json!({
            "Id": "primary-virtual",
            "Type": "Movie",
            "MediaSources": [
                {"Id": "primary-source", "Name": "1080p"}
            ]
        });
        let alternates = vec![AlternateMediaSources {
            virtual_media_id: "alternate-virtual".to_string(),
            server: alternate,
            sources: vec![
                json!({"Id": "sibling-source", "Path": "/data/movie.mkv"}),
                json!({"Id": "sibling-source"}),
            ],
        }];

        splice_alternate_sources(&mut payload, &alternates);

        let sources = payload["MediaSources"].as_array().unwrap();
        assert_eq!(
            sources.len(),
            2,
            "duplicate sibling sources collapse into one entry"
        );
        assert_eq!(sources[1]["Id"], "alternate-virtual");
        assert_eq!(sources[1]["Name"], "Server 3");
        assert_eq!(sources[1]["Type"], "Grouping");
        assert_eq!(payload["MediaSourceCount"], 2);
    }

    #[test]
    fn splicing_is_skipped_without_a_media_sources_array() {
        let mut payload = json!({"Id": "primary-virtual", "Type": "Movie"});
        let alternates = vec![AlternateMediaSources {
            virtual_media_id: "alternate-virtual".to_string(),
            server: server_fixture(2),
            sources: vec![json!({"Id": "sibling-source"})],
        }];

        splice_alternate_sources(&mut payload, &alternates);

        assert!(payload.get("MediaSources").is_none());
    }

    #[test]
    fn playback_item_segment_extraction_and_replacement() {
        let url = url::Url::parse("http://localhost/Users/u1/Items/abc/PlaybackInfo").unwrap();
        assert_eq!(playback_url_item_id(&url).as_deref(), Some("abc"));

        let mut url = url.clone();
        assert!(swap_playback_item_segment(&mut url, "def"));
        assert_eq!(playback_url_item_id(&url).as_deref(), Some("def"));

        let detail = url::Url::parse("http://localhost/Items/abc").unwrap();
        assert_eq!(playback_url_item_id(&detail), None);
    }

    #[test]
    fn last_path_segment_swap_targets_item_detail_urls() {
        let mut url = url::Url::parse("http://localhost/Users/u1/Items/abc").unwrap();
        assert!(swap_last_path_segment(&mut url, "def"));
        assert_eq!(url.path(), "/Users/u1/Items/def");

        let with_query = url::Url::parse("http://localhost/Items/abc?Fields=Overview").unwrap();
        let mut with_query = with_query;
        assert!(swap_last_path_segment(&mut with_query, "def"));
        assert_eq!(with_query.path(), "/Items/def");
        assert!(with_query
            .query_pairs()
            .any(|(key, value)| key == "Fields" && value == "Overview"));
    }

    #[test]
    fn query_field_is_appended_without_duplicating() {
        let mut url = url::Url::parse("http://localhost/Items/abc?Fields=Overview").unwrap();
        ensure_query_field(&mut url, "Fields", "MediaSources");
        ensure_query_field(&mut url, "Fields", "MediaSources");

        let fields = url
            .query_pairs()
            .find(|(key, _)| key.eq_ignore_ascii_case("Fields"))
            .map(|(_, value)| value.into_owned())
            .unwrap();
        assert_eq!(fields, "Overview,MediaSources");

        let mut missing = url::Url::parse("http://localhost/Items/abc").unwrap();
        ensure_query_field(&mut missing, "Fields", "MediaSources");
        assert_eq!(
            missing
                .query_pairs()
                .find(|(key, _)| key.eq_ignore_ascii_case("Fields"))
                .map(|(_, value)| value.into_owned()),
            Some("MediaSources".to_string())
        );
    }
}
