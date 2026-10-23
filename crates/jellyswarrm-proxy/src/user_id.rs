use std::{fmt, ops::Deref};

use serde::{Deserialize, Serialize};
use sqlx::{database::Database, error::BoxDynError, Decode, Encode, Sqlite, Type};

/// A local user identifier in its canonical form.
///
/// UUIDs are lowercase and undashed. This normalizes identifiers; it does not
/// validate them. Non-UUID identifiers (including the UI's `admin`) are preserved exactly.
/// The inner string is private so construction, JSON input, and database decoding
/// all enforce the same normalization.
#[derive(Debug, Clone, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(from = "String")]
pub struct UserId(String);

impl UserId {
    pub fn new(value: impl Into<String>) -> Self {
        let value = value.into();
        Self(match uuid::Uuid::parse_str(&value) {
            Ok(id) => id.simple().to_string(),
            Err(_) => value,
        })
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<String> for UserId {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

impl From<&str> for UserId {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

impl From<UserId> for String {
    fn from(value: UserId) -> Self {
        value.0
    }
}

impl AsRef<str> for UserId {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl Deref for UserId {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        self.as_str()
    }
}

impl fmt::Display for UserId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl Type<Sqlite> for UserId {
    fn type_info() -> <Sqlite as Database>::TypeInfo {
        <String as Type<Sqlite>>::type_info()
    }

    fn compatible(ty: &<Sqlite as Database>::TypeInfo) -> bool {
        <String as Type<Sqlite>>::compatible(ty)
    }
}

impl<'q> Encode<'q, Sqlite> for UserId {
    fn encode_by_ref(
        &self,
        buf: &mut <Sqlite as Database>::ArgumentBuffer<'q>,
    ) -> Result<sqlx::encode::IsNull, BoxDynError> {
        <String as Encode<Sqlite>>::encode_by_ref(&self.0, buf)
    }
}

impl<'r> Decode<'r, Sqlite> for UserId {
    fn decode(value: <Sqlite as Database>::ValueRef<'r>) -> Result<Self, BoxDynError> {
        // A derived transparent decoder would bypass normalization.
        <String as Decode<Sqlite>>::decode(value).map(Self::from)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    const SIMPLE: &str = "550e8400e29b41d4a716446655440000";
    const DASHED: &str = "550E8400-E29B-41D4-A716-446655440000";

    #[test]
    fn construction_equality_and_hashing_use_canonical_ids() {
        let ids = [
            UserId::new(SIMPLE),
            UserId::from(DASHED),
            UserId::from(DASHED.to_string()),
        ];
        assert!(ids.iter().all(|id| id.as_str() == SIMPLE));
        assert_eq!(ids.into_iter().collect::<HashSet<_>>().len(), 1);
    }

    #[test]
    fn serde_normalizes_input_and_serializes_as_a_string() {
        let id: UserId = serde_json::from_value(serde_json::json!(DASHED)).unwrap();
        assert_eq!(id, UserId::new(SIMPLE));
        assert_eq!(serde_json::to_value(&id).unwrap(), SIMPLE);
        assert!(serde_json::from_value::<UserId>(serde_json::json!(42)).is_err());
    }

    #[test]
    fn non_uuid_identifiers_are_preserved_exactly() {
        for id in [
            "",
            "admin",
            "opaque-User-ID",
            "not-a-valid-uuid",
            "0123456789abcdef0123456789abcde",
        ] {
            assert_eq!(UserId::new(id).as_str(), id);
        }
        assert_ne!(UserId::new("Opaque"), UserId::new("opaque"));
    }

    #[tokio::test]
    async fn database_decode_normalizes_and_encode_stays_text() {
        let pool = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        let id: UserId = sqlx::query_scalar("SELECT ?")
            .bind(DASHED)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(id.as_str(), SIMPLE);
        let encoded: String = sqlx::query_scalar("SELECT ?")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(encoded, SIMPLE);
    }
}
