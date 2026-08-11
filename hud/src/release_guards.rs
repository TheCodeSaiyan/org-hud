//! Release-shape guards for the HUD packaging path.
//!
//! These are `#[cfg(test)]`-only and read `tauri.conf.json` / `Cargo.toml` with `include_str!`, so
//! they introduce no non-test items — which matters because `hud` is a **bin** crate: under
//! `cargo clippy --all-targets` the `cfg(test)` unit is stripped from the binary, so any helper
//! reachable only from a test is dead code and `-D warnings` kills it.
//!
//! Both include paths stay inside the crate (`../tauri.conf.json`, `../Cargo.toml`), which keeps the
//! `crates/hud/Cargo.toml` extraction invariant intact — the module compiles unchanged in the
//! standalone layout `scripts/extract-hud.ps1` produces.

/// `tauri.conf.json` and `Cargo.toml` must agree about the app version.
///
/// They are two independent sources for one number. If they drift, `latest.json` advertises a
/// version the installed binary does not report, and every client re-downloads the same update
/// forever — a failure only visible to a user, never to CI. `.github/workflows/release-hud.yml`
/// re-checks this in its preflight because a tag push does not run `ci.yml`; this test is the copy
/// that fires on the PR that causes the drift.
#[test]
fn cargo_and_tauri_versions_agree() {
    let tauri_conf = include_str!("../tauri.conf.json");
    let cargo_toml = include_str!("../Cargo.toml");

    let tauri_version = tauri_conf
        .lines()
        .find_map(|l| l.trim().strip_prefix("\"version\":"))
        .map(|v| v.trim().trim_end_matches(',').trim_matches('"').to_string())
        .expect("tauri.conf.json has a top-level \"version\" key");

    let cargo_version = cargo_toml
        .lines()
        .find_map(|l| l.trim().strip_prefix("version = "))
        .map(|v| v.trim().trim_matches('"').to_string())
        .expect("crates/hud/Cargo.toml has a `version = \"…\"` line");

    assert_eq!(
        tauri_version, cargo_version,
        "HUD version drift: tauri.conf.json says {tauri_version}, Cargo.toml says {cargo_version}. \
         Bump both — latest.json is generated from tauri.conf.json but the running app reports \
         the Cargo version, so a mismatch makes auto-update loop."
    );
}

/// Windows ships BOTH installers, and NSIS owns the updater.
///
/// This pins a coupled pair of decisions that live in different files: `msi` is in the bundle
/// targets here, and `.github/workflows/release-hud.yml` therefore passes
/// `updaterJsonPreferNsis: true` to tauri-action. tauri-action's own default for that input is
/// `false`, so if both installers exist and nobody says otherwise the update manifest is built from
/// the **MSI** — silently moving existing `-setup.exe` users onto a different install path. Dropping
/// either target from this list without revisiting the workflow re-opens that.
#[test]
fn windows_bundles_both_installers_and_nsis_owns_the_updater() {
    let tauri_conf = include_str!("../tauri.conf.json");
    let targets = tauri_conf
        .lines()
        .find(|l| l.trim_start().starts_with("\"targets\":"))
        .expect("tauri.conf.json declares bundle.targets");

    assert!(
        targets.contains("\"nsis\""),
        "the NSIS target is gone, but release-hud.yml still sets updaterJsonPreferNsis: true — the \
         updater would fall back to the MSI. Decide which installer owns auto-update, in both files."
    );
    assert!(
        targets.contains("\"msi\""),
        "the MSI target is gone; drop `updaterJsonPreferNsis` from release-hud.yml too, or leave a \
         note saying why the preference is kept for a single-installer build."
    );
}

/// The updater's two halves must agree, and the endpoint must be reachable WITHOUT auth.
///
/// Replaces `the_updater_is_knowingly_inert`, which asserted the pubkey placeholder was still
/// present. That guard did its job — it forced a conscious decision when the real key landed — but
/// "the placeholder is still here" stops describing anything once signing is on. What still needs
/// pinning are the two states that fail silently:
///
///   1. **Key and artifacts must match.** Tauri signs BECAUSE it was asked to emit updater
///      artifacts, not because a key env var is set. `createUpdaterArtifacts: true` with no key
///      builds both installers and then dies on `failed to decode secret key` (hud-v0.1.0); a real
///      pubkey with artifacts `false` produces no `.sig` at all, so `latest.json` would reference
///      signatures that were never made. `release-hud.yml`'s preflight refuses both at CI time —
///      this is the compile-time half, so a local `cargo tauri build` cannot drift either.
///
///   2. **The endpoint must not point at a PRIVATE repo.** Release assets on a private repo require
///      an authenticated request and the Tauri updater sends none, so such an endpoint 404s
///      forever — and a failed update check is not user-visible. `orgplatform` is private; the
///      endpoint must therefore name a public host (today: the `org-hud` repo of Option 3 in
///      `docs/HUD-RELEASE.md`).
///
/// **This is not a "has the endpoint been created yet" check** — a test cannot know that. It only
/// catches the endpoint being aimed somewhere structurally incapable of serving the updater.
#[test]
fn updater_signing_and_endpoint_agree() {
    let tauri_conf = include_str!("../tauri.conf.json");

    let placeholder = tauri_conf.contains("REPLACE_WITH_TAURI_SIGNER_PUBLIC_KEY");
    let artifacts_on = tauri_conf.contains("\"createUpdaterArtifacts\": true");

    assert_eq!(
        !placeholder, artifacts_on,
        "updater half-configured: pubkey_is_placeholder={placeholder}, \
         createUpdaterArtifacts={artifacts_on}. A real key needs artifacts ON (or nothing is \
         signed and latest.json references signatures that do not exist); the placeholder needs \
         them OFF (or `tauri build` demands a key it does not have and fails AFTER building both \
         installers). See docs/HUD-RELEASE.md."
    );

    // Only meaningful once a real key is in play — with the placeholder the updater is inert and
    // the endpoint cannot mislead anyone.
    if !placeholder {
        assert!(
            !tauri_conf.contains("TheCodeSaiyan/orgplatform/releases"),
            "the updater endpoint points at `orgplatform`, which is PRIVATE. Release assets there \
             need an authenticated request and the updater sends none, so every update check would \
             404 — silently. Point it at a public host (docs/HUD-RELEASE.md, Option 3)."
        );
    }
}
