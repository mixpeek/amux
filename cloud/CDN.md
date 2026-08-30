# amux CDN — `cdn.amux.io` (public object storage)

A public, cached CDN subdomain for amux static content: diagrams, media assets,
downloads, anything a lane wants to upload once and serve by URL. This is the
CloudFront-equivalent on the stack amux already runs (Cloudflare R2 + Cloudflare's
edge cache), not GCS — amux cloud already uses R2 (the gateway carries `R2_ACCESS_KEY`,
`R2_SECRET_KEY`, `CF_ACCOUNT_ID`), so R2 is the native choice.

Status: **design ready, provisioning blocked on one access grant** (see the bottom).
Tracked on board card AC-370.

## Architecture

```
upload (S3 API, R2 creds)                     serve (public, cached)
  aws s3 cp file  s3://amux-cdn/path   ─────▶  https://cdn.amux.io/path
        │                                            ▲
        ▼                                            │
  Cloudflare R2 bucket  "amux-cdn"  ──── custom domain (Cloudflare edge cache) ──┘
```

- **Bucket:** one R2 bucket, `amux-cdn`, in the amux Cloudflare account.
- **Public serving:** bind the custom domain `cdn.amux.io` to the bucket (R2 →
  Settings → Custom Domains). Cloudflare then serves objects at
  `https://cdn.amux.io/<key>` over HTTPS, cached at the edge — that caching layer IS
  the CloudFront-equivalent. The DNS record for `cdn.amux.io` is created automatically
  by the R2 custom-domain binding (amux.io is on Cloudflare, zone `9818ed31…`).
- **Access control:** the bucket is public-read on the served prefix only. Uploads use
  the R2 S3-API credentials (`R2_ACCESS_KEY` / `R2_SECRET_KEY`, already on the cloud
  host and in the deploy secrets); the public has read-only, no listing.

## Uploading content

Once provisioned, any lane uploads with the S3-compatible API (R2 endpoint):

```bash
# endpoint is https://<CF_ACCOUNT_ID>.r2.cloudflarestorage.com
aws s3 cp ./diagram.png \
  s3://amux-cdn/media/diagram.png \
  --endpoint-url "https://${CF_ACCOUNT_ID}.r2.cloudflarestorage.com"
# now public at:
#   https://cdn.amux.io/media/diagram.png
```

Or with wrangler:

```bash
wrangler r2 object put amux-cdn/media/diagram.png --file ./diagram.png
```

Reference the object anywhere as `https://cdn.amux.io/<key>`. Cloudflare caches it at
the edge; bust the cache by uploading to a new key (content-hash the filename) rather
than relying on invalidation.

Guidance: keep this for PUBLIC assets only (it is world-readable). Anything private
stays in the per-workspace volumes or a private bucket. Do not upload secrets or
customer data.

## Provisioning (blocked on access)

The bucket + custom-domain binding are one-time account-level Cloudflare operations.
The DNS half is already reachable (a zone-scoped token can create the record), but
creating the R2 bucket and binding the public custom domain needs **account/R2 admin**
access that the current token lacks. To unblock, grant either:

- **(recommended) a Cloudflare API token with R2 admin scope** (Account → Workers R2
  Storage: Edit) for the amux account — then this is built entirely on amux's own
  stack; or
- GCP `storage.admin` + `compute.loadBalancerAdmin` on `mixpeek-inference-463103` for
  the GCS + Cloud CDN variant instead.

With either grant, the setup is: create `amux-cdn`, bind `cdn.amux.io`, upload a test
object, confirm `https://cdn.amux.io/<key>` serves it cached, and this doc's status
flips to live.
