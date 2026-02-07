use crate::error::Error;
use crate::models::{
    AuthResponse, IncludeBaseItemFields, IncludeItemTypes, MediaFoldersResponse, User,
};
use moka::ops::compute::Op;
use reqwest::{header, Client, StatusCode};
use serde::de::DeserializeOwned;
use serde_json::json;
use std::sync::{Arc, RwLock};
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

#[derive(Clone)]
pub struct JellyfinClient {
    base_url: Url,
    client_info: ClientInfo,
    http_client: Client,
    auth_token: Arc<RwLock<Option<String>>>,
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
            auth_token: Arc::new(RwLock::new(None)),
        })
    }

    pub fn with_token(&self, token: String) -> &Self {
        *self.auth_token.write().unwrap() = Some(token);
        self
    }

    pub fn get_token(&self) -> Option<String> {
        self.auth_token.read().unwrap().clone()
    }

    fn build_auth_header(&self) -> String {
        let mut header = format!(
            "MediaBrowser Client=\"{}\", Device=\"{}\", DeviceId=\"{}\", Version=\"{}\"",
            self.client_info.client,
            self.client_info.device,
            self.client_info.device_id,
            self.client_info.version
        );

        if let Some(token) = self.auth_token.read().unwrap().as_ref() {
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
        let url = self.base_url.join(path)?;
        let auth_header = self.build_auth_header();

        let mut request = self
            .http_client
            .request(method, url)
            .header(header::AUTHORIZATION, auth_header);

        if let Some(b) = body {
            request = request.json(b);
        }

        let response = request.send().await?;
        let status = response.status();

        if status.is_success() {
            let data = response.json::<T>().await?;
            Ok(data)
        } else {
            match status {
                StatusCode::UNAUTHORIZED => Err(Error::Unauthorized),
                StatusCode::FORBIDDEN => Err(Error::Forbidden),
                StatusCode::NOT_FOUND => Err(Error::NotFound),
                _ => {
                    let text = response.text().await.unwrap_or_default();
                    Err(Error::ServerError(format!("{} - {}", status, text)))
                }
            }
        }
    }

    async fn request_no_content(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<&serde_json::Value>,
    ) -> Result<(), Error> {
        let url = self.base_url.join(path)?;
        let auth_header = self.build_auth_header();

        let mut request = self
            .http_client
            .request(method, url)
            .header(header::AUTHORIZATION, auth_header);

        if let Some(b) = body {
            request = request.json(b);
        }

        let response = request.send().await?;
        let status = response.status();

        if status.is_success() {
            Ok(())
        } else {
            match status {
                StatusCode::UNAUTHORIZED => Err(Error::Unauthorized),
                StatusCode::FORBIDDEN => Err(Error::Forbidden),
                StatusCode::NOT_FOUND => Err(Error::NotFound),
                _ => {
                    let text = response.text().await.unwrap_or_default();
                    Err(Error::ServerError(format!("{} - {}", status, text)))
                }
            }
        }
    }

    pub async fn authenticate_by_name(
        &self,
        username: &str,
        password: &str,
    ) -> Result<User, Error> {
        let body = json!({
            "Username": username,
            "Pw": password
        });

        let response: AuthResponse = self
            .request(
                reqwest::Method::POST,
                "Users/AuthenticateByName",
                Some(&body),
            )
            .await
            .map_err(|e| match e {
                Error::Unauthorized => {
                    Error::AuthenticationFailed("Invalid credentials".to_string())
                }
                _ => e,
            })?;

        *self.auth_token.write().unwrap() = Some(response.access_token);
        Ok(response.user)
    }

    pub async fn logout(&self) -> Result<(), Error> {
        self.request_no_content(reqwest::Method::POST, "Sessions/Logout", None)
            .await?;
        *self.auth_token.write().unwrap() = None;
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

        let response: MediaFoldersResponse =
            self.request(reqwest::Method::GET, &path, None).await?;
        Ok(response.items)
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
        let url = self.base_url.join(&path)?;

        let auth_header = self.build_auth_header();

        let response = self
            .http_client
            .get(url)
            .header(header::AUTHORIZATION, auth_header)
            .query(&query)
            .send()
            .await?;

        let status = response.status();

        if status.is_success() {
            let data = response.json::<crate::models::ItemsResponse>().await?;
            Ok(data)
        } else {
            match status {
                StatusCode::UNAUTHORIZED => Err(Error::Unauthorized),
                StatusCode::FORBIDDEN => Err(Error::Forbidden),
                StatusCode::NOT_FOUND => Err(Error::NotFound),
                _ => {
                    let text = response.text().await.unwrap_or_default();
                    Err(Error::ServerError(format!("{} - {}", status, text)))
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
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
        assert_eq!(client.get_token().as_deref(), Some("test_token"));
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
            //.and(header("Authorization", "MediaBrowser Client=\"Jellyfin API Client\", Device=\"Unknown\", DeviceId=\"unknown-device-id\", Version=\"0.0.0\", Token=\"test_token\""))
            .respond_with(ResponseTemplate::new(200).set_body_json(folders_response))
            .mount(&mock_server)
            .await;

        let client_info = ClientInfo::default();
        let client = JellyfinClient::new(&mock_server.uri(), client_info).unwrap();
        let client = client.with_token("test_token".to_string());

        let folders = client.get_media_folders(None).await.unwrap();

        assert_eq!(folders.len(), 1);
        assert_eq!(folders[0].name, "Movies");
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
