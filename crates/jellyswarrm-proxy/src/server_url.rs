use std::ops::Deref;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use url::Url;

#[derive(Debug, Clone, Eq, PartialEq, Hash)]
pub struct ServerUrl {
    url: Url,
    canonical: String,
}

impl ServerUrl {
    pub fn parse(input: &str) -> Result<Self, url::ParseError> {
        let canonical = Self::canonicalize(input)?;
        let url = Url::parse(&canonical)?;
        Ok(Self { url, canonical })
    }

    pub fn canonicalize(input: &str) -> Result<String, url::ParseError> {
        let mut url = Url::parse(input.trim())?;
        url.set_fragment(None);
        url.set_query(None);

        let mut canonical = url.to_string();
        while canonical.ends_with('/') {
            canonical.pop();
        }

        Ok(canonical)
    }

    pub fn as_str(&self) -> &str {
        &self.canonical
    }

    pub fn as_url(&self) -> &Url {
        &self.url
    }
}

impl AsRef<str> for ServerUrl {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl Deref for ServerUrl {
    type Target = Url;

    fn deref(&self) -> &Self::Target {
        &self.url
    }
}

impl Serialize for ServerUrl {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for ServerUrl {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(&value).map_err(serde::de::Error::custom)
    }
}

impl std::fmt::Display for ServerUrl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonicalize_removes_trailing_slashes_query_and_fragment() {
        let url = ServerUrl::parse(" https://Example.COM/jellyfin///?foo=bar#frag ").unwrap();

        assert_eq!(url.as_str(), "https://example.com/jellyfin");
    }

    #[test]
    fn canonicalize_keeps_path_case() {
        let url = ServerUrl::parse("https://example.com/Jellyfin/").unwrap();

        assert_eq!(url.as_str(), "https://example.com/Jellyfin");
    }
}
