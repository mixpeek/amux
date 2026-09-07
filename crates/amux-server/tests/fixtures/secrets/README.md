# Test fixtures — NOT real secrets

`test-key.txt` and `test-secrets.yaml.age` are a throwaway age keypair and a
tiny encrypted fixture used by `tests/secrets_decrypt.rs` (direct
`SecretStore::load`/`get`) and `crates/amux-server/src/api/settings.rs`'s
`get_env` fallback-tier test (the end-to-end path: an unmapped credential
that lives ONLY in the encrypted store, resolved through the real
`GET /api/settings/env` handler). The private key in `test-key.txt` decrypts
exactly two things anywhere, both checked in right next to it in this same
directory, inside `test-secrets.yaml.age`:
- `test.dummy_secret` → `fixture-value-do-not-use`
- `external_services.openai.api_key` → `sk-fixture-do-not-use-1234`

If a scanner (or a person) flags `test-key.txt` as a "leaked private key" —
that pattern-match is correct, it IS a real age private key, it's just one
with nothing of value behind it. Regenerate both files together if this ever
needs to change:

```bash
age-keygen -o test-key.txt
PUB=$(age-keygen -y test-key.txt)
cat <<'YAML' | age -r "$PUB" -o test-secrets.yaml.age -
test:
  dummy_secret: fixture-value-do-not-use
external_services:
  openai:
    api_key: sk-fixture-do-not-use-1234
YAML
```
