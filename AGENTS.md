## Agent skills

### Issue tracker

GitHub — issues live in `kido5217/user-service-switcher` GitHub Issues, driven via the `gh` CLI. See `docs/agents/issue-tracker.md`.

### Triage labels

Default five-role vocabulary, each label string equal to its role name. See `docs/agents/triage-labels.md`.

### Domain docs

Single-context: `CONTEXT.md` + `docs/adr/` at the repo root. See `docs/agents/domain.md`.

### Git workflow

Standard flow: branch → commit → push → PR → merge (squash) → pull + rebase. `main` is protected — changes land only via a merged PR. The model is permitted to merge.
