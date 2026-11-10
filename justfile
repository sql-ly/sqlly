# SQLLY task runner — mirrors the macOS PR CI gate.
# All cargo recipes run from `client/`. Install: https://github.com/casey/just

set shell := ["bash", "-eu", "-o", "pipefail", "-c"]

client := "client"

# Apply rustfmt (local fix-up; CI uses fmt-check).
fmt:
    cd {{client}} && cargo fmt --all

# Azure macos_ci: Check Rust formatting
fmt-check:
    cd {{client}} && cargo fmt --all -- --check

# Azure macos_ci: icon manifest + generated code drift guard
icons-check:
    node tools/icons/check.mjs

# Azure macos_ci: browser Lucide vendor drift guard (needs workspace deps fetched — run after check)
icons-lucide-check:
    node tools/icons/sync-lucide.mjs --check

# Azure macos_ci: Lint Rust workspace
clippy:
    cd {{client}} && cargo clippy --workspace --all-targets --all-features --locked -- -D warnings

# Compile the workspace without running tests.
check:
    cd {{client}} && cargo check --workspace --locked

# Azure macos_ci: Test Rust workspace
test:
    cd {{client}} && cargo test --workspace --locked

# Azure macos_ci: Audit Rust dependencies (cargo deny).
# cargo-deny 0.20+: global --locked before subcommand; 0.19 used check --locked.
audit:
    cd {{client}} && cargo deny --locked check

# Azure macos_ci: Check wasm browser build (compile-only on PR CI). Separate from `ci` — needs wasm32 target + clang wasm support.
wasm-check:
    cd {{client}} && cargo check -p sqlly-browser --target wasm32-unknown-unknown --locked

# Azure linux_live_sql: live SQL integration tests (requires Docker + SQL Server)
live-sql:
    cd {{client}} && SQLLY_REQUIRE_LIVE_SQL=1 cargo test -p sqlly-engines -p sqlly-services -p sqlly-cli --locked

# Azure live-dialects / GHA live-drivers: PostgreSQL conformance (start postgres:16 locally first)
live-pg:
    cd {{client}} && SQLLY_PG_SERVER=127.0.0.1 SQLLY_PG_PORT=5432 SQLLY_PG_USER=postgres SQLLY_PG_PASSWORD=sqlly-ci SQLLY_PG_DATABASE=postgres cargo test -p sqlly-engines --test pg_conformance --locked

# Azure live-dialects / GHA live-drivers: MySQL conformance (start mysql:8 locally first)
live-mysql:
    cd {{client}} && SQLLY_MYSQL_SERVER=127.0.0.1 SQLLY_MYSQL_PORT=3306 SQLLY_MYSQL_USER=root SQLLY_MYSQL_PASSWORD=sqlly-ci SQLLY_MYSQL_DATABASE=sqlly_conformance cargo test -p sqlly-engines --test mysql_conformance --locked

# Optional: CockroachDB conformance over the PostgreSQL driver (docker run -d -p 26257:26257 cockroachdb/cockroach:latest start-single-node --insecure)
live-cockroach:
    cd {{client}} && SQLLY_PG_ENGINE=cockroachdb SQLLY_PG_SERVER=127.0.0.1 SQLLY_PG_PORT=26257 SQLLY_PG_USER=root SQLLY_PG_PASSWORD= SQLLY_PG_DATABASE=defaultdb cargo test -p sqlly-engines --test pg_conformance --locked

# Optional: TiDB conformance over the MySQL driver (docker run -d -p 4000:4000 pingcap/tidb:latest)
live-tidb:
    cd {{client}} && SQLLY_MYSQL_ENGINE=tidb SQLLY_MYSQL_SERVER=127.0.0.1 SQLLY_MYSQL_PORT=4000 SQLLY_MYSQL_USER=root SQLLY_MYSQL_PASSWORD= SQLLY_MYSQL_DATABASE=test cargo test -p sqlly-engines --test mysql_conformance --locked

# Optional: kubectl port-forward transport tests (fake kubectl fixture)
fake-kubectl:
    cd {{client}} && SQLLY_FAKE_KUBECTL=1 cargo test -p sqlly-engines --locked --test kube_forward

# GPUI macOS arm64 release app — `./build.sh`
build-mac:
    ./build.sh

# GPUI macOS arm64 debug app — `./build-gpui-debug-app.sh`
build-debug:
    ./build-gpui-debug-app.sh

# Cross-platform GPUI builds — `./build-all.sh`
build-all:
    ./build-all.sh

# Browser/wasm bundle — `./build-browser.sh` (release pipeline wasm_build stage)
build-browser:
    ./build-browser.sh

# Stage the browser preview into the website for local serving (maintainers
# only: the website isn't in the open-source tree). Builds first if no bundle
# exists; pass --build to force a rebuild.
stage-browser-preview *ARGS:
    ./scripts/stage-browser-preview.sh {{ARGS}}

# Azure macos_ci gate: fmt + icons + clippy + deny + check + test + Lucide vendor check
ci: fmt-check icons-check clippy audit check test icons-lucide-check
