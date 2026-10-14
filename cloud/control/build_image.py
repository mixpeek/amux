#!/usr/bin/env python3
"""Build the amux Cloud golden image (docs/design/cloud-self-service.md, section 3 provisioning, D9).

Creates a short-lived cx33 from Ubuntu 24.04, runs bootstrap.sh with
AMUX_IMAGE_BUILD=1 at a pinned git ref, waits for it to power off, snapshots it
with the label amux-golden=<ref>, deletes the builder and prints the image id to
put in AMUX_GOLDEN_IMAGE. One build costs about one server-hour (about $0.02).

  build_image.py --ref <git sha> [--location fsn1] [--dry-run]
Env: HCLOUD_TOKEN
"""
import argparse, json, os, sys, time, urllib.request

HCLOUD = "https://api.hetzner.cloud/v1"
REPO = "https://github.com/mixpeek/amux.git"


def user_data(ref):
    return f"""#cloud-config
runcmd:
  - [bash, -c, "set -e; git clone -q {REPO} /root/amux && cd /root/amux && git checkout -q {ref} && AMUX_IMAGE_BUILD=1 bash cloud/desktop/bootstrap.sh > /var/log/amux-image-build.log 2>&1 && echo IMAGE_OK >> /var/log/amux-image-build.log; poweroff"]
"""


def call(token, method, path, body=None):
    req = urllib.request.Request(HCLOUD + path, method=method, data=json.dumps(body).encode() if body else None,
                                 headers={"Authorization": f"Bearer {token}", "Content-Type": "application/json"})
    with urllib.request.urlopen(req, timeout=60) as r:
        raw = r.read()
        return json.loads(raw) if raw else {}


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--ref", required=True, help="git sha of the amux checkout to bake")
    ap.add_argument("--location", default="fsn1")
    ap.add_argument("--server-type", default="cx33")
    ap.add_argument("--dry-run", action="store_true")
    a = ap.parse_args()
    name = f"amux-golden-build-{a.ref[:8]}"
    plan = {"create": {"name": name, "server_type": a.server_type, "location": a.location, "image": "ubuntu-24.04",
                       "labels": {"amux-golden-build": "1"}},
            "then": ["wait for status off (bootstrap powers off when done, up to 45 min)",
                     f"snapshot with labels amux-golden={a.ref[:12]}", "delete the builder server"],
            "user_data": user_data(a.ref)}
    if a.dry_run:
        print(json.dumps(plan, indent=1))
        return 0
    token = os.environ["HCLOUD_TOKEN"]
    srv = call(token, "POST", "/servers", dict(plan["create"], user_data=plan["user_data"]))["server"]
    sid = srv["id"]
    print(f"builder {sid} created; waiting for bootstrap to finish and power off", flush=True)
    try:
        deadline = time.time() + 45 * 60
        while time.time() < deadline:
            status = call(token, "GET", f"/servers/{sid}")["server"]["status"]
            if status == "off":
                break
            time.sleep(20)
        else:
            raise SystemExit(f"builder {sid} did not power off in 45 minutes; left for inspection")
        img = call(token, "POST", f"/servers/{sid}/actions/create_image",
                   {"type": "snapshot", "description": f"amux golden {a.ref[:12]}",
                    "labels": {"amux-golden": a.ref[:12]}})["image"]
        while call(token, "GET", f"/images/{img['id']}")["image"]["status"] != "available":
            time.sleep(10)
    except SystemExit:
        raise
    call(token, "DELETE", f"/servers/{sid}")
    print(json.dumps({"golden_image": img["id"], "ref": a.ref}))
    return 0


if __name__ == "__main__":
    sys.exit(main())
