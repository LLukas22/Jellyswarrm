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

pub(crate) fn is_android_tv_client(client: &str) -> bool {
    normalize_device(client).contains("android tv")
}

impl Device {
    /// Match known device IDs strictly, falling back to names only for unknown IDs.
    pub(crate) fn matches(&self, other: &Device) -> bool {
        let self_client = normalize_device(&self.client);
        let other_client = normalize_device(&other.client);
        if self_client != other_client {
            return false;
        }

        let self_device_id = normalize_device(&self.device_id);
        let other_device_id = normalize_device(&other.device_id);
        let short_self_device_id = &self_device_id[..self_device_id.len().min(16)];
        let short_other_device_id = &other_device_id[..other_device_id.len().min(16)];
        let self_has_known_device_id = Self::has_known_device_id(&self_device_id)
            || Self::has_known_device_id(short_self_device_id);
        let other_has_known_device_id = Self::has_known_device_id(&other_device_id)
            || Self::has_known_device_id(short_other_device_id);

        if self_has_known_device_id && other_has_known_device_id {
            if self_device_id == other_device_id {
                return true;
            }
            // Web IDs encode user-agent + timestamp. Their common Mozilla prefix
            // is not identity: require an exact ID instead of prefix matching.
            const WEB_DEVICE_ID_PREFIX: &str = "tw96awxs";
            if self_device_id.starts_with(WEB_DEVICE_ID_PREFIX)
                || other_device_id.starts_with(WEB_DEVICE_ID_PREFIX)
            {
                return false;
            }
            return short_self_device_id == short_other_device_id;
        }
        let self_device = normalize_device(&self.device);
        let other_device = normalize_device(&other.device);
        !self_device.is_empty() && self_device == other_device
    }

    pub(crate) fn has_known_device_id(device_id: &str) -> bool {
        !device_id.is_empty()
            && device_id != "unknown-device-id"
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
