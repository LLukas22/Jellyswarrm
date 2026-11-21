//! Device metadata parsing and matching without authentication or storage access.
#[derive(Debug, Clone)]
pub(crate) struct Device {
    pub(crate) client: String,
    pub(crate) device: String,
    pub(crate) device_id: String,
    pub(crate) version: String,
}

pub(crate) fn normalize_device(value: &str) -> String {
    value.trim().to_lowercase().replace("+", " ")
}

impl Device {
    pub(crate) fn has_known_device_id(device_id: &str) -> bool {
        !device_id.is_empty()
            && device_id != "unknown-device-id"
            && device_id != "unknown device id"
            && device_id != "unknown"
            && device_id != "n/a"
    }

    pub(crate) fn from_useragent(user_agent: &str) -> Self {
        let (client, version, device) = Self::parse_user_agent(user_agent);
        Device {
            client,
            device,
            device_id: "unknown-device-id".to_string(),
            version,
        }
    }

    fn parse_user_agent(user_agent: &str) -> (String, String, String) {
        let user_agent = user_agent.trim();
        if let Some(captures) = regex::Regex::new(r"^([^/]+)/([^\s\(]+)\s*\(([^)]+)\)")
            .ok()
            .and_then(|re| re.captures(user_agent))
        {
            let device_info = captures.get(3).map_or("Unknown".to_string(), |m| {
                let device_str = m.as_str();
                if device_str.contains("Windows") {
                    "Windows".to_string()
                } else if device_str.contains("Mac") || device_str.contains("Darwin") {
                    "macOS".to_string()
                } else if device_str.contains("Linux") && !device_str.contains("Android") {
                    "Linux".to_string()
                } else if device_str.contains("Android") {
                    "Android".to_string()
                } else if device_str.contains("iPhone")
                    || device_str.contains("iPad")
                    || device_str.contains("iOS")
                {
                    "iOS".to_string()
                } else {
                    device_str.to_string()
                }
            });
            return (
                captures
                    .get(1)
                    .map_or("Unknown".to_string(), |m| m.as_str().to_string()),
                captures
                    .get(2)
                    .map_or("0.0.0".to_string(), |m| m.as_str().to_string()),
                device_info,
            );
        }
        if let Some(captures) = regex::Regex::new(r"^([^/]+)/([^\s]+)")
            .ok()
            .and_then(|re| re.captures(user_agent))
        {
            return (
                captures
                    .get(1)
                    .map_or("Unknown".to_string(), |m| m.as_str().to_string()),
                captures
                    .get(2)
                    .map_or("0.0.0".to_string(), |m| m.as_str().to_string()),
                "Unknown".to_string(),
            );
        }
        (
            user_agent.to_string(),
            "0.0.0".to_string(),
            "Unknown".to_string(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholder_device_ids_are_not_known_identifiers() {
        for id in [
            "",
            "unknown-device-id",
            "unknown",
            "n/a",
            "Unknown Device id",
            " UNKNOWN+DEVICE+ID ",
        ] {
            assert!(!Device::has_known_device_id(&normalize_device(id)), "{id}");
        }
        assert!(Device::has_known_device_id("kodi-device"));
    }
}
