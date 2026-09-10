use crate::models::{
    enums::{BaseItemKind, CollectionType},
    MediaItem,
};

pub(super) fn is_live_tv_user_view(item: &MediaItem) -> bool {
    item.collection_type == Some(CollectionType::LiveTv) && item.item_type == BaseItemKind::UserView
}

pub(super) fn automatic_library_key(item: &MediaItem) -> Option<String> {
    let collection_type = presentable_library_collection_type(item)?;
    let name = item.name.as_deref().unwrap_or("").to_lowercase();
    Some(format!("{}:{name}", collection_type.as_str()))
}

pub(super) fn presentable_library_collection_type(item: &MediaItem) -> Option<&CollectionType> {
    let is_library = matches!(
        item.item_type,
        BaseItemKind::UserView | BaseItemKind::CollectionFolder
    );
    item.collection_type
        .as_ref()
        .filter(|collection_type| is_library && **collection_type != CollectionType::LiveTv)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unassigned_library_uses_automatic_grouping() {
        let mut item: MediaItem = serde_json::from_value(serde_json::json!({
            "Id": "library-id",
            "Type": "CollectionFolder",
            "CollectionType": "movies",
        }))
        .unwrap();
        item.name = Some("Anime".to_string());

        assert_eq!(
            automatic_library_key(&item).as_deref(),
            Some("movies:anime")
        );
    }
}
