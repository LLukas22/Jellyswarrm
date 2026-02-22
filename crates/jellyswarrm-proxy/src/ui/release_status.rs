use askama::Template;
use axum::{
    extract::State,
    http::StatusCode,
    response::{Html, IntoResponse},
};
use std::{
    cmp::Ordering,
    sync::LazyLock,
    time::{Duration, Instant},
};
use tracing::{error, warn};

use crate::AppState;

const RELEASE_STATUS_CACHE_TTL: Duration = Duration::from_secs(60 * 60 * 3);
const JELLYSWARRM_LATEST_RELEASE_URL: &str =
    "https://api.github.com/repos/LLukas22/Jellyswarrm/releases/latest";

#[derive(Debug, Clone)]
struct ReleaseStatusCacheEntry {
    checked_at: Instant,
    status: JellyswarrmReleaseStatus,
}

static JELLYSWARRM_RELEASE_STATUS_CACHE: LazyLock<
    tokio::sync::RwLock<Option<ReleaseStatusCacheEntry>>,
> = LazyLock::new(|| tokio::sync::RwLock::new(None));

#[derive(Debug, Clone)]
struct JellyswarrmReleaseStatus {
    css_variant: &'static str,
    icon_class: &'static str,
    label: String,
    title: String,
}

impl JellyswarrmReleaseStatus {
    fn latest(latest_tag: &str) -> Self {
        Self {
            css_variant: "latest",
            icon_class: "fas fa-circle-check",
            label: "Latest".to_string(),
            title: format!("Running latest release ({latest_tag})"),
        }
    }

    fn update_available(latest_tag: &str) -> Self {
        Self {
            css_variant: "update",
            icon_class: "fas fa-circle-up",
            label: format!("{latest_tag} available"),
            title: format!("New release available: {latest_tag}"),
        }
    }

    fn unknown(label: &str, title: &str) -> Self {
        Self {
            css_variant: "unknown",
            icon_class: "fas fa-triangle-exclamation",
            label: label.to_string(),
            title: title.to_string(),
        }
    }
}

#[derive(Debug, serde::Deserialize)]
struct GithubRelease {
    tag_name: String,
}

#[derive(Template)]
#[template(path = "components/release_status.html")]
struct ReleaseStatusTemplate {
    css_variant: &'static str,
    icon_class: &'static str,
    label: String,
    title: String,
}

pub async fn jellyswarrm_release_status(State(state): State<AppState>) -> impl IntoResponse {
    let status = get_jellyswarrm_release_status(&state).await;
    let template = ReleaseStatusTemplate {
        css_variant: status.css_variant,
        icon_class: status.icon_class,
        label: status.label,
        title: status.title,
    };

    match template.render() {
        Ok(html) => Html(html).into_response(),
        Err(e) => {
            error!("Failed to render release status template: {}", e);
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

async fn get_jellyswarrm_release_status(state: &AppState) -> JellyswarrmReleaseStatus {
    {
        let cache = JELLYSWARRM_RELEASE_STATUS_CACHE.read().await;
        if let Some(entry) = cache.as_ref() {
            if entry.checked_at.elapsed() < RELEASE_STATUS_CACHE_TTL {
                return entry.status.clone();
            }
        }
    }

    let status = fetch_jellyswarrm_release_status(&state.reqwest_client).await;

    let mut cache = JELLYSWARRM_RELEASE_STATUS_CACHE.write().await;
    *cache = Some(ReleaseStatusCacheEntry {
        checked_at: Instant::now(),
        status: status.clone(),
    });

    status
}

async fn fetch_jellyswarrm_release_status(client: &reqwest::Client) -> JellyswarrmReleaseStatus {
    let current_version = normalize_version(env!("CARGO_PKG_VERSION"));
    if current_version.is_empty() {
        return JellyswarrmReleaseStatus::unknown(
            "Version unknown",
            "Current Jellyswarrm version is unknown",
        );
    }

    let response = match client
        .get(JELLYSWARRM_LATEST_RELEASE_URL)
        .header(reqwest::header::ACCEPT, "application/vnd.github+json")
        .header(
            reqwest::header::USER_AGENT,
            format!("jellyswarrm-proxy/{}", env!("CARGO_PKG_VERSION")),
        )
        .send()
        .await
    {
        Ok(response) => response,
        Err(e) => {
            warn!("Failed to check latest Jellyswarrm release: {}", e);
            return JellyswarrmReleaseStatus::unknown(
                "Status unavailable",
                "Unable to check latest release",
            );
        }
    };

    if !response.status().is_success() {
        warn!(
            "Failed to check latest Jellyswarrm release: GitHub returned {}",
            response.status()
        );
        return JellyswarrmReleaseStatus::unknown(
            "Status unavailable",
            "Unable to check latest release",
        );
    }

    let release: GithubRelease = match response.json().await {
        Ok(release) => release,
        Err(e) => {
            warn!("Failed to parse latest Jellyswarrm release payload: {}", e);
            return JellyswarrmReleaseStatus::unknown(
                "Status unavailable",
                "Unable to check latest release",
            );
        }
    };

    let latest_tag = release.tag_name.trim().to_string();
    if latest_tag.is_empty() {
        return JellyswarrmReleaseStatus::unknown(
            "Status unavailable",
            "Latest release tag is missing",
        );
    }

    let latest_version = normalize_version(&latest_tag);
    if latest_version.is_empty() {
        return JellyswarrmReleaseStatus::unknown(
            "Status unavailable",
            "Latest release version is invalid",
        );
    }

    match compare_versions(&current_version, &latest_version) {
        Ordering::Less => JellyswarrmReleaseStatus::update_available(&latest_tag),
        Ordering::Equal | Ordering::Greater => JellyswarrmReleaseStatus::latest(&latest_tag),
    }
}

fn normalize_version(version: &str) -> String {
    version
        .trim()
        .trim_start_matches(['v', 'V'])
        .split(['+', '-'])
        .next()
        .unwrap_or_default()
        .to_string()
}

fn compare_versions(left: &str, right: &str) -> Ordering {
    let left_parts = version_parts(left);
    let right_parts = version_parts(right);
    let max_len = left_parts.len().max(right_parts.len());

    for index in 0..max_len {
        let left_value = *left_parts.get(index).unwrap_or(&0);
        let right_value = *right_parts.get(index).unwrap_or(&0);

        match left_value.cmp(&right_value) {
            Ordering::Equal => continue,
            ordering => return ordering,
        }
    }

    Ordering::Equal
}

fn version_parts(version: &str) -> Vec<u32> {
    normalize_version(version)
        .split('.')
        .map(|part| part.parse::<u32>().unwrap_or(0))
        .collect()
}
