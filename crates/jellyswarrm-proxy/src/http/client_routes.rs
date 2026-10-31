//! Client library/playback API routes and their adapters, shared by production and
//! HTTP contract tests. Each handler extracts caller input, calls one application
//! workflow, and renders the result; status policy lives in `http::error`.
use super::{
    extractors::{CatalogInput, LiveInput, OperationInput},
    fallback::fallback_handler,
    quick_connect,
    state::AppState,
};
use crate::{
    application::{
        accounts::{self, AuthenticationService},
        catalog, details,
        error::ApplicationError,
        playback::{
            live::{self, LiveStreamActionResult},
            negotiation, resources,
        },
        services::MediaServices,
    },
    http::assets::JELLYFIN_UI_VERSION,
    models::{AuthenticateRequest, PlaybackResponse, PublicServerInfo, ServerInfo, User},
    upstream::delivery::{DeliveryBody, DeliveryResponse},
};
use axum::{
    body::{to_bytes, Body},
    extract::{OriginalUri, Path, Request, State},
    handler::Handler,
    response::{IntoResponse, Response},
    routing::{get, post, MethodRouter},
    Json, Router,
};
use hyper::{HeaderMap, StatusCode};
use serde_json::Value;

#[cfg(test)]
#[path = "../unit_tests/item_handlers.rs"]
mod tests;

pub(crate) fn router() -> Router<AppState> {
    jellyswarrm_macros::lowercase_routes! {
        Router::new()
            .nest("/Users", Router::new()
                .route("/AuthenticateByName", post(authenticate_by_name))
                .route("/AuthenticateWithQuickConnect", post(quick_connect::authenticate))
                .route("/Public", get(public_users))
                .route("/Me", get(me))
                .route("/{user_id}", get(user_by_id))
                .route("/{user_id}/Views", get(all_servers))
                .route("/{user_id}/Items", get(unless_restricted))
                .route("/{user_id}/Items/Resume", get(all_servers))
                .route("/{user_id}/Items/Latest", get(unless_restricted))
                .route("/{user_id}/Items/{item_id}", get(item))
                .route("/{user_id}/Items/{item_id}/SpecialFeatures", get(item_list)))
            .route("/UserViews", get(all_servers))
            .route("/Library/MediaFolders", get(media_folders))
            .route("/UserItems/Resume", get(all_servers))
            .nest("/System", Router::new()
                .route("/Info", get(server_info))
                .route("/Info/Public", get(public_server_info)))
            .nest("/Items", Router::new()
                .route("/", get(unless_restricted))
                .route("/Suggestions", get(unless_restricted))
                .route("/Latest", get(unless_restricted))
                .route("/{item_id}", item_detail_routes())
                .route("/{item_id}/Download", get(download))
                .route("/Filters", get(filters))
                .route("/Filters2", get(filters))
                .route("/{item_id}/Similar", get(item_list))
                .route("/{item_id}/LocalTrailers", get(item_list))
                .route("/{item_id}/SpecialFeatures", get(item_list))
                .route("/{item_id}/PlaybackInfo", post(playback_info)))
            .route("/MediaSegments/{item_id}", get(item_list))
            .nest("/Shows", Router::new()
                .route("/{item_id}/Seasons", get(show_children))
                .route("/{item_id}/Episodes", get(show_children))
                .route("/NextUp", get(unless_restricted)))
            .nest("/LiveTv", Router::new()
                .route("/Channels", get(all_servers))
                .route("/Channels/{item_id}", get(item))
                .route("/Programs", get(live_tv_programs).post(post_live_tv_programs))
                .route("/Programs/Recommended", get(all_servers))
                .route("/Programs/{item_id}", get(item))
                .route("/Recordings", get(all_servers))
                .route("/Recordings/Folders", get(all_servers))
                .route("/Recordings/{item_id}", get(item))
                .route("/LiveRecordings/{recordingId}/stream", get(stream))
                .route("/LiveStreamFiles/{streamId}/stream.{container}", get(stream)))
            .route("/LiveStreams/Open", post(live_stream_open))
            .route("/LiveStreams/Close", post(live_stream_action))
            .route("/LiveStreams/MediaInfo", post(live_stream_action))
            .route("/Genres", get(all_servers))
            .route("/Studios", get(all_servers))
            .nest("/Videos", Router::new()
                .route("/{item_id}/AdditionalParts", get(item_list))
                .route("/{stream_id}/Trickplay/{*path}", get(fallback_handler))
                .route("/{item_id}/stream", get(stream))
                .route("/{item_id}/stream.mkv", get(stream))
                .route("/{item_id}/stream.mp4", get(stream))
                .route("/{item_id}/stream.mov", get(stream))
                .route("/{stream_id}/{*path}", get(video_resource)))
            .nest("/Audio", Router::new()
                .route("/{item_id}/universal", get(stream))
                .route("/{item_id}/stream", get(stream))
                .route("/{item_id}/stream.{container}", get(stream))
                .route("/{item_id}/master.m3u8", get(video_resource))
                .route("/{item_id}/main.m3u8", get(video_resource))
                .route("/{item_id}/live.m3u8", get(video_resource))
                .route("/{item_id}/hls1/{*path}", get(video_resource)))
            .nest("/Persons", Router::new().route("/", get(all_servers)))
            .nest("/Artists", Router::new().route("/", get(all_servers)))
    }
}

// Shared with HTTP integration tests so mutations exercise the production route.
pub(crate) fn item_detail_routes() -> MethodRouter<AppState> {
    get(item).post(fallback_handler).delete(fallback_handler)
}

type JsonResult<T = Value> = Result<Json<T>, ApplicationError>;

// --- Users and system -------------------------------------------------------

async fn authenticate_by_name(
    State(state): State<AuthenticationService>,
    headers: HeaderMap,
    Json(payload): Json<AuthenticateRequest>,
) -> Result<crate::http::sessions::AuthenticationResponse, ApplicationError> {
    accounts::authenticate_by_name(&state, headers, payload)
        .await
        .map(crate::http::sessions::authentication_response)
}

async fn public_users() -> Json<Vec<User>> {
    Json(accounts::public_users())
}

async fn me(
    State(state): State<MediaServices>,
    OperationInput(input): OperationInput,
) -> JsonResult<User> {
    accounts::get_me(&state, input).await.map(Json)
}

async fn user_by_id(
    State(state): State<MediaServices>,
    Path(_user_id): Path<String>,
    OperationInput(input): OperationInput,
) -> JsonResult<User> {
    accounts::get_user_by_id(&state, input).await.map(Json)
}

fn ui_version() -> String {
    JELLYFIN_UI_VERSION.clone().unwrap_or_default().version
}

async fn public_server_info(State(state): State<AuthenticationService>) -> Json<PublicServerInfo> {
    Json(accounts::public_server_info(&state, &ui_version()).await)
}

async fn server_info(
    State(state): State<MediaServices>,
    OperationInput(input): OperationInput,
) -> JsonResult<ServerInfo> {
    accounts::server_info(&state, input, &ui_version())
        .await
        .map(Json)
}

// --- Federated catalog ------------------------------------------------------

async fn all_servers(
    State(state): State<MediaServices>,
    CatalogInput(request): CatalogInput,
) -> JsonResult {
    catalog::get_catalog(&state, request).await.map(Json)
}

pub(crate) async fn unless_restricted(
    State(state): State<MediaServices>,
    CatalogInput(request): CatalogInput,
) -> JsonResult {
    catalog::get_items_from_all_servers_if_not_restricted(&state, request)
        .await
        .map(Json)
}

async fn show_children(
    State(state): State<MediaServices>,
    CatalogInput(request): CatalogInput,
) -> JsonResult {
    catalog::get_show_children(&state, request).await.map(Json)
}

async fn media_folders(
    State(state): State<MediaServices>,
    CatalogInput(request): CatalogInput,
) -> JsonResult {
    catalog::get_media_folders(&state, request).await.map(Json)
}

async fn filters(
    State(state): State<MediaServices>,
    CatalogInput(request): CatalogInput,
) -> JsonResult {
    catalog::get_filters(&state, request).await.map(Json)
}

async fn live_tv_programs(
    State(state): State<MediaServices>,
    CatalogInput(request): CatalogInput,
) -> JsonResult {
    catalog::get_live_tv_programs(&state, request)
        .await
        .map(Json)
}

/// Treat the legacy guide DTO and modern query form as the same catalog policy,
/// while preserving POST and its typed body on the upstream wire.
async fn post_live_tv_programs(State(state): State<MediaServices>, request: Request) -> Response {
    match live_tv_body_as_query(request).await {
        Ok(request) => Handler::call(live_tv_programs, request, state).await,
        Err(status) => status.into_response(),
    }
}

async fn live_tv_body_as_query(request: Request) -> Result<Request, StatusCode> {
    let (mut parts, body) = request.into_parts();
    let bytes = to_bytes(body, 2 * 1024 * 1024)
        .await
        .map_err(|_| StatusCode::PAYLOAD_TOO_LARGE)?;
    let payload: Value = serde_json::from_slice(&bytes).map_err(|_| StatusCode::BAD_REQUEST)?;
    let fields = payload.as_object().ok_or(StatusCode::BAD_REQUEST)?;
    let original_uri = &parts
        .extensions
        .get::<OriginalUri>()
        .ok_or(StatusCode::BAD_REQUEST)?
        .0;
    let mut url = url::Url::parse(&format!("http://localhost{original_uri}"))
        .map_err(|_| StatusCode::BAD_REQUEST)?;
    // Body fields are authoritative for the POST DTO. Mirror them before server
    // selection so access checks, ID partitioning and pagination use one policy.
    let mut pairs = url
        .query_pairs()
        .filter(|(key, _)| !fields.keys().any(|field| field.eq_ignore_ascii_case(key)))
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect::<Vec<_>>();
    for (key, value) in fields {
        if value.is_null() {
            continue;
        }
        let text = match value {
            Value::String(text) => text.clone(),
            Value::Bool(_) | Value::Number(_) => value.to_string(),
            Value::Array(values) => values
                .iter()
                .map(|value| value.as_str().ok_or(StatusCode::BAD_REQUEST))
                .collect::<Result<Vec<_>, _>>()?
                .join(","),
            _ => return Err(StatusCode::BAD_REQUEST),
        };
        pairs.push((key.clone(), text));
    }
    url.query_pairs_mut().clear().extend_pairs(pairs);
    parts.uri = url[url::Position::BeforePath..]
        .parse()
        .map_err(|_| StatusCode::BAD_REQUEST)?;
    // OriginalUri normally preserves the pre-nest URI. This request now carries
    // the equivalent query policy, including the configured prefix.
    parts.extensions.insert(OriginalUri(parts.uri.clone()));
    Ok(Request::from_parts(parts, Body::from(bytes)))
}

// --- Item details and playback ----------------------------------------------

/// Item detail with media versions merged into one item.
async fn item(
    State(state): State<MediaServices>,
    OperationInput(input): OperationInput,
) -> JsonResult {
    details::get_item_detail(&state, input, true)
        .await
        .map(Json)
}

/// Item-scoped lists (similar, trailers, special features, …) without version merging.
async fn item_list(
    State(state): State<MediaServices>,
    OperationInput(input): OperationInput,
) -> JsonResult {
    details::get_item_detail(&state, input, false)
        .await
        .map(Json)
}

async fn playback_info(
    State(state): State<MediaServices>,
    OperationInput(input): OperationInput,
) -> JsonResult<PlaybackResponse> {
    negotiation::post_playback_info(&state, input)
        .await
        .map(Json)
}

async fn download(
    State(state): State<MediaServices>,
    OperationInput(input): OperationInput,
) -> Result<DeliveryResponse, ApplicationError> {
    resources::get_download(&state, input).await
}

async fn video_resource(
    State(state): State<MediaServices>,
    OperationInput(input): OperationInput,
) -> Result<DeliveryResponse, ApplicationError> {
    resources::get_video_resource(&state, input).await
}

async fn stream(
    State(state): State<MediaServices>,
    OperationInput(input): OperationInput,
) -> Result<DeliveryResponse, ApplicationError> {
    resources::get_stream(&state, input).await
}

async fn live_stream_open(
    State(state): State<MediaServices>,
    LiveInput(input): LiveInput,
) -> JsonResult {
    live::post_livestream_open(&state, input).await.map(Json)
}

async fn live_stream_action(
    State(state): State<MediaServices>,
    LiveInput(input): LiveInput,
) -> Result<LiveStreamActionResult, ApplicationError> {
    live::post_live_stream_action(&state, input).await
}

impl IntoResponse for DeliveryResponse {
    fn into_response(self) -> Response {
        let body = match self.body {
            DeliveryBody::Empty => Body::empty(),
            DeliveryBody::Stream(stream) => Body::from_stream(stream),
            DeliveryBody::Playlist(text) => Body::from(text),
        };
        let mut response = Response::new(body);
        *response.status_mut() = self.status;
        *response.headers_mut() = self.headers;
        response
    }
}

impl IntoResponse for LiveStreamActionResult {
    fn into_response(self) -> Response {
        match self {
            Self::Status(status) => status.into_response(),
            Self::Json(payload) => Json(payload).into_response(),
        }
    }
}
