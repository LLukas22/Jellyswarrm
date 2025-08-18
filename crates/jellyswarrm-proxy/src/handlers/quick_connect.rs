use axum::Json;
use hyper::StatusCode;

pub async fn handle_quick_connect() -> Result<Json<bool>, StatusCode> {
    Ok(Json(false))
}
