//! Real fixture-based proof that `SecretStore::load()` actually decrypts and
//! parses — PR #163 review item 5 (@esteininger): the only existing coverage
//! (`secrets/persist.rs::test_encrypt_and_persist`) is `#[ignore]`d because it
//! needs a real machine-local age key, so it never runs in CI and proves
//! nothing to a reviewer. This uses a throwaway age keypair + a tiny
//! encrypted fixture, both checked in under `tests/fixtures/secrets/` — no
//! real secret, no machine dependency on a *key*, so it runs by default.
//!
//! It DOES still depend on the `age` binary being on PATH, same as the
//! production code it is testing (`SecretStore::load` shells out to `age -d`).
//! CI (`.github/workflows/rust.yml`) does not currently install `age`, so
//! `age_available()` below skips (not fails) when it is missing — this test
//! runs for real on any box that has `age` (this dev box included, right now)
//! and is silent-but-harmless where it doesn't, rather than turning "no age
//! binary" into a red CI build for a check that isn't testing the CI runner.
//!
//! Ethos rule 7 ("can your check actually fail?"): `decrypt_corrupted_fixture_fails`
//! proves this the other direction — the same store, pointed at a fixture
//! with one byte flipped, must return `Err`, not silently succeed with an
//! empty store.

use amux_server::secrets::SecretStore;
use std::path::PathBuf;

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/secrets")
}

/// `age` is not installed on every box (confirmed absent from CI's
/// `ubuntu-latest` job — no install step anywhere in `.github/workflows/`).
/// Skipping here, rather than failing, keeps this test from turning a
/// missing dev-tool into a red build for a check that isn't testing CI itself.
async fn age_available() -> bool {
    tokio::process::Command::new("age")
        .arg("--version")
        .output()
        .await
        .is_ok()
}

#[tokio::test]
async fn load_decrypts_and_get_retrieves_the_real_value() {
    if !age_available().await {
        eprintln!("skipping: `age` binary not on PATH");
        return;
    }
    let dir = fixtures_dir();
    let store = SecretStore::new(dir.join("test-key.txt"), dir.join("test-secrets.yaml.age"));

    store.load().await.expect("fixture must decrypt — if this fails, the fixture or age itself is broken, not the code under test");

    assert_eq!(
        store.get("test.dummy_secret").await.as_deref(),
        Some("fixture-value-do-not-use"),
        "SecretStore::get() must retrieve the value that was actually encrypted into the fixture"
    );
    // Same fixture also carries the one path settings.rs's `get_env` fallback
    // tier is wired to (`known_env_secret_path("OPENAI_API_KEY")`) — see that
    // module's own test for the end-to-end version of this assertion.
    assert_eq!(
        store.get("external_services.openai.api_key").await.as_deref(),
        Some("sk-fixture-do-not-use-1234")
    );
    // A path that was never in the fixture must miss cleanly, not panic or
    // silently return something else.
    assert_eq!(store.get("test.nonexistent").await, None);
}

/// Proves the happy-path test above can actually fail (ethos rule 7): a
/// SecretStore pointed at a byte-corrupted copy of the same fixture must
/// return Err from `load()`, not silently succeed with an empty/wrong store.
#[tokio::test]
async fn decrypt_corrupted_fixture_fails() {
    if !age_available().await {
        eprintln!("skipping: `age` binary not on PATH");
        return;
    }
    let dir = fixtures_dir();
    let good = std::fs::read(dir.join("test-secrets.yaml.age")).expect("fixture must exist");
    let mut corrupted = good.clone();
    // Flip a byte well inside the age payload (past the short ASCII header
    // lines) so this corrupts ciphertext, not just whitespace age tolerates.
    let flip_at = corrupted.len() - 10;
    corrupted[flip_at] ^= 0xFF;

    let tmp = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(tmp.path(), &corrupted).unwrap();

    let store = SecretStore::new(dir.join("test-key.txt"), tmp.path().to_path_buf());
    let result = store.load().await;
    assert!(result.is_err(), "a corrupted ciphertext must fail to decrypt, not succeed with wrong data");
}
