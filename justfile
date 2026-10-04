# Local CI for user-service-switcher.
#
# Contract: enter the dev shell first (nix develop), then run these. The
# pinned toolchain (rust 1.98) comes from the devShell; `nix` is on the host
# PATH. `just ci` is the merge gate — the pipeline that used to run on
# GitHub Actions (removed): fmt-check -> clippy -> test -> build.

default: ci

# The full check pipeline (merge gate).
ci: fmt-check clippy test build

# -- fast iteration ---------------------------------------------------------

# Type-check + lints, no codegen.
check:
    cargo check --all-targets

# Run tests (unit + CLI harness; no user session needed).
test:
    cargo test

# Lint with clippy, warnings as errors.
clippy:
    cargo clippy --all-targets -- -D warnings

# Check formatting without modifying files.
fmt-check:
    cargo fmt --check

# Apply formatting.
fmt:
    cargo fmt

# Reproducible package build: the store path must carry bin/uss + bin/ussd.
build:
    nix build .#packages.x86_64-linux.default
    ls "$(readlink -f result)/bin/uss" "$(readlink -f result)/bin/ussd"

# Integration/e2e suite: needs a live user session (systemd user manager).
integration:
    cargo test -- --ignored
