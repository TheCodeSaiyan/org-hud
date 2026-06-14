# Contributing to Org HUD

Thanks for looking. This is the public, member-facing overlay client; contributions from
members and the wider community are welcome.

## Ground rules

1. **The compliance promise is non-negotiable.** No change may introduce process injection,
   memory reading, input automation, or network interception of the game. PRs that do will be
   declined regardless of how useful the feature is. See [docs/COMPLIANCE.md](docs/COMPLIANCE.md).
2. **No private platform internals.** This repo depends only on the public `hud-protocol`
   wire contract. Do not add dependencies on the platform's private crates.
3. **Keep it inspectable.** Clear code over clever code — members trust this because they can
   read it.

## Building

```bash
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo build
```

## Proposing changes

- Open an issue first for anything beyond a small fix.
- Conventional commit messages (`feat:`, `fix:`, `docs:` …).
- Small, reviewable PRs; CI must be green before merge.
