//! A Mixpeek instance the owner brings (BYO Mixpeek), local or hosted.
//!
//! The Photos tab treats one Mixpeek NAMESPACE as the boundary of a media
//! library. This client knows where the instance is, the namespace, and the
//! API key, and nothing about any one deployment: the endpoint is whatever the
//! owner entered (a local Docker Compose stack, a hosted API, a private
//! deployment). Ingestion and search will be added to this client; today it
//! validates a connection.
use super::email::HttpTransport;
use serde_json::Value;

#[derive(Clone, Debug, PartialEq)]
pub struct MixpeekClient {
    /// Base URL with no trailing slash and no `/v1`, e.g. `https://api.mixpeek.com`.
    pub endpoint: String,
    /// Optional: a local instance may run without auth.
    pub api_key: Option<String>,
    pub namespace: String,
}

/// What a check of the connection found. `Connected` carries the namespace's
/// own name when the instance returned one.
#[derive(Clone, Debug, PartialEq)]
pub enum Check {
    Connected { namespace_name: Option<String> },
    /// 401/403: the instance answered and refused the key.
    KeyRejected { http: u16 },
    /// 404: the instance answered and has no such namespace (or the endpoint
    /// is not a Mixpeek API).
    NamespaceNotFound,
    /// The instance answered with something else.
    Error { http: u16, detail: String },
    /// No answer: wrong address, the instance is down, or the network.
    Unreachable { detail: String },
}

impl Check {
    pub fn status(&self) -> &'static str {
        match self {
            Check::Connected { .. } => "connected",
            Check::KeyRejected { .. } => "key_rejected",
            Check::NamespaceNotFound => "namespace_not_found",
            Check::Error { .. } => "error",
            Check::Unreachable { .. } => "unreachable",
        }
    }
    /// One sentence for the person who entered the details.
    pub fn message(&self) -> String {
        match self {
            Check::Connected { namespace_name: Some(n) } => format!("Connected to {n}."),
            Check::Connected { namespace_name: None } => "Connected.".into(),
            Check::KeyRejected { .. } => "The instance refused this API key.".into(),
            Check::NamespaceNotFound => {
                "The instance has no namespace with that ID, or this address is not a Mixpeek API.".into()
            }
            Check::Error { http, .. } => format!("The instance answered with an error (HTTP {http})."),
            Check::Unreachable { .. } => "Could not reach the instance at that address.".into(),
        }
    }
}

/// `https://x.y/` and `https://x.y/v1` both mean `https://x.y`. Only http(s).
pub fn normalize_endpoint(raw: &str) -> Result<String, String> {
    let mut e = raw.trim().trim_end_matches('/').to_string();
    if let Some(stripped) = e.strip_suffix("/v1") {
        e = stripped.trim_end_matches('/').to_string();
    }
    let rest = e
        .strip_prefix("https://")
        .or_else(|| e.strip_prefix("http://"))
        .ok_or_else(|| "The endpoint must start with http:// or https://".to_string())?;
    if rest.is_empty() || rest.contains(char::is_whitespace) {
        return Err("The endpoint is not a valid address".into());
    }
    Ok(e)
}

/// Namespace ids and names are path segments: letters, digits, `_`, `-`, `.`.
pub fn valid_namespace(ns: &str) -> bool {
    !ns.is_empty() && ns.len() <= 128 && ns.bytes().all(|b| b.is_ascii_alphanumeric() || b"_-.".contains(&b))
}

impl MixpeekClient {
    fn url(&self, path: &str) -> String {
        format!("{}/v1{path}", self.endpoint)
    }

    /// Can this key read this namespace? `GET /v1/namespaces/{id}`.
    pub async fn check(&self, http: &dyn HttpTransport) -> Check {
        let url = self.url(&format!("/namespaces/{}", self.namespace));
        match http.get(&url, self.api_key.as_deref().filter(|k| !k.is_empty())).await {
            Ok((st, body)) if (200..300).contains(&st) => Check::Connected {
                namespace_name: body
                    .get("namespace_name")
                    .or_else(|| body.get("name"))
                    .and_then(Value::as_str)
                    .map(str::to_string),
            },
            Ok((401 | 403, _)) => Check::KeyRejected { http: 401 },
            Ok((404, _)) => Check::NamespaceNotFound,
            Ok((st, body)) => Check::Error {
                http: st,
                detail: body.to_string().chars().take(200).collect(),
            },
            Err(e) => Check::Unreachable { detail: e.chars().take(200).collect() },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoints_normalize_and_bad_ones_are_refused() {
        assert_eq!(normalize_endpoint(" https://api.mixpeek.com/ ").unwrap(), "https://api.mixpeek.com");
        assert_eq!(normalize_endpoint("http://studio.local:8000/v1/").unwrap(), "http://studio.local:8000");
        assert!(normalize_endpoint("api.mixpeek.com").is_err());
        assert!(normalize_endpoint("ftp://x").is_err());
        assert!(normalize_endpoint("https://").is_err());
        assert!(valid_namespace("ns_8f2a-photos.v2"));
        assert!(!valid_namespace("../x"));
        assert!(!valid_namespace(""));
    }
}
