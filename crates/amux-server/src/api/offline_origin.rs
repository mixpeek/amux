//! Service-worker guidance from loaded TLS state; browser trust is unmeasured.
use axum::http::HeaderMap;
use axum::Json;
use serde_json::{json, Value};

pub async fn offline_origin(headers: HeaderMap) -> Json<Value> {
    let tls = crate::tls::connection_status(&crate::config::ServerConfig::from_process_env().tls_dir());
    let ts = tls["tailscale"]["hostname"].as_str().unwrap_or("");
    let proxied = headers.get("x-forwarded-proto").and_then(|v|v.to_str().ok())
        .is_some_and(|v|v.eq_ignore_ascii_case("https"));
    let mut value = json!({"tailscale_hostname":ts,
        "good_origin": if ts.is_empty() { String::new() } else { format!("https://{ts}:{}", crate::config::canonical_port()) },
        "trusted_cert":null, "trust":"unknown_to_server", "measured":tls["measured"],
        "n_considered":tls["n_considered"],
        "why":"Service worker registration failed. Settings → Connect → Connection & security shows the loaded certificate and repair actions. Only the browser/OS can determine trust."});
    if proxied {
        value["proxied"] = json!(true);
        value["good_origin"] = json!("");
        value["why"] = json!("TLS may be terminated by a proxy. The server cannot measure that certificate or browser trust; inspect the browser security information or contact the proxy operator.");
    }
    Json(value)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn offline_origin_does_not_infer_trust_from_disk_or_forwarded_header() {
        for proto in [None, Some("https"), Some("HTTPS")] {
            let mut h=HeaderMap::new();
            if let Some(proto)=proto {h.insert("x-forwarded-proto",proto.parse().unwrap());}
            let v=futures::executor::block_on(offline_origin(h)).0;
            assert!(v["trusted_cert"].is_null());
            assert_eq!(v["trust"],"unknown_to_server");
            assert_eq!(v.get("proxied").is_some(),proto.is_some());
        }
    }
}
