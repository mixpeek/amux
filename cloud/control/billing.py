"""Stripe billing module (docs/design/cloud-self-service.md, D11 and section 3).

Every payment call in the control plane goes through StripeBilling; tests use
fakes.FakeBilling with the same methods. Standard library only.
"""
import hashlib, hmac, json, time, urllib.error, urllib.parse, urllib.request

from lifecycle import ProviderError

API = "https://api.stripe.com/v1"


def _flatten(params, prefix=""):
    """Stripe's form encoding: {"a": {"b": 1}, "c": [x]} -> a[b]=1, c[0]=x."""
    out = []
    items = params.items() if isinstance(params, dict) else enumerate(params)
    for k, v in items:
        key = f"{prefix}[{k}]" if prefix else str(k)
        if isinstance(v, (dict, list, tuple)):
            out += _flatten(v, key)
        elif v is None:
            continue
        elif isinstance(v, bool):
            out.append((key, "true" if v else "false"))
        else:
            out.append((key, str(v)))
    return out


class StripeBilling:
    def __init__(self, secret_key, plans, timeout=20):
        if not secret_key:
            raise ValueError("Stripe secret key missing")
        self.key = secret_key
        self.plans = {p["id"]: p for p in plans["plans"]}
        self.timeout = timeout
        self._price_ids = {}
        self._health = (0.0, False)

    # ---------------------------------------------------------- transport
    def _req(self, method, path, params=None, idempotency_key=None):
        data = None
        url = f"{API}{path}"
        if params and method == "GET":
            url += "?" + urllib.parse.urlencode(_flatten(params))
        elif params:
            data = urllib.parse.urlencode(_flatten(params)).encode()
        headers = {"Authorization": f"Bearer {self.key}"}
        if idempotency_key:
            headers["Idempotency-Key"] = idempotency_key
        req = urllib.request.Request(url, data=data, method=method, headers=headers)
        try:
            with urllib.request.urlopen(req, timeout=self.timeout) as r:
                return json.load(r)
        except urllib.error.HTTPError as e:
            body = e.read().decode(errors="replace")[:500]
            if e.code >= 500 or e.code == 429:
                raise ProviderError(f"stripe {e.code}: {body}")
            raise StripeError(e.code, body)
        except (urllib.error.URLError, TimeoutError, OSError) as e:
            raise ProviderError(f"stripe unreachable: {e}")

    # ---------------------------------------------------------- health
    def healthy(self):
        ts, ok = self._health
        if time.time() - ts < 60:
            return ok
        try:
            self._req("GET", "/prices", {"limit": 1})
            ok = True
        except Exception:
            ok = False
        self._health = (time.time(), ok)
        return ok

    # ---------------------------------------------------------- prices
    def price_id(self, plan_id):
        if plan_id not in self._price_ids:
            key = self.plans[plan_id]["stripe_lookup_key"]
            got = self._req("GET", "/prices", {"lookup_keys": [key], "active": "true"})["data"]
            if not got:
                raise StripeError(404, f"no active price with lookup key {key}")
            self._price_ids[plan_id] = got[0]["id"]
        return self._price_ids[plan_id]

    # ---------------------------------------------------------- checkout
    def create_checkout(self, plan_id, signup_id, source, success_url, cancel_url, customer_email=None,
                        customer_id=None):
        params = {
            "mode": "subscription",
            "line_items": [{"price": self.price_id(plan_id), "quantity": 1}],
            "success_url": success_url, "cancel_url": cancel_url,
            "client_reference_id": signup_id,
            "automatic_tax": {"enabled": True},
            "tax_id_collection": {"enabled": True},
            "billing_address_collection": "auto",
            "allow_promotion_codes": True,
            "metadata": {"signup_id": signup_id, "plan": plan_id, "source": source or ""},
            "subscription_data": {"metadata": {"signup_id": signup_id, "plan": plan_id}},
        }
        if customer_id:
            params["customer"] = customer_id
            params["customer_update"] = {"address": "auto", "name": "auto"}
        elif customer_email:
            params["customer_email"] = customer_email
        return self._req("POST", "/checkout/sessions", params, idempotency_key=f"checkout:{signup_id}")

    def get_checkout_session(self, session_id):
        return self._req("GET", f"/checkout/sessions/{session_id}", {"expand": ["subscription", "customer"]})

    def portal_url(self, customer_id, return_url):
        return self._req("POST", "/billing_portal/sessions", {"customer": customer_id, "return_url": return_url})["url"]

    # ---------------------------------------------------------- subscriptions
    def get_subscription(self, sub_id):
        s = self._req("GET", f"/subscriptions/{sub_id}")
        return normalize_subscription(s)

    def cancel_now(self, sub_id):
        """Account deletion: end the subscription now, not at period end."""
        try:
            return normalize_subscription(self._req("DELETE", f"/subscriptions/{sub_id}"))
        except StripeError as e:
            if e.code == 404:
                return {"id": sub_id, "status": "canceled"}
            raise

    def cancel_at_period_end(self, sub_id):
        return normalize_subscription(self._req("POST", f"/subscriptions/{sub_id}", {"cancel_at_period_end": True}))

    # ---------------------------------------------------------- money back
    def refund(self, payment_intent, amount_cents, reason):
        return self._req("POST", "/refunds", {"payment_intent": payment_intent, "amount": amount_cents,
                                              "reason": "requested_by_customer", "metadata": {"why": reason}},
                         idempotency_key=f"refund:{payment_intent}:{amount_cents}")

    def submit_dispute_evidence(self, dispute_id, evidence):
        return self._req("POST", f"/disputes/{dispute_id}", {"evidence": evidence, "submit": True})

    def close_dispute(self, dispute_id):
        return self._req("POST", f"/disputes/{dispute_id}/close")


class StripeError(Exception):
    def __init__(self, code, body):
        super().__init__(f"stripe {code}: {body}")
        self.code = code


def normalize_subscription(s):
    """Stripe moved current_period_end onto subscription items in newer API versions; read either."""
    end = s.get("current_period_end")
    if end is None:
        items = (s.get("items") or {}).get("data") or []
        end = items[0].get("current_period_end") if items else None
    plan = ((s.get("metadata") or {}).get("plan")) or None
    items = (s.get("items") or {}).get("data") or []
    lookup = ((items[0].get("price") or {}).get("lookup_key")) if items else None
    return {"id": s["id"], "status": s["status"], "current_period_end": end,
            "cancel_at_period_end": bool(s.get("cancel_at_period_end")), "customer": s.get("customer"),
            "plan": plan, "price_lookup_key": lookup}


def verify_webhook(payload, sig_header, secret, tolerance=300, now=None):
    """Stripe-Signature: t=...,v1=... over "t.payload" with HMAC-SHA256. Returns the parsed event."""
    if not secret:
        raise ValueError("webhook secret missing")
    parts = dict(p.split("=", 1) for p in (sig_header or "").split(",") if "=" in p)
    ts = parts.get("t")
    sigs = [v for k, v in (p.split("=", 1) for p in (sig_header or "").split(",") if "=" in p) if k == "v1"]
    if not ts or not sigs:
        raise ValueError("missing signature")
    if abs((now or time.time()) - int(ts)) > tolerance:
        raise ValueError("signature timestamp outside tolerance")
    expected = hmac.new(secret.encode(), f"{ts}.".encode() + payload, hashlib.sha256).hexdigest()
    if not any(hmac.compare_digest(expected, s) for s in sigs):
        raise ValueError("signature mismatch")
    return json.loads(payload)


def sign_webhook(payload, secret, ts=None):
    """Test helper: produce a valid Stripe-Signature header for payload."""
    ts = int(ts or time.time())
    sig = hmac.new(secret.encode(), f"{ts}.".encode() + payload, hashlib.sha256).hexdigest()
    return f"t={ts},v1={sig}"
