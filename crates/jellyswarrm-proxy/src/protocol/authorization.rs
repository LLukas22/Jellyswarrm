//! Parse and render Jellyfin authorization headers and their client/device metadata.
use percent_encoding::percent_decode_str;
use std::fmt;

// See https://github.com/jellyfin/jellyfin/blob/master/Jellyfin.Server.Implementations/Security/AuthorizationContext.cs
#[derive(Clone)]
pub(crate) struct Authorization {
    pub(crate) client: String,
    pub(crate) device: String,
    pub(crate) device_id: String,
    pub(crate) version: String,
    pub(crate) token: Option<String>,
}

impl Authorization {
    /// Parse authorization header with support for MediaBrowser and Emby prefixes
    pub(crate) fn parse(header_value: &str) -> Result<Self, String> {
        Self::parse_with_legacy(header_value, false)
    }

    /// Parse authorization header with optional legacy Emby support
    pub(crate) fn parse_with_legacy(
        header_value: &str,
        enable_legacy: bool,
    ) -> Result<Self, String> {
        Self::parse_internal(header_value, enable_legacy, false)
    }

    /// Authenticated requests may omit metadata available in the stored session.
    /// Missing metadata is represented by empty strings; login parsing stays strict.
    pub(crate) fn parse_request(header_value: &str) -> Result<Self, String> {
        Self::parse_internal(header_value, false, true)
    }

    pub(crate) fn has_complete_metadata(&self) -> bool {
        !self.client.is_empty()
            && !self.device.is_empty()
            && !self.device_id.is_empty()
            && !self.version.is_empty()
    }

    fn parse_internal(
        header_value: &str,
        enable_legacy: bool,
        allow_partial: bool,
    ) -> Result<Self, String> {
        let content = if header_value.starts_with("MediaBrowser ") {
            &header_value[12..] // Skip "MediaBrowser "
        } else if enable_legacy && header_value.starts_with("Emby ") {
            &header_value[5..] // Skip "Emby "
        } else {
            return Err("Invalid authorization header format".to_string());
        };

        let mut client = String::new();
        let mut device = String::new();
        let mut device_id = String::new();
        let mut version = String::new();
        let mut token = None;

        let parts = parse_quoted_params(content)?;

        for (key, value) in parts {
            match key.as_str() {
                "Client" => client = percent_decode_str(&value).decode_utf8_lossy().to_string(),
                "Device" => device = percent_decode_str(&value).decode_utf8_lossy().to_string(),
                "DeviceId" => device_id = value,
                "Version" => version = value,
                "Token" => {
                    if value.is_empty() {
                        token = None;
                    } else {
                        token = Some(value);
                    }
                }
                _ => {} // Ignore unknown parameters
            }
        }

        if (!allow_partial || token.is_none())
            && (client.is_empty()
                || device.is_empty()
                || device_id.is_empty()
                || version.is_empty())
        {
            return Err("Missing required authorization parameters".to_string());
        }

        Ok(Authorization {
            client,
            device,
            device_id,
            version,
            token,
        })
    }

    /// Convert to MediaBrowser authorization header string
    pub(crate) fn to_header_value(&self) -> String {
        let mut result = format!(
            "MediaBrowser Client=\"{}\", Device=\"{}\", DeviceId=\"{}\", Version=\"{}\"",
            self.client, self.device, self.device_id, self.version
        );

        if let Some(token) = &self.token {
            result.push_str(&format!(", Token=\"{token}\""));
        }

        result
    }

    /// Convert to a log-safe authorization header string with secrets redacted.
    pub(crate) fn to_redacted_header_value(&self) -> String {
        let mut result = format!(
            "MediaBrowser Client=\"{}\", Device=\"{}\", DeviceId=\"{}\", Version=\"{}\"",
            self.client, self.device, self.device_id, self.version
        );

        if self.token.is_some() {
            result.push_str(", Token=\"<redacted>\"");
        }

        result
    }

    /// Get a short string representation for logging
    #[cfg(test)]
    pub(crate) fn to_short_string(&self) -> String {
        format!(
            "{} on {} ({})",
            self.client,
            self.device,
            if self.token.is_some() {
                "token present"
            } else {
                "no token"
            }
        )
    }
}

impl fmt::Debug for Authorization {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Authorization")
            .field("client", &self.client)
            .field("device", &self.device)
            .field("device_id", &self.device_id)
            .field("version", &self.version)
            .field("token", &self.token.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

/// Parse authorization header parameters following C# GetParts logic
/// This mirrors the logic from Jellyfin.Server.Implementations.Security.AuthorizationContext.GetParts
fn parse_quoted_params(content: &str) -> Result<Vec<(String, String)>, String> {
    let mut result = Vec::new();
    let mut in_quotes = false;
    let mut start = 0;
    let mut key = String::new();

    for (i, ch) in content.char_indices() {
        match ch {
            '"' => in_quotes = !in_quotes,
            ',' if !in_quotes => {
                if start < i && !key.is_empty() {
                    let value = decode_value(&content[start..i]);
                    result.push((std::mem::take(&mut key), value));
                }
                start = i + 1;
            }
            '=' if !in_quotes => {
                key = content[start..i].trim().to_string();
                start = i + 1;
            }
            _ => {}
        }
    }

    // Add last value
    if start < content.len() && !key.is_empty() {
        let value = decode_value(&content[start..]);
        result.push((key, value));
    }

    Ok(result)
}

/// Decode and clean a parameter value
#[inline]
fn decode_value(raw: &str) -> String {
    let trimmed = raw.trim().trim_matches('"');
    percent_decode_str(trimmed).decode_utf8_lossy().into_owned()
}

impl fmt::Display for Authorization {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_redacted_header_value())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_headers_are_only_accepted_for_authenticated_requests() {
        for header in [
            r#"MediaBrowser DeviceId="device", Token="proxy""#,
            r#"MediaBrowser Token="proxy""#,
        ] {
            let auth = Authorization::parse_request(header).unwrap();
            assert_eq!(auth.token.as_deref(), Some("proxy"));
            assert!(!auth.has_complete_metadata());
            assert!(Authorization::parse(header).is_err());
        }
        assert!(
            Authorization::parse_request(r#"MediaBrowser DeviceId="device", Token="""#).is_err()
        );
        assert!(Authorization::parse_request("Bearer proxy").is_err());
        assert!(Authorization::parse_request(r#"Emby DeviceId="device", Token="proxy""#).is_err());
    }

    #[test]
    fn header_variants_preserve_fields_and_round_trip() {
        let long_device_id = "TW96aWxsYS81LjAgKFgxMTsgTGludXggeDg2XzY0OyBydjoxNDAuMCkgR2Vja28vMjAxMDAxMDEgRmlyZWZveC8xNDAuMHwxNzUyMDcwMzk0MDky";
        for (case, header, metadata, token) in [
            (
                "quoted with long device id",
                r#"MediaBrowser Client="Jellyfin Web", Device="Firefox", DeviceId="TW96aWxsYS81LjAgKFgxMTsgTGludXggeDg2XzY0OyBydjoxNDAuMCkgR2Vja28vMjAxMDAxMDEgRmlyZWZveC8xNDAuMHwxNzUyMDcwMzk0MDky", Version="10.10.7", Token="test123""#,
                ["Jellyfin Web", "Firefox", long_device_id, "10.10.7"],
                Some("test123"),
            ),
            (
                "unquoted reordered iOS fields and empty token",
                r#"MediaBrowser Device=iPad, Version=1.3.1, DeviceId=iPadOS_test, Token=, Client=Swiftfin iPadOS"#,
                ["Swiftfin iPadOS", "iPad", "iPadOS_test", "1.3.1"],
                None,
            ),
            (
                "Switchfin missing closing quote and omitted token",
                r#"MediaBrowser Client="Switchfin", Device="System Product Name", DeviceId="725a281e0b7b4ce38a19b5f8b38122d9", Version="0.7.4"#,
                [
                    "Switchfin",
                    "System Product Name",
                    "725a281e0b7b4ce38a19b5f8b38122d9",
                    "0.7.4",
                ],
                None,
            ),
            (
                "URL encoded values",
                r#"MediaBrowser Client="Test", Device="My%20Device%20Name", DeviceId="123", Version="1.0""#,
                ["Test", "My Device Name", "123", "1.0"],
                None,
            ),
            (
                "mixed quoting",
                r#"MediaBrowser Client="Jellyfin Web", Device=Firefox, DeviceId="abc123", Version=1.0.0"#,
                ["Jellyfin Web", "Firefox", "abc123", "1.0.0"],
                None,
            ),
            (
                "whitespace",
                r#"MediaBrowser Client = "Jellyfin Web" , Device = "Firefox" , DeviceId = "abc123" , Version = "1.0.0""#,
                ["Jellyfin Web", "Firefox", "abc123", "1.0.0"],
                None,
            ),
            (
                "quoted empty token",
                r#"MediaBrowser Client="Test", Device="Dev", DeviceId="123", Version="1.0", Token="""#,
                ["Test", "Dev", "123", "1.0"],
                None,
            ),
            (
                "special characters",
                r#"MediaBrowser Client="Test", Device="John's iPad (2024)", DeviceId="123", Version="1.0""#,
                ["Test", "John's iPad (2024)", "123", "1.0"],
                None,
            ),
            (
                "quoted commas",
                r#"MediaBrowser Client="Test, Client", Device="Dev, Device", DeviceId="123", Version="1.0""#,
                ["Test, Client", "Dev, Device", "123", "1.0"],
                None,
            ),
            (
                "unknown fields",
                r#"MediaBrowser Client="Test", Device="Dev", DeviceId="123", Version="1.0", UnknownParam="ignored", AnotherParam="also ignored""#,
                ["Test", "Dev", "123", "1.0"],
                None,
            ),
        ] {
            let auth =
                Authorization::parse(header).unwrap_or_else(|error| panic!("{case}: {error}"));
            let regenerated = auth.to_header_value();
            let reparsed = Authorization::parse(&regenerated).unwrap();
            for parsed in [&auth, &reparsed] {
                assert_eq!(
                    [
                        parsed.client.as_str(),
                        parsed.device.as_str(),
                        parsed.device_id.as_str(),
                        parsed.version.as_str()
                    ],
                    metadata,
                    "{case}"
                );
                assert_eq!(parsed.token.as_deref(), token, "{case}");
            }
        }
    }

    #[test]
    fn test_parse_legacy_emby_header() {
        let header = r#"Emby Client="Emby Theater", Device="PC", DeviceId="abc123", Version="3.0.0", Token="test_token""#;

        // Should fail without legacy enabled
        assert!(Authorization::parse(header).is_err());

        // Should succeed with legacy enabled
        let auth = Authorization::parse_with_legacy(header, true).unwrap();
        assert_eq!(auth.client, "Emby Theater");
        assert_eq!(auth.device, "PC");
        assert_eq!(auth.device_id, "abc123");
        assert_eq!(auth.version, "3.0.0");
        assert_eq!(auth.token, Some("test_token".to_string()));
    }

    #[test]
    fn login_headers_reject_missing_metadata_and_invalid_prefixes() {
        for (case, header) in [
            (
                "missing version",
                r#"MediaBrowser Client="Test", Device="Dev", DeviceId="123""#,
            ),
            (
                "missing client",
                r#"MediaBrowser Device="Dev", DeviceId="123", Version="1.0""#,
            ),
            (
                "missing device",
                r#"MediaBrowser Client="Test", DeviceId="123", Version="1.0""#,
            ),
            (
                "missing device id",
                r#"MediaBrowser Client="Test", Device="Dev", Version="1.0""#,
            ),
            ("bearer", "Bearer token=abc123"),
            ("basic", "Basic YWxhZGRpbjpvcGVuc2VzYW1l"),
            (
                "missing prefix",
                r#"Client="Test", Device="Dev", DeviceId="123", Version="1.0""#,
            ),
        ] {
            assert!(Authorization::parse(header).is_err(), "{case}");
        }
    }

    #[test]
    fn header_serialization_uses_canonical_format_and_omits_absent_token() {
        for (token, suffix) in [(Some("test_token"), r#", Token="test_token""#), (None, "")] {
            let auth = Authorization {
                client: "Test".into(),
                device: "Dev".into(),
                device_id: "123".into(),
                version: "1.0".into(),
                token: token.map(str::to_owned),
            };
            assert_eq!(
                auth.to_header_value(),
                format!(
                    r#"MediaBrowser Client="Test", Device="Dev", DeviceId="123", Version="1.0"{suffix}"#
                )
            );
        }
    }

    #[test]
    fn test_to_short_string() {
        let auth = Authorization {
            client: "Jellyfin Web".to_string(),
            device: "Firefox".to_string(),
            device_id: "abc123".to_string(),
            version: "10.10.7".to_string(),
            token: Some("test_token".to_string()),
        };

        let short = auth.to_short_string();
        assert_eq!(short, "Jellyfin Web on Firefox (token present)");

        let auth_no_token = Authorization {
            token: None,
            ..auth
        };
        let short = auth_no_token.to_short_string();
        assert_eq!(short, "Jellyfin Web on Firefox (no token)");
    }

    #[test]
    fn test_display_trait() {
        let auth = Authorization {
            client: "Test".to_string(),
            device: "Dev".to_string(),
            device_id: "123".to_string(),
            version: "1.0".to_string(),
            token: Some("abc".to_string()),
        };

        let display = format!("{}", auth);
        assert_eq!(display, auth.to_redacted_header_value());
        assert!(!display.contains("abc"));
    }
}
