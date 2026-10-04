## Agent skills

### Issue tracker

GitHub — issues live in `kido5217/user-service-switcher` GitHub Issues, driven via the `gh` CLI. See `docs/agents/issue-tracker.md`.

### Triage labels

Default five-role vocabulary, each label string equal to its role name. See `docs/agents/triage-labels.md`.

### Domain docs

Single-context: `CONTEXT.md` + `docs/adr/` at the repo root. See `docs/agents/domain.md`.

### Git workflow

Standard flow: branch → commit → push → PR → merge (squash) → pull + rebase. `main` is protected — changes land only via a merged PR. The model is permitted to merge.

**Local CI**: there is no GitHub Actions CI. The merge gate is `just ci` green in the dev shell (`nix develop`; the `justfile` at the repo root; `just` ships in the devShell) plus the reviewer's own verification runs. `just ci` = `cargo fmt --check`, clippy `-D warnings`, `cargo test`, `nix build`.
