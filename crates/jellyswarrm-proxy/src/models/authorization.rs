use percent_encoding::percent_decode_str;

pub fn generate_token() -> String {
    use uuid::Uuid;
    Uuid::new_v4().simple().to_string()
}

#[derive(Debug, Clone)]
pub struct Authorization {
    pub client: String,
    pub device: String,
    pub device_id: String,
    pub version: String,
    pub token: Option<String>,
}

impl Authorization {
    pub fn parse(header_value: &str) -> Result<Self, String> {
        if !header_value.starts_with("MediaBrowser ") {
            return Err("Invalid authorization header format".to_string());
        }

        let content = &header_value[12..]; // Skip "MediaBrowser "
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

fn parse_quoted_params(content: &str) -> Result<Vec<(String, String)>, String> {
    let mut params = Vec::new();
    let mut chars = content.chars().peekable();

    while chars.peek().is_some() {
        // Skip whitespace and commas
        while let Some(&ch) = chars.peek() {
            if ch.is_whitespace() || ch == ',' {
                chars.next();
            } else {
                break;
            }
        }

        if chars.peek().is_none() {
            break;
        }

        // Parse key
        let mut key = String::new();
        while let Some(&ch) = chars.peek() {
            if ch == '=' {
                chars.next(); // consume '='
                break;
            } else if ch.is_alphanumeric() || ch == '_' {
                key.push(chars.next().unwrap());
            } else {
                return Err(format!("Invalid character in parameter key: {ch}"));
            }
        }

        if key.is_empty() {
            return Err("Empty parameter key".to_string());
        }

        // Parse value (quoted or unquoted)
        let mut value = String::new();

        if chars.peek() == Some(&'"') {
            // Quoted value
            chars.next(); // consume opening quote
            let mut escaped = false;

            for ch in chars.by_ref() {
                if escaped {
                    value.push(ch);
                    escaped = false;
                } else if ch == '\\' {
                    escaped = true;
                } else if ch == '"' {
                    break; // end of quoted value
                } else {
                    value.push(ch);
                }
            }
        } else {
            // Unquoted value - read until comma or end
            while let Some(&ch) = chars.peek() {
                if ch == ',' {
                    break;
                } else {
                    value.push(chars.next().unwrap());
                }
            }
            // Trim trailing whitespace from unquoted values
            value = value.trim_end().to_string();
        }

        params.push((key, value));
    }

    Ok(params)
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
}
