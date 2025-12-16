use axum::{
    body::Body,
    extract::{Request, State},
    http::{HeaderName, StatusCode},
    response::{IntoResponse, Response},
    routing::{any, get, post},
    Router,
};

use axum_messages::MessagesManagerLayer;
use percent_encoding::percent_decode_str;
use rust_embed::RustEmbed;
use sqlx::{sqlite::SqliteConnectOptions, SqlitePool};
use std::{net::SocketAddr, str::FromStr};
use std::{sync::Arc, time::Duration};
use tokio::task::AbortHandle;
use tower::ServiceBuilder;
use tower_http::{cors::CorsLayer, trace::TraceLayer};
use tower_sessions::cookie::Key;
use tower_sessions_sqlx_store::SqliteStore;
use tracing::{debug, error, info, trace, warn};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

use axum_login::{
    tower_sessions::{ExpiredDeletion, Expiry, SessionManagerLayer},
    AuthManagerLayerBuilder,
};

mod config;
mod encryption;
mod federated_users;
mod handlers;
mod media_storage_service;
mod models;
mod processors;
mod request_preprocessing;
mod server_storage;
mod session_storage;
mod ui;
mod url_helper;
mod user_authorization_service;

use federated_users::FederatedUserService;
use media_storage_service::MediaStorageService;
use server_storage::ServerStorageService;
use user_authorization_service::UserAuthorizationService;

use crate::{
    config::DATA_DIR, encryption::Password, request_preprocessing::preprocess_request,
    session_storage::SessionStorage, ui::ui_routes,
};
use crate::{
    config::{AppConfig, MIGRATOR},
    processors::{
        request_analyzer::RequestAnalyzer,
        request_processor::{RequestProcessingContext, RequestProcessor},
    },
    request_preprocessing::body_to_json,
    ui::Backend,
};

#[derive(Clone)]
pub struct AppState {
    pub reqwest_client: reqwest::Client,
    pub user_authorization: Arc<UserAuthorizationService>,
    pub server_storage: Arc<ServerStorageService>,
    pub media_storage: Arc<MediaStorageService>,
    pub play_sessions: Arc<SessionStorage>,
    pub config: Arc<tokio::sync::RwLock<AppConfig>>,
    pub processors: Arc<JsonProcessors>,
    pub federated_users: Arc<FederatedUserService>,
}

impl AppState {
    pub fn new(
        reqwest_client: reqwest::Client,
        data_context: DataContext,
        json_processors: JsonProcessors,
    ) -> Self {
        // Create temporary state to initialize FederatedUserService
        // This is a bit circular but FederatedUserService needs parts of AppState
        // We can construct it manually here since we have all components
        let federated_users = Arc::new(FederatedUserService::new_from_components(
            data_context.server_storage.clone(),
            data_context.user_authorization.clone(),
            data_context.config.clone(),
        ));

        Self {
            reqwest_client,
            user_authorization: data_context.user_authorization,
            server_storage: data_context.server_storage,
            media_storage: data_context.media_storage,
            play_sessions: data_context.play_sessions,
            config: data_context.config,
            processors: Arc::new(json_processors),
            federated_users,
        }
    }

    pub async fn get_ui_route(&self) -> String {
        let config = self.config.read().await;
        if let Some(prefix) = &config.url_prefix {
            format!("{}/{}", prefix, config.ui_route)
        } else {
            config.ui_route.to_string()
        }
    }

    pub async fn get_url_prefix(&self) -> Option<String> {
        let config = self.config.read().await;
        config.url_prefix.as_ref().map(|prefix| prefix.to_string())
    }

    pub async fn get_admin_password(&self) -> Password {
        let config = self.config.read().await;
        config.password.clone()
    }

    pub async fn can_change_item_names(&self) -> bool {
        let config = self.config.read().await;
        config.include_server_name_in_media
    }

    pub async fn remove_prefix_from_path<'a>(&self, path: &'a str) -> &'a str {
        let config = self.config.read().await;
        if let Some(prefix) = &config.url_prefix {
            path.trim_start_matches(&format!("/{}", prefix))
        } else {
            path
        }
    }
}

#[derive(Clone)]
/// Struct holding shared services and configuration
pub struct DataContext {
    pub user_authorization: Arc<UserAuthorizationService>,
    pub server_storage: Arc<ServerStorageService>,
    pub media_storage: Arc<MediaStorageService>,
    pub play_sessions: Arc<SessionStorage>,
    pub config: Arc<tokio::sync::RwLock<AppConfig>>,
}

pub struct JsonProcessors {
    pub request_processor: RequestProcessor,
    pub request_analyzer: RequestAnalyzer,
}

/// These traits on axum::Router allow simple additional matching on lower-case
/// endpoints that some legacy clients use. (I'm looking at you, Roku build)
/// This could be changed to issue HTTP redirects, or additional logging
/// use `--features legacy-lowercase` to enable
use axum::routing::MethodRouter;

#[cfg(feature = "legacy-lowercase")]
fn lower_case_path_preserving_params(path: &str) -> String {
    let mut result = String::with_capacity(path.len());
    let mut in_braces = false;

    for c in path.chars() {
        match c {
            '{' => {
                in_braces = true;
                result.push(c);
            }
            '}' => {
                in_braces = false;
                result.push(c);
            }
            _ => {
                if in_braces {
                    result.push(c); // keep parameter as-is
                } else {
                    result.extend(c.to_lowercase());
                }
            }
        }
    }

    result
}

pub trait RouterLegacyExt<S>
where
    S: Clone + Send + Sync + 'static,
{
    /// Register a canonical route and a lowercase legacy variant.
    fn route_legacy(self, path: &str, mr: MethodRouter<S>) -> Router<S>;
    /// Nest a router under both canonical and lowercase prefixes.
    fn nest_legacy(self, path: &str, router: Router<S>) -> Router<S>;
    /// Merge another router and also merge its lowercase-legacy form
    /// which does nothing special, it's just for semantic completeness
    fn merge_legacy(self, router: Router<S>) -> Router<S>;
}

#[cfg(not(feature = "legacy-lowercase"))]
impl<S> RouterLegacyExt<S> for axum::Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    fn route_legacy(self, path: &str, mr: MethodRouter<S>) -> Router<S> {
        self.route(path, mr)
    }

    fn nest_legacy(self, path: &str, router: Router<S>) -> Router<S> {
        self.nest(path, router)
    }

    fn merge_legacy(self, router: Router<S>) -> Router<S> {
        self.merge(router)
    }
}

#[cfg(feature = "legacy-lowercase")]
impl<S> RouterLegacyExt<S> for Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    fn route_legacy(self, path: &str, mr: MethodRouter<S>) -> Router<S> {
        // canonical route
        let router = self.route(path, mr.clone());

        // lowercase legacy route
        let legacy_path = lower_case_path_preserving_params(path);
        if legacy_path != path {
            router.route(&legacy_path, mr)
        } else {
            router
        }
    }

    fn nest_legacy(self, path: &str, router: Router<S>) -> Router<S> {
        // register canonical nest
        let outer = self.nest(path, router.clone());

        // register lowercase legacy nest if different
        let legacy_path = lower_case_path_preserving_params(path);
        if legacy_path != path {
            outer.nest(&legacy_path, router)
        } else {
            outer
        }
    }

    fn merge_legacy(self, router: Router<S>) -> Router<S> {
        self.merge(router)
    }
}

#[derive(RustEmbed)]
#[folder = "static/"]
struct Asset;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    use crate::RouterLegacyExt;

    // Initialize file logging

    let file_appender = tracing_appender::rolling::daily(DATA_DIR.join("logs"), "jellyswarm.log");
    let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);

    // Create an environment filter with configurable log level
    // Defaults to "jellyswarrm_proxy=info" but can be overridden with RUST_LOG env var
    // Examples:
    //   RUST_LOG=debug                           - Enable debug for all modules
    //   RUST_LOG=jellyswarrm_proxy=debug         - Enable debug for this app only
    //   RUST_LOG=jellyswarrm_proxy=trace,tower=info - Debug this app, info for tower
    let default_filter = "jellyswarrm_proxy=info";
    let env_filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_filter));

    tracing_subscriber::registry()
        .with(env_filter)
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(non_blocking)
                .with_ansi(false),
        )
        .with(tracing_subscriber::fmt::layer().with_writer(std::io::stdout))
        .init();

    let loaded_config = crate::config::load_config();
    info!("Loaded configuration: {:?}", loaded_config);

    // Resolve database path inside DATA_DIR
    let db_path = DATA_DIR.join("jellyswarrm.db");
    let db_url = format!("sqlite://{}", db_path.to_string_lossy());
    let options = SqliteConnectOptions::from_str(&db_url)?.create_if_missing(true);

    let pool = SqlitePool::connect_with(options).await?;

    MIGRATOR.run(&pool).await.unwrap_or_else(|e| {
        error!("Failed to run database migrations: {}", e);
        std::process::exit(1);
    });

    sqlx::query("PRAGMA foreign_keys = ON;")
        .execute(&pool)
        .await?;

    // Create reqwest client
    let reqwest_client = reqwest::Client::builder()
        .timeout(Duration::from_secs(loaded_config.timeout))
        .build()
        .unwrap_or_else(|e| {
            error!("Failed to create reqwest client: {}", e);
            std::process::exit(1);
        });

    // Initialize user authorization service
    let user_authorization = UserAuthorizationService::new(pool.clone());

    // Initialize server storage service
    let server_storage = ServerStorageService::new(pool.clone());

    // Initialize media storage service
    let media_storage = MediaStorageService::new(pool.clone());

    if !loaded_config.preconfigured_servers.is_empty() {
        info!(
            "Adding {} preconfigured servers from config",
            loaded_config.preconfigured_servers.len()
        );
        for server in &loaded_config.preconfigured_servers {
            match server_storage
                .add_server(&server.name, &server.url, server.priority)
                .await
            {
                Ok(_) => {
                    info!(
                        "  Added preconfigured server: {} ({}) with priority {}",
                        server.name, server.url, server.priority
                    );
                }
                Err(e) => {
                    error!(
                        "  Failed to add preconfigured server {} ({}): {}",
                        server.name, server.url, e
                    );
                }
            }
        }
    }

    match server_storage.list_servers().await {
        Ok(servers) => {
            if servers.is_empty() {
                warn!("No servers found, configure them via the UI.");
            } else {
                info!("Found {} configured servers", servers.len());
                for server in &servers {
                    info!(
                        "  {} ({}): priority {}",
                        server.name, server.url, server.priority,
                    );
                }
            }
        }
        Err(e) => {
            error!("Failed to check existing servers: {}", e);
        }
    }

    let data_context = DataContext {
        user_authorization: Arc::new(user_authorization.clone()),
        server_storage: Arc::new(server_storage.clone()),
        media_storage: Arc::new(media_storage.clone()),
        play_sessions: Arc::new(SessionStorage::new()),
        config: Arc::new(tokio::sync::RwLock::new(loaded_config.clone())),
    };

    let json_processors = JsonProcessors {
        request_processor: RequestProcessor::new(data_context.clone()),
        request_analyzer: RequestAnalyzer::new(data_context.clone()),
    };

    let app_state = AppState::new(reqwest_client, data_context, json_processors);

    let session_store = SqliteStore::new(pool);
    session_store.migrate().await?;

    let deletion_task = tokio::task::spawn(
        session_store
            .clone()
            .continuously_delete_expired(tokio::time::Duration::from_secs(60)),
    );

    let key = Key::from(loaded_config.session_key.as_slice());

    let session_layer = SessionManagerLayer::new(session_store)
        .with_secure(false)
        .with_same_site(tower_sessions::cookie::SameSite::Lax)
        .with_expiry(Expiry::OnInactivity(time::Duration::days(1))) // 24 hour
        .with_signed(key);

    let backend = Backend::new(
        app_state.config.clone(),
        app_state.user_authorization.clone(),
    );
    let auth_layer = AuthManagerLayerBuilder::new(backend, session_layer).build();

    let ui_route = loaded_config.ui_route.to_string();

    let app = Router::new()
        // UI Management routes
        .nest(&format!("/{ui_route}"), ui_routes())
        .route("/", get(index_handler))
        .route_legacy(
            "/QuickConnect/Enabled",
            get(handlers::quick_connect::handle_quick_connect),
        )
        .route_legacy(
            "/Branding/Configuration",
            get(handlers::branding::handle_branding),
        )
        // User authentication and profile routes
        .nest_legacy(
            "/Users",
            Router::new()
                .route_legacy(
                    "/AuthenticateByName",
                    post(handlers::users::handle_authenticate_by_name),
                )
                .route_legacy("/Me", get(handlers::users::handle_get_me))
                .route_legacy("/{user_id}", get(handlers::users::handle_get_user_by_id))
                .route_legacy(
                    "/{user_id}/Views",
                    get(handlers::federated::get_items_from_all_servers),
                )
                .route_legacy(
                    "/{user_id}/Items",
                    get(handlers::federated::get_items_from_all_servers_if_not_restricted),
                )
                .route_legacy(
                    "/{user_id}/Items/Resume",
                    get(handlers::federated::get_items_from_all_servers),
                )
                .route_legacy(
                    "/{user_id}/Items/Latest",
                    get(handlers::federated::get_items_from_all_servers_if_not_restricted),
                )
                .route_legacy("/{user_id}/Items/{item_id}", get(handlers::items::get_item))
                .route_legacy(
                    "/{user_id}/Items/{item_id}/SpecialFeatures",
                    get(handlers::items::get_items_list),
                ),
        )
        .route_legacy(
            "/UserViews",
            get(handlers::federated::get_items_from_all_servers),
        )
        // System info routes
        .nest_legacy(
            "/System",
            Router::new()
                .route_legacy("/Info", get(handlers::system::info))
                .route_legacy("/Info/Public", get(handlers::system::info_public)),
        )
        // Item routes (non-user specific)
        .nest_legacy(
            "/Items",
            Router::new()
                .route_legacy(
                    "/",
                    get(handlers::federated::get_items_from_all_servers_if_not_restricted),
                )
                .route_legacy(
                    "/Suggestions",
                    get(handlers::federated::get_items_from_all_servers_if_not_restricted),
                )
                .route_legacy(
                    "/Latest",
                    get(handlers::federated::get_items_from_all_servers_if_not_restricted),
                )
                .route_legacy("/{item_id}", get(handlers::items::get_item))
                .route_legacy("/{item_id}/Similar", get(handlers::items::get_items))
                .route_legacy("/{item_id}/LocalTrailers", get(handlers::items::get_items))
                .route_legacy(
                    "/{item_id}/SpecialFeatures",
                    get(handlers::items::get_items),
                )
                .route_legacy(
                    "/{item_id}/PlaybackInfo",
                    post(handlers::items::post_playback_info),
                ),
        )
        .route_legacy("/MediaSegments/{item_id}", get(handlers::items::get_items))
        // Show-specific routes
        .nest_legacy(
            "/Shows",
            Router::new()
                .route_legacy("/{item_id}/Seasons", get(handlers::items::get_items))
                .route_legacy("/{item_id}/Episodes", get(handlers::items::get_items))
                .route_legacy(
                    "/NextUp",
                    get(handlers::federated::get_items_from_all_servers_if_not_restricted),
                ),
        )
        .nest_legacy(
            "/LiveTv",
            Router::new()
                .route_legacy(
                    "/Channels",
                    get(handlers::federated::get_items_from_all_servers),
                )
                .route_legacy("/Channels/{item_id}", get(handlers::items::get_item))
                .route_legacy(
                    "/Programs",
                    get(handlers::federated::get_items_from_all_servers),
                )
                .route_legacy(
                    "/Programs/Recommended",
                    get(handlers::federated::get_items_from_all_servers),
                )
                .route_legacy("/Programs/{item_id}", get(handlers::items::get_item))
                .route_legacy(
                    "/Recordings",
                    get(handlers::federated::get_items_from_all_servers),
                )
                .route_legacy(
                    "/Recordings/Folders",
                    get(handlers::federated::get_items_from_all_servers),
                )
                .route_legacy("/Recordings/{item_id}", get(handlers::items::get_item))
                .route_legacy(
                    "/LiveRecordings/{recordingId}/stream",
                    get(handlers::videos::get_stream),
                )
                .route_legacy(
                    "/LiveStreamFiles/{streamId}/stream.{container}",
                    get(handlers::videos::get_stream),
                ),
        )
        // Video streaming routes
        .nest_legacy(
            "/Videos",
            Router::new()
                .route("/{stream_id}/Trickplay/{*path}", get(proxy_handler))
                .route("/{stream_id}/{*path}", get(handlers::videos::get_stream_part))
                .route("/{item_id}/stream", get(handlers::videos::get_stream))
                .route("/{item_id}/stream.mkv", get(handlers::videos::get_stream))
                .route("/{item_id}/stream.mp4", get(handlers::videos::get_stream))
                .route(
                    "/{stream_id}/{item_id}/{*path}",
                    get(handlers::videos::get_video_resource),
                ),
        )
        // Persons
        .nest_legacy(
            "/Persons",
            Router::new().route("/", get(handlers::federated::get_items_from_all_servers)),
        )
        // Artists
        .nest_legacy(
            "/Artists",
            Router::new().route("/", get(handlers::federated::get_items_from_all_servers)),
        )
        .route("/{*path}", any(proxy_handler))
        .fallback(proxy_handler)
        .layer(
            ServiceBuilder::new()
                .layer(TraceLayer::new_for_http())
                .layer(CorsLayer::permissive()),
        )
        .layer(MessagesManagerLayer)
        .layer(auth_layer)
        .with_state(app_state);

    // Create socket address
    let addr = match format!("{}:{}", loaded_config.host, loaded_config.port).parse::<SocketAddr>()
    {
        Ok(addr) => addr,
        Err(e) => {
            error!(
                "Invalid address {}:{}: {}",
                loaded_config.host, loaded_config.port, e
            );
            std::process::exit(1);
        }
    };

    let app = if let Some(url_prefix) = loaded_config.url_prefix {
        let url_prefix = url_prefix.to_string();
        info!("Using URL prefix: {}", url_prefix);

        info!("Starting reverse proxy on http://{}/{}", addr, url_prefix);
        info!(
            "UI Management routes available at: http://{}/{}/{}",
            addr,
            url_prefix,
            ui_route.trim_start_matches('/')
        );

        Router::new()
            .nest(&format!("/{}", url_prefix), app)
            .fallback(
                // Redirect any request outside the prefixed subtree into the prefixed route,
                // preserving the original path. e.g. /foo/bar -> /{url_prefix}/foo/bar
                // capture url_prefix by value
                {
                    let prefix = url_prefix.clone();
                    move |req: Request| {
                        let prefix = prefix.clone();
                        async move {
                            let orig = req.uri().path().trim_end_matches("/");
                            let prefix_slash = format!("/{}", prefix);
                            let target = if orig.starts_with(&prefix_slash) {
                                // already has prefix - avoid double-appending
                                orig
                            } else {
                                &format!("{prefix_slash}{orig}")
                            };
                            axum::response::Redirect::temporary(target).into_response()
                        }
                    }
                },
            )
    } else {
        info!("No URL prefix configured, using root path");
        info!("Starting reverse proxy on http://{}", addr);
        info!(
            "UI Management routes available at: http://{}/{}",
            addr, ui_route
        );
        app
    };

    // Start the server
    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(listener) => listener,
        Err(e) => {
            error!("Failed to bind to {}: {}", addr, e);
            std::process::exit(1);
        }
    };

    axum::serve(listener, app.into_make_service())
        .with_graceful_shutdown(shutdown_signal(deletion_task.abort_handle()))
        .await?;

    deletion_task.await??;
    Ok(())
}

async fn index_handler(
    State(state): State<AppState>,
    _req: Request,
) -> Result<Response<Body>, StatusCode> {
    let servers = state.server_storage.list_servers().await.map_err(|e| {
        error!("Failed to list servers: {}", e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    if servers.is_empty() {
        // No servers configured, redirect to UI management
        Ok(Response::builder()
            .status(StatusCode::TEMPORARY_REDIRECT)
            .header("Location", "/ui")
            .body(Body::empty())
            .unwrap())
    } else {
        // Servers exist, return the index.html page
        if let Some(content) = Asset::get("index.html") {
            Ok(Response::builder()
                .header("Content-Type", "text/html")
                .body(Body::from(content.data.into_owned()))
                .unwrap())
        } else {
            // Fallback if index.html is not found in assets
            error!("index.html not found in static assets");
            Err(StatusCode::NOT_FOUND)
        }
    }
}

#[axum::debug_handler]
async fn proxy_handler(
    State(state): State<AppState>,
    req: Request,
) -> Result<Response<Body>, StatusCode> {
    // check if a resource was requested
    let path = req.uri().path();
    debug!("Using generic processing for path: {}", path);
    let path = if let Some(path) = path.strip_prefix('/') {
        path
    } else {
        path
    };
    let path = if path.is_empty() { "index.html" } else { path };
    let decoded_path = percent_decode_str(path).decode_utf8_lossy().to_string();
    if let Some(content) = Asset::get(&decoded_path) {
        let mime = mime_guess::from_path(decoded_path).first_or_octet_stream();
        return Ok(Response::builder()
            .header("Content-Type", mime.as_ref())
            .body(Body::from(content.data.into_owned()))
            .unwrap());
    }

    let preprocessed = preprocess_request(req, &state).await.map_err(|e| {
        error!("Failed to preprocess request: {}", e);
        StatusCode::BAD_REQUEST
    })?;

    let request_url = preprocessed.request.url().clone();
    trace!(
        "Proxy request details:\n  Original: {:?}\n  Target URL: {}\n  Transformed: {:?}",
        preprocessed.original_request,
        preprocessed.request.url(),
        preprocessed.request
    );

    let payload_processing_context = RequestProcessingContext::new(&preprocessed);
    let mut request = preprocessed.request;

    let preprocessor = &state.processors.request_processor;
    if let Some(mut json_value) = body_to_json(&request) {
        let response =
            processors::process_json(&mut json_value, preprocessor, &payload_processing_context)
                .await
                .map_err(|e| {
                    error!("Failed to process JSON body: {}", e);
                    StatusCode::BAD_REQUEST
                })?;
        if response.was_modified {
            debug!("Modified JSON body for request to {}", request_url);
            let new_body = serde_json::to_vec(&response.data).map_err(|e| {
                error!("Failed to serialize processed JSON body: {}", e);
                StatusCode::INTERNAL_SERVER_ERROR
            })?;
            *request.body_mut() = Some(reqwest::Body::from(new_body.clone()));
            // Update Content-Length header
            request.headers_mut().insert(
                reqwest::header::CONTENT_LENGTH,
                reqwest::header::HeaderValue::from_str(&new_body.len().to_string()).unwrap(),
            );
        }
    }
    let response = state.reqwest_client.execute(request).await.map_err(|e| {
        error!("Failed to execute proxy request: {}", e);
        StatusCode::BAD_GATEWAY
    })?;

    let status = response.status();
    if !status.is_success() {
        warn!(
            "Upstream server returned error status: {} for Request to: {}",
            status, request_url
        );
    }
    let headers = response.headers().clone();
    let body_bytes = response.bytes().await.map_err(|e| {
        error!("Failed to read response body: {}", e);
        StatusCode::BAD_GATEWAY
    })?;

    let mut response_builder = Response::builder().status(status);

    // Copy headers, filtering out hop-by-hop headers
    for (name, value) in headers.iter() {
        if !is_hop_by_hop_header(name) {
            response_builder = response_builder.header(name, value);
        }
    }

    let response = response_builder.body(Body::from(body_bytes)).map_err(|e| {
        error!("Failed to build response: {}", e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    Ok(response)
}

fn is_hop_by_hop_header(name: &HeaderName) -> bool {
    // RFC 7230 Section 6.1: Hop-by-hop headers
    matches!(
        name.as_str().to_lowercase().as_str(),
        "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailers"
            | "transfer-encoding"
            | "upgrade"
    )
}

async fn shutdown_signal(deletion_task_abort_handle: AbortHandle) {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install signal handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => { deletion_task_abort_handle.abort() },
        _ = terminate => { deletion_task_abort_handle.abort() },
    }
}
