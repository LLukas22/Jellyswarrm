use std::collections::VecDeque;
use std::str::FromStr;

use crate::{
    duplicate_policy::{apply_duplicate_policy, DuplicatePolicyConfig, TaggedMediaItem},
    models::{
        enums::{BaseItemKind, CollectionType, ItemSortBy, SortOrder},
        ItemsResponseVariants, ItemsResponseWithCount, MediaItem,
    },
    server_storage::Server,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Pagination {
    pub(super) start_index: usize,
    pub(super) limit: Option<usize>,
}

impl Pagination {
    pub(super) fn from_url(url: &url::Url) -> Self {
        let mut pagination = Self {
            start_index: 0,
            limit: None,
        };

        for (key, value) in url.query_pairs() {
            if key.eq_ignore_ascii_case("StartIndex") {
                if let Ok(start_index) = value.parse() {
                    pagination.start_index = start_index;
                }
            } else if key.eq_ignore_ascii_case("Limit") {
                if let Ok(limit) = value.parse() {
                    pagination.limit = Some(limit);
                }
            }
        }

        pagination
    }

    fn apply(self, items: Vec<MediaItem>) -> Vec<MediaItem> {
        if self.start_index >= items.len() {
            return Vec::new();
        }

        let items = items.into_iter().skip(self.start_index);
        if let Some(limit) = self.limit {
            items.take(limit).collect()
        } else {
            items.collect()
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ResponseShape {
    Bare,
    Counted,
}

impl ResponseShape {
    pub(super) fn from_responses<'a>(
        responses: impl IntoIterator<Item = &'a ItemsResponseVariants>,
    ) -> Self {
        if responses
            .into_iter()
            .any(|response| matches!(response, ItemsResponseVariants::WithCount(_)))
        {
            Self::Counted
        } else {
            Self::Bare
        }
    }

    fn wrap(
        self,
        items: Vec<MediaItem>,
        total_count: usize,
        pagination: Pagination,
    ) -> ItemsResponseVariants {
        match self {
            Self::Bare => ItemsResponseVariants::Bare(items),
            Self::Counted => ItemsResponseVariants::WithCount(ItemsResponseWithCount {
                items,
                total_record_count: to_i32(total_count),
                start_index: to_i32(pagination.start_index),
            }),
        }
    }
}

pub(super) enum MergeStrategy<'a> {
    Interleave,
    DuplicatePolicy(&'a DuplicatePolicyConfig),
}

pub(super) struct ServerItems {
    pub(super) response: ItemsResponseVariants,
    pub(super) server: Server,
}

#[derive(Default)]
pub(super) struct FederatedItems {
    items: Vec<MediaItem>,
    reported_total: Option<usize>,
}

impl FederatedItems {
    pub(super) fn new(items: Vec<MediaItem>) -> Self {
        Self {
            items,
            reported_total: None,
        }
    }

    pub(super) fn interleaved(responses: Vec<ItemsResponseVariants>) -> Self {
        Self::new(interleave(responses))
    }

    pub(super) fn from_tagged_items(
        items: Vec<TaggedMediaItem>,
        config: &DuplicatePolicyConfig,
    ) -> Self {
        Self::new(apply_duplicate_policy(items, config))
    }

    pub(super) fn merge_server_items(
        mut self,
        server_items: Vec<ServerItems>,
        strategy: MergeStrategy<'_>,
    ) -> Self {
        let items = match strategy {
            MergeStrategy::Interleave => interleave(
                server_items
                    .into_iter()
                    .map(|items| items.response)
                    .collect(),
            ),
            MergeStrategy::DuplicatePolicy(config) => {
                apply_duplicate_policy(tag_server_items(server_items), config)
            }
        };
        self.items.extend(items);
        self
    }

    pub(super) fn with_reported_total(mut self, total_count: usize) -> Self {
        self.reported_total = Some(total_count);
        self
    }

    pub(super) fn len(&self) -> usize {
        self.items.len()
    }

    pub(super) fn into_response(
        mut self,
        url: &url::Url,
        pagination: Pagination,
        shape: ResponseShape,
    ) -> ItemsResponseVariants {
        sort_items(&mut self.items, url);
        let total_count = self.reported_total.unwrap_or(self.items.len());
        let items = pagination.apply(self.items);
        shape.wrap(items, total_count, pagination)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SortCriterion {
    field: ItemSortBy,
    order: SortOrder,
}

fn sort_items(items: &mut [MediaItem], url: &url::Url) {
    let criteria = sort_criteria(url);
    items.sort_by(|left, right| {
        criteria
            .iter()
            .find_map(|criterion| {
                let ordering = left.cmp_by(right, criterion.field);
                (!ordering.is_eq()).then(|| match criterion.order {
                    SortOrder::Ascending => ordering,
                    SortOrder::Descending => ordering.reverse(),
                })
            })
            .unwrap_or(std::cmp::Ordering::Equal)
    });
}

fn sort_criteria(url: &url::Url) -> Vec<SortCriterion> {
    let mut fields = query_list::<ItemSortBy>(url, "SortBy");
    let mut orders = query_list::<SortOrder>(url, "SortOrder");

    if fields.is_empty() {
        if url.path().to_ascii_lowercase().ends_with("/latest") {
            fields.push(ItemSortBy::DateCreated);
            orders = vec![SortOrder::Descending];
        } else {
            fields.push(ItemSortBy::SortName);
        }
    }

    let default_order = orders.first().copied().unwrap_or_default();
    fields
        .into_iter()
        .enumerate()
        .map(|(index, field)| SortCriterion {
            field,
            order: orders.get(index).copied().unwrap_or(default_order),
        })
        .collect()
}

fn query_list<T>(url: &url::Url, expected_key: &str) -> Vec<T>
where
    T: FromStr,
{
    let mut values = Vec::new();
    for (key, value) in url.query_pairs() {
        if key.eq_ignore_ascii_case(expected_key) {
            values = value
                .split(',')
                .filter_map(|value| value.trim().parse().ok())
                .collect();
        }
    }
    values
}

fn interleave(responses: Vec<ItemsResponseVariants>) -> Vec<MediaItem> {
    let mut queues = responses
        .into_iter()
        .map(|response| VecDeque::from(response.into_items()))
        .collect::<Vec<_>>();
    let mut remaining = queues.iter().map(VecDeque::len).sum();
    let mut items = Vec::with_capacity(remaining);
    let mut has_live_tv_user_view = false;

    while remaining > 0 {
        for queue in &mut queues {
            let Some(item) = queue.pop_front() else {
                continue;
            };
            remaining -= 1;

            if is_live_tv_user_view(&item) {
                if has_live_tv_user_view {
                    continue;
                }
                has_live_tv_user_view = true;
            }

            items.push(item);
        }
    }

    items
}

fn tag_server_items(server_items: Vec<ServerItems>) -> Vec<TaggedMediaItem> {
    server_items
        .into_iter()
        .flat_map(|server_items| {
            server_items
                .response
                .into_items()
                .into_iter()
                .map(move |item| TaggedMediaItem {
                    item,
                    server: server_items.server.clone(),
                })
        })
        .collect()
}

fn is_live_tv_user_view(item: &MediaItem) -> bool {
    item.collection_type == Some(CollectionType::LiveTv) && item.item_type == BaseItemKind::UserView
}

fn to_i32(value: usize) -> i32 {
    value.min(i32::MAX as usize) as i32
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::MediaStreamingMode, duplicate_policy::DuplicatePolicy, server_id::ServerId,
        server_url::ServerUrl,
    };
    use serde_json::json;

    #[test]
    fn pagination_from_url_defaults_when_absent() {
        let url = url::Url::parse("http://localhost/Items?Recursive=true").unwrap();

        assert_eq!(
            Pagination::from_url(&url),
            Pagination {
                start_index: 0,
                limit: None,
            }
        );
    }

    #[test]
    fn pagination_from_url_is_case_insensitive() {
        let url = url::Url::parse("http://localhost/Items?startindex=12&limit=24").unwrap();

        assert_eq!(
            Pagination::from_url(&url),
            Pagination {
                start_index: 12,
                limit: Some(24),
            }
        );
    }

    #[test]
    fn pagination_from_url_ignores_invalid_numbers() {
        let url = url::Url::parse("http://localhost/Items?StartIndex=nope&Limit=nope").unwrap();

        assert_eq!(
            Pagination::from_url(&url),
            Pagination {
                start_index: 0,
                limit: None,
            }
        );
    }

    #[test]
    fn sorting_honors_requested_order() {
        let url =
            url::Url::parse("http://localhost/Items?SortBy=SortName&SortOrder=Descending").unwrap();
        let items = vec![
            named_media_item("a", "Alpha"),
            named_media_item("b", "Beta"),
        ];

        let response = FederatedItems::new(items).into_response(
            &url,
            Pagination {
                start_index: 0,
                limit: None,
            },
            ResponseShape::Bare,
        );

        assert_eq!(item_ids(&response.into_items()), vec!["b", "a"]);
    }

    #[test]
    fn sorting_orders_latest_by_date_created() {
        let url = url::Url::parse("http://localhost/Users/u/Items/Latest").unwrap();
        let mut older = named_media_item("old", "Older");
        older.date_created = Some("2025-01-01T00:00:00Z".to_string());
        let mut newer = named_media_item("new", "Newer");
        newer.date_created = Some("2026-01-01T00:00:00Z".to_string());

        let response = FederatedItems::new(vec![older, newer]).into_response(
            &url,
            Pagination {
                start_index: 0,
                limit: None,
            },
            ResponseShape::Bare,
        );

        assert_eq!(item_ids(&response.into_items()), vec!["new", "old"]);
    }

    #[test]
    fn sorting_defaults_to_sort_name() {
        let url = url::Url::parse("http://localhost/Items").unwrap();
        let items = vec![
            named_media_item("b", "Beta"),
            named_media_item("a", "Alpha"),
        ];

        let response = FederatedItems::new(items).into_response(
            &url,
            Pagination {
                start_index: 0,
                limit: None,
            },
            ResponseShape::Bare,
        );

        assert_eq!(item_ids(&response.into_items()), vec!["a", "b"]);
    }

    #[test]
    fn pagination_slices_after_interleave() {
        let url = url::Url::parse("http://localhost/Items?SortBy=Random").unwrap();
        let response = FederatedItems::interleaved(vec![
            ItemsResponseVariants::Bare(vec![media_item("a1", None), media_item("a2", None)]),
            ItemsResponseVariants::Bare(vec![media_item("b1", None), media_item("b2", None)]),
        ])
        .into_response(
            &url,
            Pagination {
                start_index: 1,
                limit: Some(2),
            },
            ResponseShape::Bare,
        );

        assert_eq!(item_ids(&response.into_items()), vec!["b1", "a2"]);
    }

    #[test]
    fn pagination_returns_rest_when_limit_is_absent() {
        let items = Pagination {
            start_index: 1,
            limit: None,
        }
        .apply(vec![
            media_item("one", None),
            media_item("two", None),
            media_item("three", None),
        ]);

        assert_eq!(item_ids(&items), vec!["two", "three"]);
    }

    #[test]
    fn pagination_returns_empty_when_start_is_out_of_range() {
        let items = Pagination {
            start_index: 2,
            limit: Some(10),
        }
        .apply(vec![media_item("one", None), media_item("two", None)]);

        assert!(items.is_empty());
    }

    #[test]
    fn pagination_allows_zero_limit() {
        let items = Pagination {
            start_index: 0,
            limit: Some(0),
        }
        .apply(vec![media_item("one", None)]);

        assert!(items.is_empty());
    }

    #[test]
    fn interleave_round_robins_uneven_server_lists() {
        let items = FederatedItems::interleaved(vec![
            ItemsResponseVariants::Bare(vec![media_item("a1", None), media_item("a2", None)]),
            ItemsResponseVariants::Bare(vec![
                media_item("b1", None),
                media_item("b2", None),
                media_item("b3", None),
            ]),
            ItemsResponseVariants::Bare(vec![media_item("c1", None)]),
        ]);

        assert_eq!(
            item_ids(&items.items),
            vec!["a1", "b1", "c1", "a2", "b2", "b3"]
        );
    }

    #[test]
    fn interleave_keeps_only_one_live_tv_user_view() {
        let items = FederatedItems::interleaved(vec![
            ItemsResponseVariants::Bare(vec![
                media_item("live-one", Some("livetv")),
                media_item("movie-one", None),
            ]),
            ItemsResponseVariants::Bare(vec![
                media_item("live-two", Some("livetv")),
                media_item("movie-two", None),
            ]),
        ]);

        assert_eq!(
            item_ids(&items.items),
            vec!["live-one", "movie-one", "movie-two"]
        );
    }

    #[test]
    fn interleave_does_not_dedupe_non_user_view_live_tv_items() {
        let items = FederatedItems::interleaved(vec![
            ItemsResponseVariants::Bare(vec![typed_media_item(
                "channel-one",
                "LiveTvChannel",
                Some("livetv"),
            )]),
            ItemsResponseVariants::Bare(vec![typed_media_item(
                "channel-two",
                "LiveTvChannel",
                Some("livetv"),
            )]),
        ]);

        assert_eq!(item_ids(&items.items), vec!["channel-one", "channel-two"]);
    }

    #[test]
    fn response_shape_detects_counted_response() {
        let responses = [
            ItemsResponseVariants::Bare(Vec::new()),
            ItemsResponseVariants::WithCount(ItemsResponseWithCount {
                items: Vec::new(),
                total_record_count: 0,
                start_index: 0,
            }),
        ];

        assert_eq!(
            ResponseShape::from_responses(&responses),
            ResponseShape::Counted
        );
    }

    #[test]
    fn response_shape_is_bare_when_all_responses_are_bare() {
        let responses = [
            ItemsResponseVariants::Bare(Vec::new()),
            ItemsResponseVariants::Bare(Vec::new()),
        ];

        assert_eq!(
            ResponseShape::from_responses(&responses),
            ResponseShape::Bare
        );
    }

    #[test]
    fn pipeline_sorts_then_paginates_with_reported_total() {
        let url = url::Url::parse("http://localhost/Items?SortBy=SortName").unwrap();
        let response = FederatedItems::new(vec![
            named_media_item("c", "Charlie"),
            named_media_item("a", "Alpha"),
            named_media_item("b", "Beta"),
        ])
        .with_reported_total(12)
        .into_response(
            &url,
            Pagination {
                start_index: 1,
                limit: Some(1),
            },
            ResponseShape::Counted,
        );

        let ItemsResponseVariants::WithCount(response) = response else {
            panic!("expected counted response");
        };
        assert_eq!(
            (
                item_ids(&response.items),
                response.total_record_count,
                response.start_index,
            ),
            (vec!["b"], 12, 1)
        );
    }

    #[test]
    fn merge_server_items_applies_duplicate_policy() {
        let policy = DuplicatePolicyConfig {
            policy: DuplicatePolicy::ServerPriority,
            preferred_server_id: None,
        };
        let items = FederatedItems::default().merge_server_items(
            vec![
                ServerItems {
                    response: ItemsResponseVariants::Bare(vec![duplicate_media_item("low")]),
                    server: server(1, 10),
                },
                ServerItems {
                    response: ItemsResponseVariants::Bare(vec![duplicate_media_item("high")]),
                    server: server(2, 20),
                },
            ],
            MergeStrategy::DuplicatePolicy(&policy),
        );

        assert_eq!(item_ids(&items.items), vec!["high"]);
    }

    fn item_ids(items: &[MediaItem]) -> Vec<&str> {
        items.iter().map(|item| item.id.as_str()).collect()
    }

    fn media_item(id: &str, collection_type: Option<&str>) -> MediaItem {
        typed_media_item(id, "UserView", collection_type)
    }

    fn named_media_item(id: &str, name: &str) -> MediaItem {
        serde_json::from_value(json!({
            "Id": id,
            "Name": name,
            "SortName": name,
            "Type": "Movie",
        }))
        .unwrap()
    }

    fn duplicate_media_item(id: &str) -> MediaItem {
        serde_json::from_value(json!({
            "Id": id,
            "Name": "Movie",
            "Type": "Movie",
            "ProviderIds": { "Tmdb": "same" },
        }))
        .unwrap()
    }

    fn typed_media_item(id: &str, item_type: &str, collection_type: Option<&str>) -> MediaItem {
        let mut item = json!({
            "Id": id,
            "Type": item_type,
        });
        if let Some(collection_type) = collection_type {
            item["CollectionType"] = json!(collection_type);
        }
        serde_json::from_value(item).unwrap()
    }

    fn server(id: i64, priority: i32) -> Server {
        let now = chrono::Utc::now();
        Server {
            id: ServerId::new(id),
            name: format!("Server {id}"),
            url: ServerUrl::parse("http://example:8096").unwrap(),
            priority,
            media_streaming_mode: MediaStreamingMode::Redirect,
            created_at: now,
            updated_at: now,
        }
    }
}
