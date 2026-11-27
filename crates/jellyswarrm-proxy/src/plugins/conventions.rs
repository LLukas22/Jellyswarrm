//! The ID rules translation applies: the core lists plus every compiled-in
//! plugin's [`Conventions`]. The plugin set is fixed per build, so the merged
//! value is computed once and is the same in production and tests.
use std::sync::LazyLock;

use jellyswarrm_plugin_api::{Conventions, Plugin};

use crate::{
    protocol::routes::{CORE_MEDIA_ID_PATH_TAGS, CORE_USER_ID_PATH_TAGS},
    translation::field_matcher::{
        FieldMatcher, CORE_MEDIA_ID_ARRAY_FIELDS, CORE_MEDIA_ID_MAP_KEY_FIELDS,
    },
};

pub(crate) struct ActiveConventions {
    /// Path segments followed by a media ID, core entries first.
    pub(crate) media_id_path_tags: Vec<&'static str>,
    /// Path segments followed by a user ID, core entries first.
    pub(crate) user_id_path_tags: Vec<&'static str>,
    pub(crate) media_id_array_fields: FieldMatcher,
    pub(crate) media_id_map_key_fields: FieldMatcher,
}

static ACTIVE: LazyLock<ActiveConventions> =
    LazyLock::new(|| ActiveConventions::merge(super::PLUGINS));

/// The merged ID rules for this build.
pub(crate) fn conventions() -> &'static ActiveConventions {
    &ACTIVE
}

impl ActiveConventions {
    fn merge(plugins: &[&dyn Plugin]) -> Self {
        let contributed = || plugins.iter().map(|plugin| plugin.conventions());
        let list = |core: &'static [&'static str],
                    field: fn(&'static Conventions) -> &'static [&'static str]| {
            union(core, contributed().flat_map(field).copied())
        };
        Self {
            media_id_path_tags: list(CORE_MEDIA_ID_PATH_TAGS, |c| c.media_id_path_tags),
            user_id_path_tags: list(CORE_USER_ID_PATH_TAGS, |c| c.user_id_path_tags),
            media_id_array_fields: FieldMatcher::new(&list(CORE_MEDIA_ID_ARRAY_FIELDS, |c| {
                c.media_id_array_fields
            })),
            media_id_map_key_fields: FieldMatcher::new(&list(CORE_MEDIA_ID_MAP_KEY_FIELDS, |c| {
                c.media_id_map_key_fields
            })),
        }
    }
}

/// `core` in order, then each new entry once; matching is case-insensitive.
fn union(
    core: &'static [&'static str],
    extra: impl Iterator<Item = &'static str>,
) -> Vec<&'static str> {
    let mut merged = core.to_vec();
    for entry in extra {
        if !merged.iter().any(|known| known.eq_ignore_ascii_case(entry)) {
            merged.push(entry);
        }
    }
    merged
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture;
    impl Plugin for Fixture {
        fn id(&self) -> &'static str {
            "fixture"
        }
        fn name(&self) -> &'static str {
            "Fixture"
        }
        fn conventions(&self) -> &'static Conventions {
            &Conventions {
                media_id_path_tags: &["PluginItems", "items"],
                user_id_path_tags: &["PluginUsers"],
                media_id_array_fields: &["PluginIds"],
                media_id_map_key_fields: &["PluginMap"],
            }
        }
    }

    #[test]
    fn plugin_rules_extend_core_lists_without_duplicates() {
        let merged = ActiveConventions::merge(&[&Fixture]);
        assert_eq!(
            merged.media_id_path_tags[..CORE_MEDIA_ID_PATH_TAGS.len()],
            *CORE_MEDIA_ID_PATH_TAGS
        );
        assert_eq!(
            merged.media_id_path_tags[CORE_MEDIA_ID_PATH_TAGS.len()..],
            ["PluginItems"],
            "`items` duplicates the core `Items` tag"
        );
        assert!(merged.user_id_path_tags.contains(&"PluginUsers"));
        assert!(merged.media_id_array_fields.contains("pluginids"));
        assert!(merged.media_id_map_key_fields.contains("PluginMap"));
    }

    #[test]
    fn without_plugins_the_core_lists_apply_unchanged() {
        let merged = ActiveConventions::merge(&[]);
        assert_eq!(merged.media_id_path_tags, CORE_MEDIA_ID_PATH_TAGS);
        assert_eq!(merged.user_id_path_tags, CORE_USER_ID_PATH_TAGS);
        for field in CORE_MEDIA_ID_ARRAY_FIELDS {
            assert!(merged.media_id_array_fields.contains(field));
        }
        assert!(!merged.media_id_map_key_fields.contains("PluginMap"));
    }
}
