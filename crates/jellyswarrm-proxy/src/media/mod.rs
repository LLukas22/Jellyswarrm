//! Media identity, grouping and presentation rules, with persisted inventory and mappings.

pub(crate) mod grouping;
pub(crate) mod identity;
pub(crate) mod presentation;
pub(crate) mod scope;
pub(crate) mod storage;

pub(crate) use storage::MediaStorageService;
