use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserPolicy {
    #[serde(rename = "IsAdministrator")]
    pub is_administrator: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct User {
    #[serde(rename = "Id")]
    pub id: String,
    #[serde(rename = "Name")]
    pub name: String,
    #[serde(rename = "ServerId")]
    pub server_id: Option<String>,
    #[serde(rename = "Policy")]
    pub policy: Option<UserPolicy>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthResponse {
    #[serde(rename = "AccessToken")]
    pub access_token: String,
    #[serde(rename = "User")]
    pub user: User,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaFolder {
    #[serde(rename = "Name")]
    pub name: String,
    #[serde(rename = "CollectionType")]
    pub collection_type: Option<String>,
    #[serde(rename = "Id")]
    pub id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaFoldersResponse {
    #[serde(rename = "Items")]
    pub items: Vec<MediaFolder>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewUserRequest {
    #[serde(rename = "Name")]
    pub name: String,
    #[serde(rename = "Password")]
    pub password: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PublicSystemInfo {
    #[serde(rename = "LocalAddress")]
    pub local_address: Option<String>,
    #[serde(rename = "ServerName")]
    pub server_name: Option<String>,
    #[serde(rename = "Version")]
    pub version: Option<String>,
    #[serde(rename = "ProductName")]
    pub product_name: Option<String>,
    #[serde(rename = "Id")]
    pub id: Option<String>,
    #[serde(rename = "StartupWizardCompleted")]
    pub startup_wizard_completed: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BaseItem {
    #[serde(rename = "Name")]
    pub name: String,
    #[serde(rename = "Id")]
    pub id: String,
    #[serde(rename = "Type")]
    pub type_: String,
    #[serde(rename = "ImageTags")]
    pub image_tags: Option<std::collections::HashMap<String, String>>,
    #[serde(rename = "ProductionYear")]
    pub production_year: Option<i32>,
    #[serde(rename = "RunTimeTicks")]
    pub run_time_ticks: Option<i64>,
    #[serde(rename = "CommunityRating")]
    pub community_rating: Option<f32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ItemsResponse {
    #[serde(rename = "Items")]
    pub items: Vec<BaseItem>,
    #[serde(rename = "TotalRecordCount")]
    pub total_record_count: i32,
}
