"""Hetzner Cloud and Cloudflare clients (docs/design/cloud-self-service.md, D1, D8, section 3).

Standard library only. fakes.FakeInfra has the same methods for tests.
Every server we create carries the label amux-cloud=1; lifecycle never touches
a server without it.
"""
import hashlib, json, os, time, urllib.error, urllib.request

from lifecycle import ProviderError, LABEL
from fakes import CapacityError

HCLOUD = "https://api.hetzner.cloud/v1"
CF = "https://api.cloudflare.com/client/v4"


def _call(url, token, method="GET", body=None, timeout=30, auth="Bearer"):
    data = json.dumps(body).encode() if body is not None else None
    headers = {"Content-Type": "application/json"}
    if auth == "Bearer":
        headers["Authorization"] = f"Bearer {token}"
    req = urllib.request.Request(url, data=data, method=method, headers=headers)
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            raw = r.read()
            return json.loads(raw) if raw else {}
    except urllib.error.HTTPError as e:
        text = e.read().decode(errors="replace")[:600]
        try:
            err = json.loads(text)
        except ValueError:
            err = {}
        code = ((err.get("error") or {}).get("code")) if isinstance(err.get("error"), dict) else None
        if code in ("resource_unavailable", "placement_error"):
            raise CapacityError(text)
        if e.code >= 500 or e.code == 429:
            raise ProviderError(f"{url.split('/')[2]} {e.code}: {text}")
        raise InfraError(e.code, text)
    except (urllib.error.URLError, TimeoutError, OSError) as e:
        raise ProviderError(f"{url.split('/')[2]} unreachable: {e}")


class InfraError(Exception):
    def __init__(self, code, body):
        super().__init__(f"{code}: {body}")
        self.code = code


class Infra:
    def __init__(self, hcloud_token, cf_token, cf_zone_id, domain="amux.cloud"):
        self.h, self.cf, self.zone, self.domain = hcloud_token, cf_token, cf_zone_id, domain

    # ---------------------------------------------------------- Hetzner
    def healthy(self):
        try:
            _call(f"{HCLOUD}/locations", self.h, timeout=10)
            return True
        except Exception:
            return False

    def list_servers(self, label_selector=None):
        out, page = [], 1
        while True:
            q = f"?page={page}&per_page=50" + (f"&label_selector={label_selector}" if label_selector else "")
            r = _call(f"{HCLOUD}/servers{q}", self.h)
            for s in r["servers"]:
                out.append({"id": s["id"], "name": s["name"], "labels": s.get("labels") or {},
                            "location": s["datacenter"]["location"]["name"], "server_type": s["server_type"]["name"],
                            "ipv4": ((s.get("public_net") or {}).get("ipv4") or {}).get("ip"), "status": s["status"]})
            nxt = ((r.get("meta") or {}).get("pagination") or {}).get("next_page")
            if not nxt:
                return out
            page = nxt

    def create_server(self, name, server_type, location, image, labels, user_data):
        labels = dict(labels, **{LABEL: "1"})
        r = _call(f"{HCLOUD}/servers", self.h, "POST", {
            "name": name, "server_type": server_type, "location": location, "image": image,
            "labels": labels, "user_data": user_data, "start_after_create": True,
            "public_net": {"enable_ipv4": True, "enable_ipv6": True},
        })
        s = r["server"]
        if s.get("id") and s.get("server_type", {}).get("name") == server_type:
            _call(f"{HCLOUD}/servers/{s['id']}/actions/enable_backup", self.h, "POST", {})
        return {"id": s["id"], "name": s["name"], "labels": s.get("labels") or labels, "location": location,
                "server_type": server_type, "ipv4": s["public_net"]["ipv4"]["ip"]}

    def create_snapshot(self, server_id, description):
        r = _call(f"{HCLOUD}/servers/{server_id}/actions/create_image", self.h, "POST",
                  {"type": "snapshot", "description": description, "labels": {LABEL: "1"}})
        return r["image"]["id"]

    def get_image(self, image_id):
        try:
            img = _call(f"{HCLOUD}/images/{int(image_id)}", self.h)["image"]
        except InfraError as e:
            if e.code == 404:
                return None
            raise
        return {"id": img["id"], "status": img["status"], "image_size": img.get("image_size")}

    def delete_server(self, server_id):
        _call(f"{HCLOUD}/servers/{server_id}", self.h, "DELETE")

    def delete_image(self, image_id):
        _call(f"{HCLOUD}/images/{int(image_id)}", self.h, "DELETE")

    def offered_locations(self, server_type):
        """Locations offering the type, from the pricing API, for capacity fallback (J7)."""
        r = _call(f"{HCLOUD}/pricing", self.h)["pricing"]
        for st in r["server_types"]:
            if st["name"] == server_type:
                return [p["location"] for p in st["prices"]]
        return []

    # ---------------------------------------------------------- Cloudflare
    def create_dns(self, name, ipv4):
        r = _call(f"{CF}/zones/{self.zone}/dns_records", self.cf, "POST",
                  {"type": "A", "name": f"{name}.{self.domain}", "content": ipv4, "proxied": True, "ttl": 1,
                   "comment": "amux-cloud"})
        return r["result"]["id"]

    def delete_dns(self, record_id):
        try:
            _call(f"{CF}/zones/{self.zone}/dns_records/{record_id}", self.cf, "DELETE")
        except InfraError as e:
            if e.code != 404:
                raise

    def backup_bucket(self, slug, server_id):
        """An R2 bucket for this server alone, and S3 credentials that can touch only that bucket.

        Needs CF_ACCOUNT_ID and a control-plane token allowed to create R2 buckets and user API
        tokens. The customer has root on their server, so per-server scoping is what keeps one
        customer's backups unreadable to another.
        """
        acct = os.environ["CF_ACCOUNT_ID"]
        bucket = f"amux-{slug}-{server_id[-6:]}".lower()
        try:
            _call(f"{CF}/accounts/{acct}/r2/buckets", self.cf, "POST", {"name": bucket, "locationHint": "weur"})
        except InfraError as e:
            if e.code != 409:
                raise
        groups = _call(f"{CF}/user/tokens/permission_groups", self.cf)["result"]
        want = {"Workers R2 Storage Bucket Item Write", "Workers R2 Storage Bucket Item Read"}
        ids = [{"id": g["id"]} for g in groups if g["name"] in want]
        if len(ids) != 2:
            raise InfraError(500, f"R2 permission groups not found: {sorted(want)}")
        tok = _call(f"{CF}/user/tokens", self.cf, "POST", {
            "name": f"amux-backup-{bucket}",
            "policies": [{"effect": "allow", "permission_groups": ids,
                          "resources": {f"com.cloudflare.edge.r2.bucket.{acct}_default_{bucket}": "*"}}]})["result"]
        return {"endpoint": f"https://{acct}.r2.cloudflarestorage.com", "bucket": bucket,
                "access_key_id": tok["id"], "secret_access_key": hashlib.sha256(tok["value"].encode()).hexdigest()}

    def _wait_action(self, action, timeout=900):
        deadline = time.time() + timeout
        while time.time() < deadline:
            a = _call(f"{HCLOUD}/actions/{action['id']}", self.h)["action"]
            if a["status"] == "success":
                return a
            if a["status"] == "error":
                raise InfraError(500, f"{a['command']} failed: {a.get('error')}")
            time.sleep(5)
        raise InfraError(504, f"{action['command']} did not finish in {timeout}s")

    def resize(self, server_id, server_type):
        """J15: shut down, change type with the larger disk, power on. A failed change powers the old type back on."""
        self._wait_action(_call(f"{HCLOUD}/servers/{server_id}/actions/shutdown", self.h, "POST", {})["action"])
        try:
            self._wait_action(_call(f"{HCLOUD}/servers/{server_id}/actions/change_type", self.h, "POST",
                                    {"server_type": server_type, "upgrade_disk": True})["action"])
        finally:
            self._wait_action(_call(f"{HCLOUD}/servers/{server_id}/actions/poweron", self.h, "POST", {})["action"])

    def pricing(self):
        r = _call(f"{HCLOUD}/pricing", self.h)["pricing"]
        return {st["name"]: {p["location"]: float(p["price_monthly"]["net"]) for p in st["prices"]} for st in r["server_types"]}

    def isolate(self, server_id, allow_ips):
        """M17: a firewall whose only outbound rule is HTTPS to the control plane. Hetzner denies all
        other outbound traffic once a firewall has any outbound rule. Inbound SSH stays closed too."""
        name = f"amux-isolate-{server_id}"
        rules = [{"direction": "out", "protocol": "tcp", "port": "443",
                  "destination_ips": [ip if "/" in ip else ip + "/32" for ip in allow_ips]},
                 {"direction": "out", "protocol": "udp", "port": "53", "destination_ips": ["0.0.0.0/0", "::/0"]}]
        existing = _call(f"{HCLOUD}/firewalls?name={name}", self.h)["firewalls"]
        fw = existing[0]["id"] if existing else _call(f"{HCLOUD}/firewalls", self.h, "POST", {
            "name": name, "labels": {LABEL: "1", "purpose": "abuse-isolation"}, "rules": rules})["firewall"]["id"]
        _call(f"{HCLOUD}/firewalls/{fw}/actions/apply_to_resources", self.h, "POST",
              {"apply_to": [{"type": "server", "server": {"id": server_id}}]})
        return fw

    def unisolate(self, server_id):
        for fw in _call(f"{HCLOUD}/firewalls?name=amux-isolate-{server_id}", self.h)["firewalls"]:
            _call(f"{HCLOUD}/firewalls/{fw['id']}/actions/remove_from_resources", self.h, "POST",
                  {"remove_from": [{"type": "server", "server": {"id": server_id}}]})

    def delete_bucket(self, bucket):
        """Empties nothing itself: R2 refuses to delete a non-empty bucket, so objects go first."""
        acct = os.environ["CF_ACCOUNT_ID"]
        try:
            _call(f"{CF}/accounts/{acct}/r2/buckets/{bucket}", self.cf, "DELETE")
        except InfraError as e:
            if e.code != 404:
                raise

    def origin_certificate(self, hostname, csr_pem, days=5475):
        """Cloudflare Origin CA certificate for one host. The server made the key and sends only the CSR."""
        r = _call(f"{CF}/certificates", self.cf, "POST", {
            "hostnames": [f"{hostname}.{self.domain}"], "requested_validity": days,
            "request_type": "origin-ecc", "csr": csr_pem})
        return r["result"]["certificate"]
