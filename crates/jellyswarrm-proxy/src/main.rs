use axum::{
    body::Body,
    extract::{Request, State},
    http::{HeaderName, StatusCode},
    response::Response,
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
use tracing::{debug, error, info, warn};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

use axum_login::{
    tower_sessions::{ExpiredDeletion, Expiry, SessionManagerLayer},
    AuthManagerLayerBuilder,
};

mod config;
mod handlers;
mod media_storage_service;
mod models;
mod request_preprocessing;
mod server_storage;
mod session_storage;
mod ui;
mod url_helper;
mod user_authorization_service;

use media_storage_service::MediaStorageService;
use server_storage::ServerStorageService;
use user_authorization_service::UserAuthorizationService;

use crate::{
    config::AppConfig,
    ui::{resource_handler, Backend},
};
use crate::{
    config::DATA_DIR, request_preprocessing::preprocess_request, session_storage::SessionStorage,
    ui::ui_routes,
};

#[derive(Clone)]
pub struct AppState {
    pub reqwest_client: reqwest::Client,
    pub user_authorization: Arc<UserAuthorizationService>,
    pub server_storage: Arc<ServerStorageService>,
    pub media_storage: Arc<MediaStorageService>,
    pub play_sessions: Arc<SessionStorage>,
    pub config: Arc<tokio::sync::RwLock<AppConfig>>,
}

#[derive(RustEmbed)]
#[folder = "static/"]
struct Asset;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
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

    // Create reqwest client
    let reqwest_client = reqwest::Client::builder()
        .timeout(Duration::from_secs(loaded_config.timeout))
        .build()
        .unwrap_or_else(|e| {
            error!("Failed to create reqwest client: {}", e);
            std::process::exit(1);
        });

    // Initialize user authorization service
    let user_authorization = UserAuthorizationService::new(pool.clone())
        .await
        .unwrap_or_else(|e| {
            error!("Failed to initialize user authorization service: {}", e);
            std::process::exit(1);
        });

    // Initialize server storage service
    let server_storage = ServerStorageService::new(pool.clone())
        .await
        .unwrap_or_else(|e| {
            error!("Failed to initialize server storage database: {}", e);
            std::process::exit(1);
        });

    // Initialize media storage service
    let media_storage = MediaStorageService::new(pool.clone())
        .await
        .unwrap_or_else(|e| {
            error!("Failed to initialize media storage service: {}", e);
            std::process::exit(1);
        });

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

    let app_state = AppState {
        reqwest_client,
        user_authorization: Arc::new(user_authorization),
        server_storage: Arc::new(server_storage),
        media_storage: Arc::new(media_storage),
        play_sessions: Arc::new(SessionStorage::new()),
        config: Arc::new(tokio::sync::RwLock::new(loaded_config.clone())),
    };

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
        .with_expiry(Expiry::OnInactivity(time::Duration::days(1))) // 24 hour
        .with_signed(key);

    let backend = Backend::new(app_state.config.clone());
    let auth_layer = AuthManagerLayerBuilder::new(backend, session_layer).build();

    let ui_route = "/ui";

    let app = Router::new()
        // UI Management routes
        .nest(ui_route, ui_routes())
        .route("/", get(index_handler))
        .route("/resources/{*path}", get(resource_handler))
        .route(
            "/QuickConnect/Enabled",
            get(handlers::quick_connect::handle_quick_connect),
        )
        .route(
            "/Branding/Configuration",
            get(handlers::branding::handle_branding),
        )
        // User authentication and profile routes
        .nest(
            "/Users",
            Router::new()
                .route(
                    "/authenticatebyname",
                    post(handlers::users::handle_authenticate_by_name),
                )
                .route(
                    "/AuthenticateByName",
                    post(handlers::users::handle_authenticate_by_name),
                )
                .route("/{user_id}", get(handlers::users::handle_get_user_by_id))
                .route("/{user_id}/Items", get(handlers::items::get_items))
                .route(
                    "/{user_id}/Items/Resume",
                    get(handlers::federated::get_items_from_all_servers),
                )
                .route(
                    "/{user_id}/Items/Latest",
                    get(handlers::items::get_items_list),
                )
                .route("/{user_id}/Items/{item_id}", get(handlers::items::get_item))
                .route(
                    "/{user_id}/Items/{item_id}/SpecialFeatures",
                    get(handlers::items::get_items_list),
                ),
        )
        .route(
            "/UserViews",
            get(handlers::federated::get_items_from_all_servers),
        )
        // System info routes
        .nest(
            "/System",
            Router::new()
                .route("/Info", get(handlers::system::info))
                .route("/Info/Public", get(handlers::system::info_public)),
        )
        .route("/system/info/public", get(handlers::system::info_public))
        // Item routes (non-user specific)
        .nest(
            "/Items",
            Router::new()
                .route("/", get(handlers::federated::get_items_from_all_servers))
                .route("/{item_id}", get(handlers::items::get_item))
                .route("/{item_id}/Similar", get(handlers::items::get_items))
                .route(
                    "/{item_id}/PlaybackInfo",
                    post(handlers::items::post_playback_info),
                ),
        )
        .route("/MediaSegments/{item_id}", get(handlers::items::get_items))
        // Show-specific routes
        .nest(
            "/Shows",
            Router::new()
                .route("/{item_id}/Seasons", get(handlers::items::get_items))
                .route("/{item_id}/Episodes", get(handlers::items::get_items))
                .route(
                    "/NextUp",
                    get(handlers::federated::get_items_from_all_servers_if_not_restricted),
                ),
        )
        .route("/LiveTv/Programs", get(handlers::items::get_items))
        // Video streaming routes
        .nest(
            "/Videos",
            Router::new()
                .route("/{stream_id}/Trickplay/{*path}", get(proxy_handler))
                .route("/{item_id}/stream.mkv", get(handlers::videos::get_mkv))
                .route("/{item_id}/stream.mp4", get(handlers::videos::get_mkv))
                .route(
                    "/{stream_id}/{item_id}/{*path}",
                    get(handlers::videos::get_video_resource),
                ),
        )
        .route(
            "/videos/{stream_id}/{*path}",
            get(handlers::videos::get_stream_part),
        )
        // Session management routes
        .nest(
            "/Sessions/Playing",
            Router::new()
                .route("/", post(handlers::sessions::post_playing))
                .route("/Progress", post(handlers::sessions::post_playing))
                .route("/Stopped", post(handlers::sessions::post_playing)),
        )
        // Persons
        .nest(
            "/Persons",
            Router::new().route("/", get(handlers::federated::get_items_from_all_servers)),
        )
        // Artists
        .nest(
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

    info!("Starting reverse proxy on http://{}", addr);
    info!(
        "UI Management routes available at: http://{}/{}",
        addr,
        ui_route.trim_start_matches('/')
    );

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

    debug!(
        "Proxy request details:\n  Original: {:?}\n  Target URL: {}\n  Transformed: {:?}",
        preprocessed.original_request,
        preprocessed.request.url(),
        preprocessed.request
    );

    let response = state
        .reqwest_client
        .execute(preprocessed.request)
        .await
        .map_err(|e| {
            error!("Failed to execute proxy request: {}", e);
            StatusCode::BAD_GATEWAY
        })?;

    let status = response.status();
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
