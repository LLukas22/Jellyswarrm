use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Query, State,
    },
    http::HeaderMap,
    response::Response,
};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message as TungsteniteMessage};
use tracing::{debug, error, info};

use crate::{
    models::Authorization,
    request_preprocessing::JellyfinAuthorization,
    server_storage::Server,
    url_helper::join_server_url,
    user_authorization_service::AuthorizationSession,
    AppState,
};

/// Query parameters for WebSocket connection (Jellyfin sends auth this way)
#[derive(Debug, Deserialize, Default)]
pub struct WebSocketQuery {
    #[serde(rename = "api_key")]
    api_key: Option<String>,
    #[serde(rename = "ApiKey")]
    api_key_alt: Option<String>,
    #[serde(rename = "deviceId")]
    device_id: Option<String>,
}

impl WebSocketQuery {
    pub fn get_api_key(&self) -> Option<&str> {
        self.api_key.as_deref().or(self.api_key_alt.as_deref())
    }
}

/// Result of resolving the backend server for WebSocket
struct ResolvedBackend {
    server: Server,
    /// The real Jellyfin token for this server (not the virtual proxy token)
    real_token: Option<String>,
}

/// Extract authorization from headers (similar to request_preprocessing)
fn extract_auth_from_headers(headers: &HeaderMap) -> Option<JellyfinAuthorization> {
    // Check Authorization header
    if let Some(auth_header) = headers.get("authorization") {
        if let Ok(auth_str) = auth_header.to_str() {
            if let Ok(auth) = Authorization::parse(auth_str) {
                return Some(JellyfinAuthorization::Authorization(auth));
            }
        }
    }

    // Check X-Emby-Authorization header
    if let Some(auth_header) = headers.get("x-emby-authorization") {
        if let Ok(auth_str) = auth_header.to_str() {
            if let Ok(auth) = Authorization::parse(auth_str) {
                return Some(JellyfinAuthorization::XEmbyAuthorization(auth));
            }
        }
    }

    // Check X-MediaBrowser-Token header
    if let Some(token_header) = headers.get("x-mediabrowser-token") {
        if let Ok(token_str) = token_header.to_str() {
            return Some(JellyfinAuthorization::XMediaBrowser(token_str.to_string()));
        }
    }

    // Check X-Emby-Token header
    if let Some(token_header) = headers.get("x-emby-token") {
        if let Ok(token_str) = token_header.to_str() {
            return Some(JellyfinAuthorization::XEmbyToken(token_str.to_string()));
        }
    }

    None
}

/// Resolve which backend server to connect to and get the real token
async fn resolve_backend(
    state: &AppState,
    auth: Option<JellyfinAuthorization>,
) -> Option<ResolvedBackend> {
    // Try to find a server based on the user's authorization
    if let Some(auth) = auth {
        if let Some(token) = auth.token() {
            // Look up user sessions by virtual token
            if let Ok(Some((_user, sessions))) = state
                .user_authorization
                .get_user_sessions_by_virtual_token(&token)
                .await
            {
                // Use the first available session's server (usually the highest priority one)
                if let Some((session, server)) = sessions.into_iter().next() {
                    debug!(
                        "Resolved WebSocket to server {} for user session (real token available)",
                        server.name
                    );
                    return Some(ResolvedBackend {
                        server,
                        real_token: Some(session.jellyfin_token),
                    });
                }
            }
        }
    }

    // Fallback: use the best available server (highest priority, healthy)
    // Note: Without a session, we don't have a real token
    if let Ok(Some(server)) = state.server_storage.get_best_server().await {
        debug!(
            "Using best available server for WebSocket (no session): {}",
            server.name
        );
        return Some(ResolvedBackend {
            server,
            real_token: None,
        });
    }

    // Last resort: get any server
    if let Ok(servers) = state.server_storage.list_servers().await {
        if let Some(server) = servers.into_iter().next() {
            debug!(
                "Using first available server for WebSocket (no session): {}",
                server.name
            );
            return Some(ResolvedBackend {
                server,
                real_token: None,
            });
        }
    }

    None
}

/// Build backend WebSocket URL using the REAL server token
fn build_backend_ws_url(
    server: &Server,
    path: &str,
    real_token: Option<&str>,
    device_id: Option<&str>,
) -> String {
    let mut base_url = server.url.clone();

    // Convert http(s) to ws(s)
    let scheme = match base_url.scheme() {
        "https" => "wss",
        _ => "ws",
    };
    base_url.set_scheme(scheme).ok();

    // Join the path
    let ws_url = join_server_url(&base_url, path);

    // Add query parameters with the REAL token
    let mut url_string = ws_url.to_string();
    let mut params = vec![];

    // Use the real Jellyfin token, not the virtual proxy token
    if let Some(token) = real_token {
        params.push(format!("api_key={}", token));
    }
    if let Some(device_id) = device_id {
        params.push(format!("deviceId={}", device_id));
    }

    if !params.is_empty() {
        if url_string.contains('?') {
            url_string.push('&');
        } else {
            url_string.push('?');
        }
        url_string.push_str(&params.join("&"));
    }

    url_string
}

/// Convert Axum WebSocket message to Tungstenite message
fn axum_to_tungstenite(msg: Message) -> TungsteniteMessage {
    match msg {
        Message::Text(text) => TungsteniteMessage::Text(text.to_string().into()),
        Message::Binary(data) => TungsteniteMessage::Binary(data.to_vec().into()),
        Message::Ping(data) => TungsteniteMessage::Ping(data.to_vec().into()),
        Message::Pong(data) => TungsteniteMessage::Pong(data.to_vec().into()),
        Message::Close(_) => TungsteniteMessage::Close(None),
    }
}

/// Convert Tungstenite message to Axum WebSocket message
fn tungstenite_to_axum(msg: TungsteniteMessage) -> Option<Message> {
    match msg {
        TungsteniteMessage::Text(text) => Some(Message::Text(text.to_string().into())),
        TungsteniteMessage::Binary(data) => Some(Message::Binary(data.to_vec().into())),
        TungsteniteMessage::Ping(data) => Some(Message::Ping(data.to_vec().into())),
        TungsteniteMessage::Pong(data) => Some(Message::Pong(data.to_vec().into())),
        TungsteniteMessage::Close(_) => Some(Message::Close(None)),
        TungsteniteMessage::Frame(_) => None, // Internal frame, skip
    }
}

/// Handle WebSocket upgrade and proxy connection
pub async fn websocket_handler(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<WebSocketQuery>,
) -> Response {
    // Extract authorization from headers or query
    let auth = extract_auth_from_headers(&headers)
        .or_else(|| query.get_api_key().map(|k| JellyfinAuthorization::ApiKey(k.to_string())));

    ws.on_upgrade(move |socket| handle_websocket(socket, state, auth, query))
}

/// Handle the WebSocket connection after upgrade
async fn handle_websocket(
    client_socket: WebSocket,
    state: AppState,
    auth: Option<JellyfinAuthorization>,
    query: WebSocketQuery,
) {
    // Resolve backend server and get the real token
    let resolved = match resolve_backend(&state, auth).await {
        Some(r) => r,
        None => {
            error!("No backend server available for WebSocket connection");
            return;
        }
    };

    let server_name = resolved.server.name.clone();

    // Build backend WebSocket URL with the REAL token
    let backend_url = build_backend_ws_url(
        &resolved.server,
        "/socket",
        resolved.real_token.as_deref(),
        query.device_id.as_deref(),
    );

    info!(
        "Proxying WebSocket to backend: {} (token remapped: {})",
        backend_url.split('?').next().unwrap_or(&backend_url),
        resolved.real_token.is_some()
    );

    // Connect to backend WebSocket
    let backend_connection = match connect_async(&backend_url).await {
        Ok((ws_stream, _response)) => ws_stream,
        Err(e) => {
            error!(
                "Failed to connect to backend WebSocket {}: {}",
                backend_url.split('?').next().unwrap_or(&backend_url),
                e
            );
            return;
        }
    };

    info!("WebSocket connection established to {}", server_name);

    // Split both connections into read/write halves
    let (mut backend_write, mut backend_read) = backend_connection.split();
    let (mut client_write, mut client_read) = client_socket.split();

    // Spawn task to forward messages from client to backend
    let client_to_backend = tokio::spawn(async move {
        while let Some(result) = client_read.next().await {
            match result {
                Ok(msg) => {
                    let backend_msg = axum_to_tungstenite(msg);
                    if let Err(e) = backend_write.send(backend_msg).await {
                        debug!("Error sending to backend: {}", e);
                        break;
                    }
                }
                Err(e) => {
                    debug!("Error reading from client: {}", e);
                    break;
                }
            }
        }
        // Try to close backend connection gracefully
        let _ = backend_write.close().await;
    });

    // Spawn task to forward messages from backend to client
    let backend_to_client = tokio::spawn(async move {
        while let Some(result) = backend_read.next().await {
            match result {
                Ok(msg) => {
                    if let Some(client_msg) = tungstenite_to_axum(msg) {
                        if let Err(e) = client_write.send(client_msg).await {
                            debug!("Error sending to client: {}", e);
                            break;
                        }
                    }
                }
                Err(e) => {
                    debug!("Error reading from backend: {}", e);
                    break;
                }
            }
        }
        // Try to close client connection gracefully
        let _ = client_write.close().await;
    });

    // Wait for either task to complete (connection close or error)
    tokio::select! {
        _ = client_to_backend => {
            debug!("Client to backend task completed");
        }
        _ = backend_to_client => {
            debug!("Backend to client task completed");
        }
    }

    info!("WebSocket connection to {} closed", server_name);
}
