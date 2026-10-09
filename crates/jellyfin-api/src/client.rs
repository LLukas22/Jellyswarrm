use crate::error::Error;
use crate::models::{
    AuthResponse, IncludeBaseItemFields, IncludeItemTypes, MediaFoldersResponse, User,
};
use reqwest::{header, Client, StatusCode};
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::json;
use tokio::sync::RwLock;
use tracing::info;
use url::Url;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ClientInfo {
    pub client: String,
    pub device: String,
    pub device_id: String,
    pub version: String,
}

impl Default for ClientInfo {
    fn default() -> Self {
        Self {
            client: "Jellyfin API Client".to_string(),
            device: "Unknown".to_string(),
            device_id: "unknown-device-id".to_string(),
            version: "0.0.0".to_string(),
        }
    }
}

pub struct JellyfinClient {
    base_url: Url,
    client_info: ClientInfo,
    http_client: Client,
    auth_token: RwLock<Option<String>>,
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct QuickConnectState {
    pub authenticated: bool,
    pub secret: String,
    pub code: String,
}

impl PartialEq for JellyfinClient {
    fn eq(&self, other: &Self) -> bool {
        self.base_url == other.base_url && self.client_info == other.client_info
    }
}

impl Eq for JellyfinClient {}

impl std::hash::Hash for JellyfinClient {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.base_url.hash(state);
        self.client_info.hash(state);
    }
}

impl JellyfinClient {
    pub fn new(base_url: &str, client_info: ClientInfo) -> Result<Self, Error> {
        let http_client = Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()?;

        Self::new_with_client(base_url, client_info, http_client)
    }

    pub fn new_with_client(
        base_url: &str,
        client_info: ClientInfo,
        http_client: Client,
    ) -> Result<Self, Error> {
        let mut url = Url::parse(base_url)?;
        // Ensure trailing slash for consistent joining
        if !url.path().ends_with('/') {
            url.path_segments_mut()
                .map_err(|_| Error::UrlParse(url::ParseError::EmptyHost))?
                .push("");
        }

        Ok(Self {
            base_url: url,
            client_info,
            http_client,
            auth_token: RwLock::new(None),
        })
    }

    pub async fn with_token(&self, token: String) -> &Self {
        *self.auth_token.write().await = Some(token);
        self
    }

    pub async fn get_token(&self) -> Option<String> {
        self.auth_token.read().await.clone()
    }

    async fn build_auth_header(&self) -> String {
        let mut header = format!(
            "MediaBrowser Client=\"{}\", Device=\"{}\", DeviceId=\"{}\", Version=\"{}\"",
            self.client_info.client,
            self.client_info.device,
            self.client_info.device_id,
            self.client_info.version
        );

        if let Some(token) = self.auth_token.read().await.as_ref() {
            header.push_str(&format!(", Token=\"{}\"", token));
        }

        // println!("DEBUG HEADER: {}", header);
        header
    }

    async fn request<T: DeserializeOwned>(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<&serde_json::Value>,
    ) -> Result<T, Error> {
        let mut request = self.request_builder(method, path).await?;

        if let Some(b) = body {
            request = request.json(b);
        }

        let response = request.send().await?;
        Self::parse_response(response).await
    }

    async fn request_no_content(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<&serde_json::Value>,
    ) -> Result<(), Error> {
        let mut request = self.request_builder(method, path).await?;

        if let Some(b) = body {
            request = request.json(b);
        }

        let response = request.send().await?;
        Self::check_success(response).await
    }

    async fn request_builder(
        &self,
        method: reqwest::Method,
        path: &str,
    ) -> Result<reqwest::RequestBuilder, Error> {
        let url = self.base_url.join(path)?;
        let auth_header = self.build_auth_header().await;
        let user_agent = format!("Jellyswarrm API Client/{}", env!("CARGO_PKG_VERSION"));

        Ok(self
            .http_client
            .request(method, url)
            .header(header::AUTHORIZATION, auth_header)
            .header(header::USER_AGENT, user_agent))
    }

    async fn parse_response<T: DeserializeOwned>(response: reqwest::Response) -> Result<T, Error> {
        if response.status().is_success() {
            return Ok(response.json::<T>().await?);
        }

        Err(Self::response_error(response).await)
    }

    async fn check_success(response: reqwest::Response) -> Result<(), Error> {
        if response.status().is_success() {
            return Ok(());
        }

        Err(Self::response_error(response).await)
    }

    async fn response_error(response: reqwest::Response) -> Error {
        let status = response.status();
        match status {
            StatusCode::UNAUTHORIZED => Error::Unauthorized,
            StatusCode::FORBIDDEN => Error::Forbidden,
            StatusCode::NOT_FOUND => Error::NotFound,
            _ => {
                let text = response.text().await.unwrap_or_default();
                Error::ServerError(format!("{} - {}", status, text))
            }
        }
    }

    pub async fn authenticate_by_name_typed<T: DeserializeOwned>(
        &self,
        username: &str,
        password: &str,
    ) -> Result<T, Error> {
        let body = json!({
            "Username": username,
            "Pw": password
        });

        self.request(
            reqwest::Method::POST,
            "Users/AuthenticateByName",
            Some(&body),
        )
        .await
        .map_err(|e| match e {
            Error::Unauthorized => Error::AuthenticationFailed("Invalid credentials".to_string()),
            _ => e,
        })
    }

    pub async fn authenticate_by_name(
        &self,
        username: &str,
        password: &str,
    ) -> Result<User, Error> {
        let response: AuthResponse = self.authenticate_by_name_typed(username, password).await?;

        let mut write_guard = self.auth_token.write().await;
        *write_guard = Some(response.access_token);
        info!("Authenticated user: {}", response.user.name);
        Ok(response.user)
    }

    pub async fn quick_connect_enabled(&self) -> Result<bool, Error> {
        self.request(reqwest::Method::GET, "QuickConnect/Enabled", None)
            .await
    }

    pub async fn initiate_quick_connect(&self) -> Result<QuickConnectState, Error> {
        self.request(reqwest::Method::POST, "QuickConnect/Initiate", None)
            .await
    }

    pub async fn quick_connect_state(&self, secret: &str) -> Result<QuickConnectState, Error> {
        let mut url = self.base_url.join("QuickConnect/Connect")?;
        url.query_pairs_mut().append_pair("Secret", secret);
        let response = self
            .http_client
            .get(url)
            .header(header::AUTHORIZATION, self.build_auth_header().await)
            .send()
            .await?;
        Self::parse_response(response).await
    }

    pub async fn authenticate_with_quick_connect<T: DeserializeOwned>(
        &self,
        secret: &str,
    ) -> Result<T, Error> {
        self.request(
            reqwest::Method::POST,
            "Users/AuthenticateWithQuickConnect",
            Some(&json!({ "Secret": secret })),
        )
        .await
    }

    pub async fn get_me_typed<T: DeserializeOwned>(&self) -> Result<T, Error> {
        self.request(reqwest::Method::GET, "Users/Me", None).await
    }

    pub async fn logout(&self) -> Result<(), Error> {
        self.request_no_content(reqwest::Method::POST, "Sessions/Logout", None)
            .await?;
        *self.auth_token.write().await = None;
        Ok(())
    }

    pub async fn get_me(&self) -> Result<User, Error> {
        self.request(reqwest::Method::GET, "Users/Me", None).await
    }

    pub async fn get_media_folders(
        &self,
        user_id: Option<&str>,
    ) -> Result<Vec<crate::models::MediaFolder>, Error> {
        let path = if let Some(uid) = user_id {
            format!("Users/{}/Views", uid)
        } else {
            "Library/MediaFolders".to_string()
        };

        const PAGE_SIZE: usize = 100;
        let mut folders = Vec::new();
        let mut seen = std::collections::HashSet::new();
        let mut start_index = 0;
        let mut expected_total = None;
        loop {
            // Jellyfin Views normally returns everything without paging. Only
            // request a continuation when the response advertises missing items.
            let page_path = if user_id.is_some() && start_index == 0 {
                path.clone()
            } else {
                format!("{path}?StartIndex={start_index}&Limit={PAGE_SIZE}")
            };
            let response: MediaFoldersResponse =
                self.request(reqwest::Method::GET, &page_path, None).await?;
            if let (Some(previous), Some(current)) = (expected_total, response.total_record_count) {
                if previous != current {
                    return Err(Error::InvalidResponse(
                        "Library inventory changed while fetching its pages".into(),
                    ));
                }
            }
            expected_total = response.total_record_count.or(expected_total);
            let page_len = response.items.len();
            if page_len == 0 {
                if expected_total.is_some_and(|total| start_index < total) {
                    return Err(Error::InvalidResponse(
                        "Library pagination ended before all libraries were returned".into(),
                    ));
                }
                break;
            }
            // Fail rather than cache an incomplete list if an upstream ignores StartIndex.
            if response
                .items
                .iter()
                .any(|folder| !seen.insert(folder.id.clone()))
            {
                return Err(Error::InvalidResponse(
                    "Library pagination returned duplicate libraries".into(),
                ));
            }
            start_index += page_len;
            folders.extend(response.items);
            if expected_total.map_or(user_id.is_some() || page_len < PAGE_SIZE, |total| {
                start_index >= total
            }) {
                break;
            }
        }
        Ok(folders)
    }

    pub async fn get_public_system_info(&self) -> Result<crate::models::PublicSystemInfo, Error> {
        self.request(reqwest::Method::GET, "System/Info/Public", None)
            .await
    }

    pub async fn get_branding_configuration(
        &self,
    ) -> Result<crate::models::BrandingConfiguration, Error> {
        self.request(reqwest::Method::GET, "Branding/Configuration", None)
            .await
    }

    // Admin methods

    pub async fn get_users(&self) -> Result<Vec<User>, Error> {
        self.request(reqwest::Method::GET, "Users", None).await
    }

    pub async fn create_user(&self, username: &str, password: Option<&str>) -> Result<User, Error> {
        let body = json!({
            "Name": username,
            "Password": password
        });

        let user: User = self
            .request(reqwest::Method::POST, "Users/New", Some(&body))
            .await?;

        Ok(user)
    }

    pub async fn delete_user(&self, user_id: &str) -> Result<(), Error> {
        let path = format!("Users/{}", user_id);
        self.request_no_content(reqwest::Method::DELETE, &path, None)
            .await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn get_items(
        &self,
        user_id: &str,
        parent_id: Option<&str>,
        recursive: bool,
        include_item_types: Option<Vec<IncludeItemTypes>>,
        limit: Option<i32>,
        start_index: Option<i32>,
        sort_by: Option<String>,
        sort_order: Option<String>,
        include_fields: Option<Vec<IncludeBaseItemFields>>,
    ) -> Result<crate::models::ItemsResponse, Error> {
        let mut query = vec![
            ("Recursive", recursive.to_string()),
            //("Fields", "PrimaryImageAspectRatio,CanDelete,BasicSyncInfo,ProductionYear,RunTimeTicks,CommunityRating".to_string()),
        ];

        if let Some(include_fields) = include_fields {
            let fields_str = include_fields
                .iter()
                .map(|f| f.to_string())
                .collect::<Vec<String>>()
                .join(",");
            query.push(("Fields", fields_str));
        }

        if let Some(pid) = parent_id {
            query.push(("ParentId", pid.to_string()));
        }

        if let Some(types) = include_item_types {
            query.push((
                "IncludeItemTypes",
                types
                    .iter()
                    .map(|f| f.to_string())
                    .collect::<Vec<String>>()
                    .join(","),
            ));
        }

        if let Some(l) = limit {
            query.push(("Limit", l.to_string()));
        }

        if let Some(si) = start_index {
            query.push(("StartIndex", si.to_string()));
        }

        if let Some(s) = sort_by {
            query.push(("SortBy", s));
        }

        if let Some(o) = sort_order {
            query.push(("SortOrder", o));
        }

        let path = format!("Users/{}/Items", user_id);
        let response = self
            .request_builder(reqwest::Method::GET, &path)
            .await?
            .query(&query)
            .send()
            .await?;

        Self::parse_response(response).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{header as header_matcher, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn test_authenticate_success() {
        let mock_server = MockServer::start().await;

        let auth_response = json!({
            "AccessToken": "test_token",
            "User": {
                "Id": "user_id",
                "Name": "test_user",
                "ServerId": "server_id"
            }
        });

        Mock::given(method("POST"))
            .and(path("/Users/AuthenticateByName"))
            .respond_with(ResponseTemplate::new(200).set_body_json(auth_response))
            .mount(&mock_server)
            .await;

        let client_info = ClientInfo::default();
        let client = JellyfinClient::new(&mock_server.uri(), client_info).unwrap();

        let user = client
            .authenticate_by_name("test_user", "password")
            .await
            .unwrap();

        assert_eq!(user.name, "test_user");
        assert_eq!(client.get_token().await.as_deref(), Some("test_token"));
    }

    #[tokio::test]
    async fn quick_connect_client_exchanges_secret_for_access_token() {
        let backend = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/QuickConnect/Enabled"))
            .respond_with(ResponseTemplate::new(200).set_body_json(true))
            .mount(&backend)
            .await;
        Mock::given(method("POST"))
            .and(path("/QuickConnect/Initiate"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "Secret": "private-secret", "Code": "123456", "Authenticated": false,
            })))
            .mount(&backend)
            .await;
        Mock::given(method("GET"))
            .and(path("/QuickConnect/Connect"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "Secret": "private-secret", "Code": "123456", "Authenticated": true,
            })))
            .mount(&backend)
            .await;
        Mock::given(method("POST"))
            .and(path("/Users/AuthenticateWithQuickConnect"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "AccessToken": "durable-token", "User": { "Id": "remote-id", "Name": "Remote" },
            })))
            .mount(&backend)
            .await;
        let client = JellyfinClient::new(&backend.uri(), ClientInfo::default()).unwrap();
        assert!(client.quick_connect_enabled().await.unwrap());
        let state = client.initiate_quick_connect().await.unwrap();
        assert_eq!(state.code, "123456");
        assert!(
            client
                .quick_connect_state(&state.secret)
                .await
                .unwrap()
                .authenticated
        );
        let auth: AuthResponse = client
            .authenticate_with_quick_connect(&state.secret)
            .await
            .unwrap();
        assert_eq!(auth.access_token, "durable-token");
        assert_eq!(auth.user.id, "remote-id");
    }

    #[tokio::test]
    async fn test_get_media_folders() {
        let mock_server = MockServer::start().await;

        let folders_response = json!({
            "Items": [
                {
                    "Name": "Movies",
                    "CollectionType": "movies",
                    "Id": "folder_1"
                }
            ]
        });

        Mock::given(method("GET"))
            .and(path("/Library/MediaFolders"))
            .and(header_matcher(
                "user-agent",
                format!("Jellyswarrm API Client/{}", env!("CARGO_PKG_VERSION")),
            ))
            //.and(header("Authorization", "MediaBrowser Client=\"Jellyfin API Client\", Device=\"Unknown\", DeviceId=\"unknown-device-id\", Version=\"0.0.0\", Token=\"test_token\""))
            .respond_with(ResponseTemplate::new(200).set_body_json(folders_response))
            .mount(&mock_server)
            .await;

        let client_info = ClientInfo::default();
        let client = JellyfinClient::new(&mock_server.uri(), client_info).unwrap();
        let client = client.with_token("test_token".to_string()).await;

        let folders = client.get_media_folders(None).await.unwrap();

        assert_eq!(folders.len(), 1);
        assert_eq!(folders[0].name, "Movies");
    }

    #[tokio::test]
    async fn media_folders_fetches_all_pages_even_when_server_caps_page_size() {
        use wiremock::matchers::query_param;

        let server = MockServer::start().await;
        for start in [0, 20, 40] {
            let items = (start..(start + 20).min(45))
                .map(|id| json!({"Id": format!("library-{id}"), "Name": format!("Library {id}"), "CollectionType": "movies"}))
                .collect::<Vec<_>>();
            Mock::given(method("GET"))
                .and(path("/Library/MediaFolders"))
                .and(query_param("StartIndex", start.to_string()))
                .and(query_param("Limit", "100"))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "Items": items, "TotalRecordCount": 45
                })))
                .expect(1)
                .mount(&server)
                .await;
        }
        let client = JellyfinClient::new(&server.uri(), ClientInfo::default()).unwrap();
        let folders = client.get_media_folders(None).await.unwrap();
        assert_eq!(folders.len(), 45);
        assert_eq!(folders[44].id, "library-44");
    }

    #[tokio::test]
    async fn media_folders_rejects_repeated_pages() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/Library/MediaFolders"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "Items": [{"Id": "same", "Name": "Movies"}], "TotalRecordCount": 2
            })))
            .expect(2)
            .mount(&server)
            .await;
        let client = JellyfinClient::new(&server.uri(), ClientInfo::default()).unwrap();
        assert!(matches!(
            client.get_media_folders(None).await,
            Err(Error::InvalidResponse(_))
        ));
    }

    #[tokio::test]
    async fn media_folders_does_not_return_partial_results_on_later_page_failure() {
        use wiremock::matchers::query_param;

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/Library/MediaFolders"))
            .and(query_param("StartIndex", "0"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "Items": [{"Id": "first", "Name": "Movies"}], "TotalRecordCount": 2
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/Library/MediaFolders"))
            .and(query_param("StartIndex", "1"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let client = JellyfinClient::new(&server.uri(), ClientInfo::default()).unwrap();
        assert!(client.get_media_folders(None).await.is_err());
    }

    #[tokio::test]
    async fn user_views_remain_unpaginated() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/Users/user-id/Views"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "Items": [{"Id": "view", "Name": "Movies"}]
            })))
            .expect(1)
            .mount(&server)
            .await;
        let client = JellyfinClient::new(&server.uri(), ClientInfo::default()).unwrap();
        assert_eq!(
            client
                .get_media_folders(Some("user-id"))
                .await
                .unwrap()
                .len(),
            1
        );
        assert!(server.received_requests().await.unwrap()[0]
            .url
            .query()
            .is_none());
    }

    #[tokio::test]
    async fn user_views_fetch_continuation_when_upstream_caps_results() {
        use wiremock::matchers::query_param;
        let server = MockServer::start().await;
        Mock::given(method("GET")).and(path("/Users/user-id/Views"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "Items": (0..20).map(|id| json!({"Id":format!("library-{id}"),"Name":"Movies"})).collect::<Vec<_>>(),
                "TotalRecordCount": 25
            }))).with_priority(10).expect(1).mount(&server).await;
        Mock::given(method("GET")).and(path("/Users/user-id/Views"))
            .and(query_param("StartIndex", "20")).and(query_param("Limit", "100"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "Items": (20..25).map(|id| json!({"Id":format!("library-{id}"),"Name":"Movies"})).collect::<Vec<_>>(),
                "TotalRecordCount": 25
            }))).expect(1).mount(&server).await;
        let client = JellyfinClient::new(&server.uri(), ClientInfo::default()).unwrap();
        assert_eq!(
            client
                .get_media_folders(Some("user-id"))
                .await
                .unwrap()
                .len(),
            25
        );
    }

    #[tokio::test]
    async fn user_views_reject_incomplete_repeated_pages() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/Users/user-id/Views"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "Items": [{"Id":"same","Name":"Movies"}], "TotalRecordCount": 2
            })))
            .expect(2)
            .mount(&server)
            .await;
        let client = JellyfinClient::new(&server.uri(), ClientInfo::default()).unwrap();
        assert!(client.get_media_folders(Some("user-id")).await.is_err());
    }

    #[tokio::test]
    async fn test_get_branding_configuration() {
        let mock_server = MockServer::start().await;

        let branding_response = json!({
            "LoginDisclaimer": "Welcome to Jellyfin",
            "CustomCss": "body { background: black; }",
            "SplashscreenEnabled": true
        });

        Mock::given(method("GET"))
            .and(path("/Branding/Configuration"))
            .respond_with(ResponseTemplate::new(200).set_body_json(branding_response))
            .mount(&mock_server)
            .await;

        let client_info = ClientInfo::default();
        let client = JellyfinClient::new(&mock_server.uri(), client_info).unwrap();

        let config = client.get_branding_configuration().await.unwrap();

        assert_eq!(
            config.login_disclaimer,
            Some("Welcome to Jellyfin".to_string())
        );
        assert_eq!(
            config.custom_css,
            Some("body { background: black; }".to_string())
        );
        assert_eq!(config.splashscreen_enabled, Some(true));
    }
}
