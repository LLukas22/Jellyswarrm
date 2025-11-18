use percent_encoding::percent_decode_str;


pub fn generate_token() -> String {
    use uuid::Uuid;
    Uuid::new_v4().simple().to_string()
}


// See https://github.com/jellyfin/jellyfin/blob/master/Jellyfin.Server.Implementations/Security/AuthorizationContext.cs
#[derive(Debug, Clone)]
pub struct Authorization {
    pub client: String,
    pub device: String,
    pub device_id: String,
    pub version: String,
    pub token: Option<String>,
}

impl Authorization {
    /// Parse authorization header with support for MediaBrowser and Emby prefixes
    pub fn parse(header_value: &str) -> Result<Self, String> {
        Self::parse_with_legacy(header_value, false)
    }

    /// Parse authorization header with optional legacy Emby support
    pub fn parse_with_legacy(header_value: &str, enable_legacy: bool) -> Result<Self, String> {
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

        if client.is_empty() || device.is_empty() || device_id.is_empty() || version.is_empty() {
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
    pub fn to_header_value(&self) -> String {
        let mut result = format!(
            "MediaBrowser Client=\"{}\", Device=\"{}\", DeviceId=\"{}\", Version=\"{}\"",
            self.client, self.device, self.device_id, self.version
        );

        if let Some(token) = &self.token {
            result.push_str(&format!(", Token=\"{token}\""));
        }

        result
    }

    /// Convert to header value without "MediaBrowser " prefix
    pub fn to_params_string(&self) -> String {
        let mut result = format!(
            "Client=\"{}\", Device=\"{}\", DeviceId=\"{}\", Version=\"{}\"",
            self.client, self.device, self.device_id, self.version
        );

        if let Some(token) = &self.token {
            result.push_str(&format!(", Token=\"{token}\""));
        }

        result
    }

    /// Get a short string representation for logging
    pub fn to_short_string(&self) -> String {
        format!(
            "{} on {} ({})",
            self.client,
            self.device,
            self.token.as_deref().unwrap_or("no token")
        )
    }
}

/// Parse authorization header parameters following C# GetParts logic
/// This mirrors the logic from Jellyfin.Server.Implementations.Security.AuthorizationContext.GetParts
fn parse_quoted_params(content: &str) -> Result<Vec<(String, String)>, String> {
    let mut result = Vec::new();
    let mut escaped = false;
    let mut start = 0;
    let mut key = String::new();
    
    let chars: Vec<char> = content.chars().collect();
    let mut i = 0;
    
    while i < chars.len() {
        let token = chars[i];
        
        if token == '"' || token == ',' {
            // XOR logic to evaluate whether it is opening or closing a value
            let is_quote = token == '"';
            escaped = (!escaped) == is_quote;
            
            if token == ',' && !escaped {
                // Meeting a comma after a closing escape char means the value is complete
                if start < i {
                    let value_str: String = chars[start..i].iter().collect();
                    // Trim quotes only (matching C# Trim('"'))
                    let trimmed = value_str.trim_start_matches(|c: char| c.is_whitespace())
                        .trim_end_matches(|c: char| c.is_whitespace())
                        .trim_matches('"');
                    let decoded = percent_decode_str(trimmed).decode_utf8_lossy().to_string();
                    result.push((key.clone(), decoded));
                    key.clear();
                }
                start = i + 1;
            }
        } else if !escaped && token == '=' {
            let key_str: String = chars[start..i].iter().collect();
            key = key_str.trim().to_string();
            start = i + 1;
        }
        
        i += 1;
    }
    
    // Add last value
    if start < chars.len() {
        let value_str: String = chars[start..].iter().collect();
        // Trim quotes only (matching C# Trim('"'))
        let trimmed = value_str.trim_start_matches(|c: char| c.is_whitespace())
            .trim_end_matches(|c: char| c.is_whitespace())
            .trim_matches('"');
        let decoded = percent_decode_str(trimmed).decode_utf8_lossy().to_string();
        result.push((key, decoded));
    }
    
    Ok(result)
}

impl std::fmt::Display for Authorization {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "MediaBrowser Client=\"{}\", Device=\"{}\", DeviceId=\"{}\", Version=\"{}\"",
            self.client, self.device, self.device_id, self.version
        )?;

        if let Some(token) = &self.token {
            write!(f, ", Token=\"{token}\"")?;
        }

        Ok(())
    }
}

// Usage example:
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_authorization() {
        let header = r#"MediaBrowser Client="Jellyfin Web", Device="Firefox", DeviceId="TW96aWxsYS81LjAgKFgxMTsgTGludXggeDg2XzY0OyBydjoxNDAuMCkgR2Vja28vMjAxMDAxMDEgRmlyZWZveC8xNDAuMHwxNzUyMDcwMzk0MDky", Version="10.10.7", Token="6fbe3193155f45b3bc3f229469db1568""#;

        let auth = Authorization::parse(header).unwrap();

        assert_eq!(auth.client, "Jellyfin Web");
        assert_eq!(auth.device, "Firefox");
        assert_eq!(auth.device_id, "TW96aWxsYS81LjAgKFgxMTsgTGludXggeDg2XzY0OyBydjoxNDAuMCkgR2Vja28vMjAxMDAxMDEgRmlyZWZveC8xNDAuMHwxNzUyMDcwMzk0MDky");
        assert_eq!(auth.version, "10.10.7");
        assert_eq!(
            auth.token,
            Some("6fbe3193155f45b3bc3f229469db1568".to_string())
        );
    }

    #[test]
    fn test_parse_ios_authorization() {
        let header = r#"MediaBrowser Device=iPad, Version=1.3.1, DeviceId=iPadOS_20C7AC61-1C80-4621-B2C3-2B043490A254, Token=, Client=Swiftfin iPadOS"#;

        let auth = Authorization::parse(header).unwrap();

        assert_eq!(auth.client, "Swiftfin iPadOS");
        assert_eq!(auth.device, "iPad");
        assert_eq!(
            auth.device_id,
            "iPadOS_20C7AC61-1C80-4621-B2C3-2B043490A254"
        );
        assert_eq!(auth.version, "1.3.1");
        assert_eq!(auth.token, None);
    }

    #[test]
    fn test_parse_emby_authorization() {
        let header = r#"MediaBrowser Client="Switchfin", Device="System Product Name", DeviceId="725a281e0b7b4ce38a19b5f8b38122d9", Version="0.7.4"#;

        let auth = Authorization::parse(header).unwrap();

        assert_eq!(auth.client, "Switchfin");
        assert_eq!(auth.device, "System Product Name");
        assert_eq!(auth.device_id, "725a281e0b7b4ce38a19b5f8b38122d9");
        assert_eq!(auth.version, "0.7.4");
        assert_eq!(auth.token, None);
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
    fn test_parse_url_encoded_values() {
        // Test URL encoding in device name
        let header = r#"MediaBrowser Client="Test", Device="My%20Device%20Name", DeviceId="test123", Version="1.0""#;
        
        let auth = Authorization::parse(header).unwrap();
        assert_eq!(auth.device, "My Device Name");
    }

    #[test]
    fn test_parse_unquoted_values() {
        // iOS style with unquoted values
        let header = r#"MediaBrowser Device=iPad, Version=1.3.1, DeviceId=device123, Token=, Client=Test Client"#;
        
        let auth = Authorization::parse(header).unwrap();
        assert_eq!(auth.device, "iPad");
        assert_eq!(auth.version, "1.3.1");
        assert_eq!(auth.device_id, "device123");
        assert_eq!(auth.client, "Test Client");
        assert_eq!(auth.token, None);
    }

    #[test]
    fn test_parse_mixed_quoted_unquoted() {
        let header = r#"MediaBrowser Client="Jellyfin Web", Device=Firefox, DeviceId="abc123", Version=1.0.0"#;
        
        let auth = Authorization::parse(header).unwrap();
        assert_eq!(auth.client, "Jellyfin Web");
        assert_eq!(auth.device, "Firefox");
        assert_eq!(auth.device_id, "abc123");
        assert_eq!(auth.version, "1.0.0");
    }

    #[test]
    fn test_parse_with_spaces() {
        // Test with extra spaces around values
        let header = r#"MediaBrowser Client = "Jellyfin Web" , Device = "Firefox" , DeviceId = "abc123" , Version = "1.0.0""#;
        
        let auth = Authorization::parse(header).unwrap();
        assert_eq!(auth.client, "Jellyfin Web");
        assert_eq!(auth.device, "Firefox");
        assert_eq!(auth.device_id, "abc123");
        assert_eq!(auth.version, "1.0.0");
    }

    #[test]
    fn test_parse_empty_token() {
        let header = r#"MediaBrowser Client="Test", Device="Dev", DeviceId="123", Version="1.0", Token="""#;
        
        let auth = Authorization::parse(header).unwrap();
        assert_eq!(auth.token, None);
    }

    #[test]
    fn test_parse_missing_required_field() {
        // Missing Version
        let header = r#"MediaBrowser Client="Test", Device="Dev", DeviceId="123""#;
        assert!(Authorization::parse(header).is_err());
        
        // Missing Client
        let header = r#"MediaBrowser Device="Dev", DeviceId="123", Version="1.0""#;
        assert!(Authorization::parse(header).is_err());
        
        // Missing Device
        let header = r#"MediaBrowser Client="Test", DeviceId="123", Version="1.0""#;
        assert!(Authorization::parse(header).is_err());
        
        // Missing DeviceId
        let header = r#"MediaBrowser Client="Test", Device="Dev", Version="1.0""#;
        assert!(Authorization::parse(header).is_err());
    }

    #[test]
    fn test_parse_invalid_prefix() {
        let header = r#"Bearer token=abc123"#;
        assert!(Authorization::parse(header).is_err());
        
        let header = r#"Basic YWxhZGRpbjpvcGVuc2VzYW1l"#;
        assert!(Authorization::parse(header).is_err());
    }

    #[test]
    fn test_parse_no_prefix() {
        let header = r#"Client="Test", Device="Dev", DeviceId="123", Version="1.0""#;
        assert!(Authorization::parse(header).is_err());
    }

    #[test]
    fn test_parse_chromecast_client() {
        // Test case that might be shared with casting device
        let header = r#"MediaBrowser Client="Jellyfin Chromecast", Device="Living Room TV", DeviceId="cast123", Version="1.0.0", Token="shared_token""#;
        
        let auth = Authorization::parse(header).unwrap();
        assert_eq!(auth.client, "Jellyfin Chromecast");
        assert_eq!(auth.device, "Living Room TV");
        assert!(auth.client.to_lowercase().contains("chromecast"));
    }

    #[test]
    fn test_parse_special_characters_in_device() {
        let header = r#"MediaBrowser Client="Test", Device="John's iPad (2024)", DeviceId="123", Version="1.0""#;
        
        let auth = Authorization::parse(header).unwrap();
        assert_eq!(auth.device, "John's iPad (2024)");
    }

    #[test]
    fn test_parse_long_device_id() {
        // Real-world Firefox device ID from your test
        let header = r#"MediaBrowser Client="Jellyfin Web", Device="Firefox", DeviceId="TW96aWxsYS81LjAgKFgxMTsgTGludXggeDg2XzY0OyBydjoxNDAuMCkgR2Vja28vMjAxMDAxMDEgRmlyZWZveC8xNDAuMHwxNzUyMDcwMzk0MDky", Version="10.10.7""#;
        
        let auth = Authorization::parse(header).unwrap();
        assert_eq!(auth.device_id, "TW96aWxsYS81LjAgKFgxMTsgTGludXggeDg2XzY0OyBydjoxNDAuMCkgR2Vja28vMjAxMDAxMDEgRmlyZWZveC8xNDAuMHwxNzUyMDcwMzk0MDky");
    }

    #[test]
    fn test_parse_with_commas_in_quoted_value() {
        // Commas inside quoted values should be preserved
        let header = r#"MediaBrowser Client="Test, Client", Device="Dev, Device", DeviceId="123", Version="1.0""#;
        
        let auth = Authorization::parse(header).unwrap();
        assert_eq!(auth.client, "Test, Client");
        assert_eq!(auth.device, "Dev, Device");
    }

    #[test]
    fn test_to_header_value() {
        let auth = Authorization {
            client: "Jellyfin Web".to_string(),
            device: "Firefox".to_string(),
            device_id: "abc123".to_string(),
            version: "10.10.7".to_string(),
            token: Some("test_token".to_string()),
        };
        
        let header = auth.to_header_value();
        assert!(header.starts_with("MediaBrowser"));
        assert!(header.contains(r#"Client="Jellyfin Web""#));
        assert!(header.contains(r#"Device="Firefox""#));
        assert!(header.contains(r#"Token="test_token""#));
        
        // Verify it can be parsed back
        let parsed = Authorization::parse(&header).unwrap();
        assert_eq!(parsed.client, auth.client);
        assert_eq!(parsed.device, auth.device);
        assert_eq!(parsed.token, auth.token);
    }

    #[test]
    fn test_to_header_value_without_token() {
        let auth = Authorization {
            client: "Test".to_string(),
            device: "Dev".to_string(),
            device_id: "123".to_string(),
            version: "1.0".to_string(),
            token: None,
        };
        
        let header = auth.to_header_value();
        assert!(!header.contains("Token="));
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
        assert_eq!(short, "Jellyfin Web on Firefox (test_token)");
        
        let auth_no_token = Authorization {
            token: None,
            ..auth
        };
        let short = auth_no_token.to_short_string();
        assert_eq!(short, "Jellyfin Web on Firefox (no token)");
    }

    #[test]
    fn test_roundtrip_parsing() {
        let original_headers = vec![
            r#"MediaBrowser Client="Jellyfin Web", Device="Firefox", DeviceId="abc123", Version="10.10.7", Token="test123""#,
            r#"MediaBrowser Client="Test", Device="Dev", DeviceId="123", Version="1.0""#,
            r#"MediaBrowser Device=iPad, Version=1.3.1, DeviceId=iPadOS_test, Token=, Client=Swiftfin iPadOS"#,
        ];
        
        for header in original_headers {
            let auth = Authorization::parse(header).unwrap();
            let regenerated = auth.to_header_value();
            let reparsed = Authorization::parse(&regenerated).unwrap();
            
            assert_eq!(auth.client, reparsed.client);
            assert_eq!(auth.device, reparsed.device);
            assert_eq!(auth.device_id, reparsed.device_id);
            assert_eq!(auth.version, reparsed.version);
            assert_eq!(auth.token, reparsed.token);
        }
    }

    #[test]
    fn test_parse_unknown_parameters_ignored() {
        // Unknown parameters should be ignored
        let header = r#"MediaBrowser Client="Test", Device="Dev", DeviceId="123", Version="1.0", UnknownParam="ignored", AnotherParam="also ignored""#;
        
        let auth = Authorization::parse(header).unwrap();
        assert_eq!(auth.client, "Test");
        assert_eq!(auth.device, "Dev");
        assert_eq!(auth.device_id, "123");
        assert_eq!(auth.version, "1.0");
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
        assert_eq!(display, auth.to_header_value());
    }
}
