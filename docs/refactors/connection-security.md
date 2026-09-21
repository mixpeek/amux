# Connection and security (AAB-9)

Settings → Connect and the existing instance list lead to one Connection & security
panel. The locked-out banner opens the same panel. Token/key drafts remain only in
the open DOM after errors; successful submission clears them. Connection lists,
migration, sync and export keep only a name and credential-free HTTPS origin.
Security mutations use the existing direct interaction transport and are excluded
at the shared outbox predicate: no automatic replay after a lost acknowledgement.

`POST /api/connection/session` exchanges an explicitly supplied JSON `token` for the
existing `__Host-amux_owner` HttpOnly/Secure/SameSite session, followed by the existing
`/api/_clear_sw` bootstrap. This new flow never places the token in a URL. It requires
an exact same HTTPS Origin/Host and rejects cross-site Fetch Metadata, query strings,
missing tokens and bad tokens even on loopback or when a cookie already exists.
Existing CLI/locality admission and legacy bearer bootstrap are unchanged. Token
rotation takes effect through the existing server configuration/restart semantics;
a session derived from the old token then fails. A scoped member without the owner
credential cannot install certificates or exchange its member session for ownership.

`GET /api/connection/security` reports authentication state and the actual loaded
TLS listener's fallback fingerprint, subject/issuer/dates (when measured), plus its
loaded Tailscale SNI hostname/fingerprint. It returns no key, token or private path.
Browser trust is **unknown to the server**, including behind a TLS proxy. The
`/api/offline-origin` guidance now uses this loaded state instead of inferring a
trusted certificate from filenames or a forwarded header.

`POST /api/connection/certificate` requires the same origin check and an existing
owner session or owner bearer, without locality/worker/member bypass. JSON contains
`certificate_pem` and `private_key_pem`; uploads are bounded to 64 KiB/32 KiB.
The shared rustls loader checks certificate parsing and public/private-key identity;
rustls verifies the requested hostname. A bounded, shell-free OpenSSL invocation
reads public certificate dates/issuer only and refuses expired/future leaves.
OpenSSL is a server prerequisite for this upload; failure is explicit, not a trust
fallback. This does not establish a trusted CA chain in any client trust store.

The validated public chain and normalized private key are written to one mode-0600
`tls/connection.pem` temporary file and atomically renamed in the configured Amux
home. Validation/save failure cannot replace the active resolver; no automatic
restart or hot reload occurs. Until operator restart, status distinguishes loaded
and saved fingerprints and reports a pending change. On restart, this bundle takes
precedence over legacy `cert.pem`/`key.pem`; invalid authoritative material fails
startup rather than silently generating another certificate. Existing Tailscale
`.crt`/`.key` SNI selection remains separate and requires a usable matching pair.

## First access when HTTPS is blocked

On the server machine, `http://localhost:<port>/` or `/connection-setup` serves only
static setup instructions; `/connection-certificate.pem` downloads the **loaded
public fallback certificate**, never its key. Both the socket peer and Host must be
local. This is not a plaintext dashboard, sign-in or upload API. Other HTTP paths
keep the existing redirect/OAuth behavior. HTTP request-head reading is bounded.

The operator must establish OS/browser trust explicitly. For example, with an
existing trusted mkcert CA, generate a server leaf for `localhost 127.0.0.1 ::1`.
Once HTTPS is accessible, upload the server leaf/key in Connect and arrange a restart.
If the app cannot load at all, the local help explains how the operator can inspect
or explicitly trust the downloaded current leaf, or atomically install a valid
server certificate/key bundle in the **correct private Amux home** and restart.
Never upload/share the CA private key. No `ignoreSSL`, disabled TLS, silent trust-store
change, remote plaintext owner access, or second authentication system is provided.

## Evidence and remaining validation

This patch was authored through native worker `amux-astra-bootstrap`, task AAB-9
(normal allocation; the initially suggested AAB-4 was already historical). Scope is
`codex/amux-project-lifecycle` and private test service only. No hooks, project
orchestration, real project state, certificates, OS trust, providers or deployments
were changed. Parent owns Rust tests, actual desktop/mobile UI checks and trusted
localhost setup. Rust parsing and JS/TS syntax are not runtime proof.

Focused tests: `connection_security_` (actual routes/DB/member session),
`connection_certificate_` (atomic save, key/name/date refusal, loaded-vs-saved state,
restart and Tailscale), `connection_setup_` (real acceptor plus local/remote controls),
and `offline_origin_does_not_infer_trust_from_disk_or_forwarded_header`.
The full-page browser fixture is `e2e/connection-security.spec.ts`, including locked
out/member/bad-token/lost-response/file-error/draft/reload/restart presentation at
390/1280 widths and explicit no-outbox/no-secret-storage assertions.

Logs: `connection_security_refused`, `owner_session_established`,
`connection_certificate_saved`, `connection_certificate_loaded`,
`connection_certificate_metadata_unmeasured`, `local_connection_setup`; refusal
reasons are fixed codes and never echo request bodies. Exact frozen source hashes,
parent commands and executable mutation controls are under
`/private/tmp/amux-astra-20260920/logs/aab4-connection-security-*`. No runtime pass,
trusted-browser acceptance, main integration or production deployment is asserted.
