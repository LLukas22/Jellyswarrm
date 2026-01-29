use askama::Template;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{Html, IntoResponse, Response},
    Form, Json,
};
use serde::{Deserialize, Serialize};
use tracing::{error, info};

use crate::{
    merged_library_storage::{MergedLibrary, MergedLibrarySource},
    AppState,
};

/// Template for the merged libraries management page
#[derive(Template)]
#[template(path = "admin/merged_libraries.html")]
pub struct MergedLibrariesPageTemplate {
    pub ui_route: String,
}

/// Template for the merged library list partial (HTMX)
#[derive(Template)]
#[template(path = "admin/merged_library_list.html")]
pub struct MergedLibraryListTemplate {
    pub libraries: Vec<MergedLibraryWithSources>,
    pub ui_route: String,
}

/// Merged library with its sources for display
#[derive(Debug, Clone, Serialize)]
pub struct MergedLibraryWithSources {
    pub library: MergedLibrary,
    pub sources: Vec<SourceWithServerName>,
}

/// Source with server name for display
#[derive(Debug, Clone, Serialize)]
pub struct SourceWithServerName {
    pub source: MergedLibrarySource,
    pub server_name: String,
}

/// Form for creating a merged library
#[derive(Debug, Deserialize)]
pub struct CreateMergedLibraryForm {
    pub name: String,
    pub collection_type: String,
    pub dedup_strategy: Option<String>,
    pub is_global: Option<String>,
}

/// Form for adding a source to a merged library
#[derive(Debug, Deserialize)]
pub struct AddSourceForm {
    pub server_id: i64,
    pub library_id: String,
    pub library_name: Option<String>,
    pub priority: Option<i32>,
}

/// JSON response for API endpoints
#[derive(Debug, Serialize)]
pub struct MergedLibraryResponse {
    pub id: i64,
    pub virtual_id: String,
    pub name: String,
    pub collection_type: String,
    pub dedup_strategy: String,
    pub is_global: bool,
    pub sources: Vec<SourceResponse>,
}

#[derive(Debug, Serialize)]
pub struct SourceResponse {
    pub id: i64,
    pub server_id: i64,
    pub server_name: String,
    pub library_id: String,
    pub library_name: Option<String>,
    pub priority: i32,
}

/// Render the merged library list
async fn render_merged_library_list(state: &AppState) -> Result<String, String> {
    match state.merged_library_storage.list_merged_libraries().await {
        Ok(libraries) => {
            let mut libs_with_sources = Vec::new();

            for library in libraries {
                let sources = state
                    .merged_library_storage
                    .get_sources(library.id)
                    .await
                    .unwrap_or_default();

                let mut sources_with_names = Vec::new();
                for source in sources {
                    let server_name = state
                        .server_storage
                        .get_server_by_id(source.server_id)
                        .await
                        .ok()
                        .flatten()
                        .map(|s| s.name)
                        .unwrap_or_else(|| format!("Server {}", source.server_id));

                    sources_with_names.push(SourceWithServerName {
                        source,
                        server_name,
                    });
                }

                libs_with_sources.push(MergedLibraryWithSources {
                    library,
                    sources: sources_with_names,
                });
            }

            let template = MergedLibraryListTemplate {
                libraries: libs_with_sources,
                ui_route: state.get_ui_route().await,
            };

            template.render().map_err(|e| e.to_string())
        }
        Err(e) => Err(e.to_string()),
    }
}

/// Main merged libraries management page
pub async fn merged_libraries_page(State(state): State<AppState>) -> impl IntoResponse {
    let template = MergedLibrariesPageTemplate {
        ui_route: state.get_ui_route().await,
    };

    match template.render() {
        Ok(html) => Html(html).into_response(),
        Err(e) => {
            error!("Failed to render merged libraries template: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, "Template error").into_response()
        }
    }
}

/// Get merged library list partial (for HTMX)
pub async fn get_merged_library_list(State(state): State<AppState>) -> impl IntoResponse {
    match render_merged_library_list(&state).await {
        Ok(html) => Html(html).into_response(),
        Err(e) => {
            error!("Failed to render merged library list: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, "Error").into_response()
        }
    }
}

/// Create a new merged library
pub async fn create_merged_library(
    State(state): State<AppState>,
    Form(form): Form<CreateMergedLibraryForm>,
) -> Response {
    // Validate the form data
    if form.name.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Html("<div class=\"alert alert-error\">Library name cannot be empty</div>"),
        )
            .into_response();
    }

    // Validate collection type
    let collection_type = form.collection_type.to_lowercase();
    if !["movies", "tvshows", "music", "books", "mixed"].contains(&collection_type.as_str()) {
        return (
            StatusCode::BAD_REQUEST,
            Html("<div class=\"alert alert-error\">Invalid collection type</div>"),
        )
            .into_response();
    }

    let is_global = form.is_global.as_deref() == Some("on") || form.is_global.as_deref() == Some("true");

    match state
        .merged_library_storage
        .create_merged_library(
            form.name.trim(),
            &collection_type,
            form.dedup_strategy.as_deref(),
            None, // created_by - could add user context here
            is_global,
        )
        .await
    {
        Ok(library) => {
            info!(
                "Created merged library: {} ({}) with virtual ID: {}",
                library.name, collection_type, library.virtual_id
            );

            // Return updated library list
            get_merged_library_list(State(state)).await.into_response()
        }
        Err(e) => {
            error!("Failed to create merged library: {}", e);

            let error_message = if e.to_string().contains("UNIQUE constraint failed") {
                "A library with that name already exists"
            } else {
                "Failed to create merged library"
            };

            (
                StatusCode::BAD_REQUEST,
                Html(format!("<div class=\"alert alert-error\">{error_message}</div>")),
            )
                .into_response()
        }
    }
}

/// Delete a merged library
pub async fn delete_merged_library(
    State(state): State<AppState>,
    Path(library_id): Path<i64>,
) -> Response {
    match state.merged_library_storage.delete_merged_library(library_id).await {
        Ok(true) => {
            info!("Deleted merged library with ID: {}", library_id);
            get_merged_library_list(State(state)).await.into_response()
        }
        Ok(false) => (
            StatusCode::NOT_FOUND,
            Html("<div class=\"alert alert-error\">Library not found</div>"),
        )
            .into_response(),
        Err(e) => {
            error!("Failed to delete merged library: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Html("<div class=\"alert alert-error\">Failed to delete library</div>"),
            )
                .into_response()
        }
    }
}

/// Add a source to a merged library
pub async fn add_source(
    State(state): State<AppState>,
    Path(library_id): Path<i64>,
    Form(form): Form<AddSourceForm>,
) -> Response {
    // Validate the form data
    if form.library_id.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Html("<div class=\"alert alert-error\">Library ID cannot be empty</div>"),
        )
            .into_response();
    }

    // Check if merged library exists
    match state.merged_library_storage.get_merged_library(library_id).await {
        Ok(Some(_)) => {}
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Html("<div class=\"alert alert-error\">Merged library not found</div>"),
            )
                .into_response();
        }
        Err(e) => {
            error!("Failed to get merged library: {}", e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Html("<div class=\"alert alert-error\">Database error</div>"),
            )
                .into_response();
        }
    }

    // Check if server exists
    match state.server_storage.get_server_by_id(form.server_id).await {
        Ok(Some(_)) => {}
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Html("<div class=\"alert alert-error\">Server not found</div>"),
            )
                .into_response();
        }
        Err(e) => {
            error!("Failed to get server: {}", e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Html("<div class=\"alert alert-error\">Database error</div>"),
            )
                .into_response();
        }
    }

    let priority = form.priority.unwrap_or(0);

    match state
        .merged_library_storage
        .add_source(
            library_id,
            form.server_id,
            form.library_id.trim(),
            form.library_name.as_deref(),
            priority,
        )
        .await
    {
        Ok(source) => {
            info!(
                "Added source {} from server {} to merged library {}",
                source.library_id, source.server_id, library_id
            );

            get_merged_library_list(State(state)).await.into_response()
        }
        Err(e) => {
            error!("Failed to add source: {}", e);

            let error_message = if e.to_string().contains("UNIQUE constraint failed") {
                "This source is already added to the library"
            } else {
                "Failed to add source"
            };

            (
                StatusCode::BAD_REQUEST,
                Html(format!("<div class=\"alert alert-error\">{error_message}</div>")),
            )
                .into_response()
        }
    }
}

/// Remove a source from a merged library
pub async fn remove_source(
    State(state): State<AppState>,
    Path((library_id, source_id)): Path<(i64, i64)>,
) -> Response {
    // Verify the library exists
    match state.merged_library_storage.get_merged_library(library_id).await {
        Ok(Some(_)) => {}
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Html("<div class=\"alert alert-error\">Merged library not found</div>"),
            )
                .into_response();
        }
        Err(e) => {
            error!("Failed to get merged library: {}", e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Html("<div class=\"alert alert-error\">Database error</div>"),
            )
                .into_response();
        }
    }

    match state.merged_library_storage.remove_source(source_id).await {
        Ok(true) => {
            info!("Removed source {} from merged library {}", source_id, library_id);
            get_merged_library_list(State(state)).await.into_response()
        }
        Ok(false) => (
            StatusCode::NOT_FOUND,
            Html("<div class=\"alert alert-error\">Source not found</div>"),
        )
            .into_response(),
        Err(e) => {
            error!("Failed to remove source: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Html("<div class=\"alert alert-error\">Failed to remove source</div>"),
            )
                .into_response()
        }
    }
}

// ============================================================================
// JSON API Endpoints (for programmatic access)
// ============================================================================

/// List all merged libraries (JSON)
pub async fn list_merged_libraries_json(State(state): State<AppState>) -> impl IntoResponse {
    match state.merged_library_storage.list_merged_libraries().await {
        Ok(libraries) => {
            let mut responses = Vec::new();

            for library in libraries {
                let sources = state
                    .merged_library_storage
                    .get_sources(library.id)
                    .await
                    .unwrap_or_default();

                let mut source_responses = Vec::new();
                for source in sources {
                    let server_name = state
                        .server_storage
                        .get_server_by_id(source.server_id)
                        .await
                        .ok()
                        .flatten()
                        .map(|s| s.name)
                        .unwrap_or_else(|| format!("Server {}", source.server_id));

                    source_responses.push(SourceResponse {
                        id: source.id,
                        server_id: source.server_id,
                        server_name,
                        library_id: source.library_id,
                        library_name: source.library_name,
                        priority: source.priority,
                    });
                }

                responses.push(MergedLibraryResponse {
                    id: library.id,
                    virtual_id: library.virtual_id,
                    name: library.name,
                    collection_type: library.collection_type.as_str().to_string(),
                    dedup_strategy: library.dedup_strategy.as_str().to_string(),
                    is_global: library.is_global,
                    sources: source_responses,
                });
            }

            Json(responses).into_response()
        }
        Err(e) => {
            error!("Failed to list merged libraries: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, "Database error").into_response()
        }
    }
}

/// Get a single merged library (JSON)
pub async fn get_merged_library_json(
    State(state): State<AppState>,
    Path(library_id): Path<i64>,
) -> impl IntoResponse {
    match state.merged_library_storage.get_merged_library(library_id).await {
        Ok(Some(library)) => {
            let sources = state
                .merged_library_storage
                .get_sources(library.id)
                .await
                .unwrap_or_default();

            let mut source_responses = Vec::new();
            for source in sources {
                let server_name = state
                    .server_storage
                    .get_server_by_id(source.server_id)
                    .await
                    .ok()
                    .flatten()
                    .map(|s| s.name)
                    .unwrap_or_else(|| format!("Server {}", source.server_id));

                source_responses.push(SourceResponse {
                    id: source.id,
                    server_id: source.server_id,
                    server_name,
                    library_id: source.library_id,
                    library_name: source.library_name,
                    priority: source.priority,
                });
            }

            Json(MergedLibraryResponse {
                id: library.id,
                virtual_id: library.virtual_id,
                name: library.name,
                collection_type: library.collection_type.as_str().to_string(),
                dedup_strategy: library.dedup_strategy.as_str().to_string(),
                is_global: library.is_global,
                sources: source_responses,
            })
            .into_response()
        }
        Ok(None) => (StatusCode::NOT_FOUND, "Library not found").into_response(),
        Err(e) => {
            error!("Failed to get merged library: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, "Database error").into_response()
        }
    }
}
