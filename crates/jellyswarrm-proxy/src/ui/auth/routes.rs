//! Handle management UI login/logout and authenticated-session redirects.
use askama::Template;
use axum::{
    extract::Query,
    http::StatusCode,
    response::{Html, IntoResponse, Redirect},
    routing::{get, post},
    Form, Router,
};
use axum_messages::{Message, Messages};
use serde::Deserialize;

use crate::{
    http::state::{AdapterPaths, AppState},
    ui::auth::{AuthSession, Credentials},
};

#[derive(Template)]
#[template(path = "login.html")]
pub(crate) struct LoginTemplate {
    messages: Vec<Message>,
    next: Option<String>,
    ui_route: String,
}

// This allows us to extract the "next" field from the query string. We use this
// to redirect after log in.
#[derive(Debug, Deserialize)]
pub(crate) struct NextUrl {
    next: Option<String>,
}

pub(crate) fn router() -> axum::Router<AppState> {
    Router::new()
        .route("/login", post(self::post::login))
        .route("/login", get(self::get::login))
        .route("/logout", get(self::get::logout))
}

mod post {

    use axum::extract::State;

    use super::*;

    pub(crate) async fn login(
        State(state): axum::extract::State<AdapterPaths>,
        mut auth_session: AuthSession,
        messages: Messages,
        Form(creds): Form<Credentials>,
    ) -> impl IntoResponse {
        let user = match auth_session.authenticate(creds.clone()).await {
            Ok(Some(user)) => user,
            Ok(None) => {
                messages.error("Invalid credentials");

                let mut login_url = format!("/{}/login", state.ui_route());
                if let Some(next) = creds.next {
                    login_url = format!("{login_url}?next={next}");
                } else {
                    login_url = format!("{login_url}?next=/{}", state.ui_route());
                }

                return Redirect::to(&login_url).into_response();
            }
            Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        };

        if auth_session.login(&user).await.is_err() {
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }

        messages.success(format!("Successfully logged in as {}", user.username));

        if let Some(ref next) = creds.next {
            Redirect::to(next)
        } else {
            Redirect::to(&format!("/{}", state.ui_route()))
        }
        .into_response()
    }
}

mod get {
    use axum::extract::State;
    use tracing::info;

    use super::*;

    pub(crate) async fn login(
        State(state): axum::extract::State<AdapterPaths>,
        messages: Messages,
        Query(NextUrl { next }): Query<NextUrl>,
    ) -> Html<String> {
        info!("Rendering login page, base={:?}", state.ui_route());
        Html(
            LoginTemplate {
                messages: messages.into_iter().collect(),
                next,
                ui_route: state.ui_route(),
            }
            .render()
            .unwrap(),
        )
    }

    pub(crate) async fn logout(
        State(state): axum::extract::State<AdapterPaths>,
        mut auth_session: AuthSession,
    ) -> impl IntoResponse {
        match auth_session.logout().await {
            Ok(_) => Redirect::to(&format!("/{}/login", state.ui_route())).into_response(),
            Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        }
    }
}
