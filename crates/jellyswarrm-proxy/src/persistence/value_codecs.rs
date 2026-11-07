//! SQLite representations of shared domain values.
use crate::domain::{
    ids::{BackendItemId, VirtualItemId},
    security::{EncryptedPassword, HashedPassword, Password},
    user_id::UserId,
};
use sqlx::{database::Database, error::BoxDynError, Decode, Encode, Sqlite, Type};

macro_rules! text_value {
    ($value:ty, $decode:path) => {
        impl Type<Sqlite> for $value {
            fn type_info() -> <Sqlite as Database>::TypeInfo {
                <String as Type<Sqlite>>::type_info()
            }

            fn compatible(ty: &<Sqlite as Database>::TypeInfo) -> bool {
                <String as Type<Sqlite>>::compatible(ty)
            }
        }

        impl<'q> Encode<'q, Sqlite> for $value {
            fn encode_by_ref(
                &self,
                buf: &mut <Sqlite as Database>::ArgumentBuffer<'q>,
            ) -> Result<sqlx::encode::IsNull, BoxDynError> {
                <String as Encode<Sqlite>>::encode(self.as_str().to_owned(), buf)
            }
        }

        impl<'r> Decode<'r, Sqlite> for $value {
            fn decode(value: <Sqlite as Database>::ValueRef<'r>) -> Result<Self, BoxDynError> {
                <String as Decode<Sqlite>>::decode(value).map($decode)
            }
        }
    };
}

// UserId construction must normalize UUIDs; decoding raw storage directly into
// its private field would bypass the same normalization used by JSON and callers.
text_value!(UserId, UserId::from);
text_value!(Password, Password::from);
text_value!(HashedPassword, HashedPassword::from_hashed);
text_value!(EncryptedPassword, EncryptedPassword::from_raw);
// Media IDs round-trip verbatim: rows keep whatever form they were stored in, and
// lookups normalize explicitly, so the codec must not add normalization of its own.
text_value!(VirtualItemId, VirtualItemId::from);
text_value!(BackendItemId, BackendItemId::from);

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn secret_codecs_preserve_sqlite_string_representation() {
        let pool = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        let password = Password::from("opaque password");
        let hashed = HashedPassword::from_password(password.as_str());
        let encrypted = EncryptedPassword::from_raw("opaque encrypted value".into());
        let decoded: (Password, HashedPassword, EncryptedPassword) =
            sqlx::query_as("SELECT ?, ?, ?")
                .bind(&password)
                .bind(&hashed)
                .bind(&encrypted)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(decoded, (password, hashed, encrypted));
        let text: String = sqlx::query_scalar("SELECT ?")
            .bind(&decoded.0)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(text, "opaque password");
        // Match the existing SQLx String decoder, including its NULL behavior;
        // moving the codec must not silently add new row-validation policy.
        let raw: String = sqlx::query_scalar("SELECT NULL")
            .fetch_one(&pool)
            .await
            .unwrap();
        let wrapped: Password = sqlx::query_scalar("SELECT NULL")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(wrapped.as_str(), raw);
    }

    #[tokio::test]
    async fn media_id_codecs_round_trip_text_verbatim() {
        let pool = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        // Dashed and uppercase: the codec must not normalize on the way through.
        const RAW: &str = "550E8400-E29B-41D4-A716-446655440000";
        let (virtual_id, backend_id): (VirtualItemId, BackendItemId) =
            sqlx::query_as("SELECT ?, ?")
                .bind(VirtualItemId::from(RAW))
                .bind(BackendItemId::from(RAW))
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(virtual_id.as_str(), RAW);
        assert_eq!(backend_id.as_str(), RAW);
        let text: String = sqlx::query_scalar("SELECT ?")
            .bind(&virtual_id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(text, RAW);
    }
}
