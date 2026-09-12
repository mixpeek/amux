//! Opt-in owner bootstrap using the local Tailscale daemon's device identity.
//! A tailnet address or client-supplied header alone is never a credential.
use serde_json::Value;
use std::net::IpAddr;
use std::time::Duration;
use tokio::process::Command;

fn tailnet_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v) => {
            let b = v.octets();
            b[0] == 100 && b[1] >= 64 && b[1] <= 127
        }
        IpAddr::V6(v) => v.segments()[..3] == [0xfd7a, 0x115c, 0xa1e0],
    }
}
fn node_unexpired(node: &Value) -> bool {
    // tailcfg.Node omits false Expired and zero KeyExpiry. Zero means expiry
    // disabled; an explicit deadline still has to be in the future.
    if node.get("Expired").is_some_and(|v| v != false) {
        return false;
    }
    match node.get("KeyExpiry") {
        None => true,
        Some(Value::String(raw)) if raw == "0001-01-01T00:00:00Z" => true,
        Some(Value::String(raw)) => chrono::DateTime::parse_from_rfc3339(raw)
            .is_ok_and(|deadline| deadline > chrono::Utc::now()),
        Some(_) => false,
    }
}
fn same_owner(status: &Value, who: &Value, peer: IpAddr) -> bool {
    let Some(owner) = status["Self"]["UserID"].as_u64().filter(|id| *id > 0) else {
        return false;
    };
    status["BackendState"] == "Running"
        && status["Self"]["Online"] == true
        && who["UserProfile"]["ID"].as_u64() == Some(owner)
        && who["Node"]["User"].as_u64() == Some(owner)
        && node_unexpired(&who["Node"])
        && who["Node"]["Tags"]
            .as_array()
            .is_none_or(|tags| tags.is_empty())
        && who["Node"]["Addresses"]
            .as_array()
            .is_some_and(|addresses| {
                addresses.iter().any(|address| {
                    address
                        .as_str()
                        .and_then(|s| s.split('/').next())
                        .and_then(|s| s.parse::<IpAddr>().ok())
                        == Some(peer)
                })
            })
}
async fn json_command(binary: &str, args: &[&str]) -> Result<Value, String> {
    let output = tokio::time::timeout(
        Duration::from_secs(3),
        Command::new(binary).args(args).kill_on_drop(true).output(),
    )
    .await
    .map_err(|_| "Tailscale identity lookup exceeded 3s".to_owned())?
    .map_err(|_| "Tailscale identity lookup could not start".to_owned())?;
    if !output.status.success() {
        return Err("Tailscale identity lookup failed".into());
    }
    serde_json::from_slice(&output.stdout)
        .map_err(|_| "Tailscale identity response was invalid".into())
}
pub(super) async fn verified(peer: IpAddr) -> bool {
    if !std::env::var("AMUX_TRUST_TAILNET_OWNER").is_ok_and(|v| v == "1") || !tailnet_ip(peer) {
        return false;
    }
    let Some(binary) = [
        "/usr/local/bin/tailscale",
        "/opt/homebrew/bin/tailscale",
        "/usr/bin/tailscale",
        "/Applications/Tailscale.app/Contents/MacOS/Tailscale",
    ]
    .into_iter()
    .find(|p| std::path::Path::new(p).is_file()) else {
        tracing::warn!(target:"amux::auth",verdict="tailnet_owner_unmeasured",measured=false,n_considered=0,"Automatic owner sign-in enabled but Tailscale is unavailable");
        return false;
    };
    let result = async {
        let status = json_command(binary, &["status", "--json"]).await?;
        let who = json_command(binary, &["whois", "--json", &peer.to_string()]).await?;
        Ok::<bool, String>(same_owner(&status, &who, peer))
    }
    .await;
    match result {
        Ok(allowed) => {
            tracing::info!(target:"amux::auth",verdict="tailnet_owner_checked",measured=true,n_considered=1,allowed,%peer,"Checked remote browser against server Tailscale owner");
            allowed
        }
        Err(reason) => {
            tracing::warn!(target:"amux::auth",verdict="tailnet_owner_unmeasured",measured=false,n_considered=1,%peer,%reason,"Automatic sign-in withheld; ordinary token sign-in remains available");
            false
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn records() -> (Value, Value) {
        (
            json!({"BackendState":"Running","Self":{"UserID":12,"Online":true}}),
            json!({"UserProfile":{"ID":12},"Node":{"User":12,"Expired":false,"Addresses":["100.64.1.2/32"]}}),
        )
    }
    #[test]
    fn owner_requires_matching_daemon_identity_and_peer_address() {
        let (status, mut who) = records();
        let peer = "100.64.1.2".parse().unwrap();
        assert!(same_owner(&status, &who, peer));
        assert!(!same_owner(&status, &who, "100.64.1.3".parse().unwrap()));
        who["UserProfile"]["ID"] = json!(13);
        assert!(!same_owner(&status, &who, peer));
        who["UserProfile"]["ID"] = json!(12);
        who["Node"]["User"] = json!(13);
        assert!(!same_owner(&status, &who, peer));
    }
    #[test]
    fn actual_daemon_omissions_and_key_expiry_are_respected() {
        let (status, mut who) = records();
        let peer = "100.64.1.2".parse().unwrap();
        who["Node"].as_object_mut().unwrap().remove("Expired");
        who["Node"]["KeyExpiry"] =
            json!((chrono::Utc::now() + chrono::Duration::days(1)).to_rfc3339());
        assert!(same_owner(&status, &who, peer));
        who["Node"]["KeyExpiry"] = json!("2020-01-01T00:00:00Z");
        assert!(!same_owner(&status, &who, peer));
        who["Node"]["KeyExpiry"] = json!("malformed");
        assert!(!same_owner(&status, &who, peer));
        who["Node"]["KeyExpiry"] = json!("0001-01-01T00:00:00Z");
        assert!(same_owner(&status, &who, peer));
    }
    #[test]
    fn expired_tagged_missing_and_stopped_identity_fail_closed() {
        let (mut status, mut who) = records();
        let peer = "100.64.1.2".parse().unwrap();
        who["Node"]["Expired"] = json!(true);
        assert!(!same_owner(&status, &who, peer));
        who["Node"]["Expired"] = json!(false);
        who["Node"]["Tags"] = json!(["tag:server"]);
        assert!(!same_owner(&status, &who, peer));
        who["Node"]["Tags"] = json!([]);
        status["BackendState"] = json!("Stopped");
        assert!(!same_owner(&status, &who, peer));
        assert!(!same_owner(&Value::Null, &Value::Null, peer));
        assert!(!tailnet_ip("192.168.1.2".parse().unwrap()));
        assert!(!tailnet_ip("127.0.0.1".parse().unwrap()));
        assert!(tailnet_ip(peer));
        assert!(tailnet_ip("fd7a:115c:a1e0::1".parse().unwrap()));
    }
}
