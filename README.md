# SQLly

SQLly is a cross-platform database studio for SQL Server, PostgreSQL, MySQL, MariaDB, SQLite, DuckDB, ClickHouse, Oracle, Redis and more, with a fast native editor, schema-aware IntelliSense, result grids, query plans and admin tools. It's written in Rust on [GPUI](https://www.gpui.rs/).

Official builds, docs and pricing are at <https://sqlly.app>. This repository holds the source of each release of the desktop app and its command-line tools.

## License

SQLly is source-available, not open source, for now. The plan is to make the desktop app fully open source within the next year. Source-available is the starting point, not the destination. [Why SQLly is source-available, and where it's headed](https://sqlly.app/blog/sqlly-source-available) explains the difference and the plan.

Until then, it's offered under two licenses:

- **Free** under the [PolyForm Small Business License](LICENSE.md), for individuals and companies with fewer than 100 people and under USD 1M in revenue. An [additional grant](LICENSING.md) also covers personal, educational and non-profit use.
- **Commercial** for everyone else; see [COMMERCIAL-LICENSE.md](COMMERCIAL-LICENSE.md).

[LICENSING.md](LICENSING.md) has the details. The name and logo are covered separately, by [TRADEMARKS.md](TRADEMARKS.md).

## Building

Every [release](../../releases) here has prebuilt, unsigned standalone builds for Linux, macOS and Windows attached, made from that tag's source by `.github/workflows/release.yml`. To build it yourself, you need Rust (the version is pinned in `client/rust-toolchain.toml`).

```sh
cd client
cargo build -p sqlly-gpui --release
cargo run -p sqlly-gpui --release
```

- macOS builds target Apple Silicon only.
- Linux and Windows build for x86_64 and arm64. Cross-building needs [zig](https://ziglang.org/) and [cargo-zigbuild](https://github.com/rust-cross/cargo-zigbuild); see `build-x64.sh` and `build-arm64.sh`.
- The browser build needs nightly Rust and `wasm-bindgen-cli`; see `build-browser.sh`.

A build from this source can't send feedback to sqlly.app (official builds carry a signing key that isn't published here), so Send Feedback and F2 say feedback is disabled.

## Standalone builds

SQLly has one build switch that turns the app into a fully standalone tool that never contacts sqlly.app: the `standalone` cargo feature.

```sh
cd client
cargo build -p sqlly-gpui --release --features standalone
# macOS app bundle:
CARGO_FEATURES=standalone ./apps/gpui/scripts/build-app.sh
```

Turning it on disables, and labels as disabled, everything that talks to sqlly.app:

| Feature | In a standalone build |
|---|---|
| Account sign-in and the account panel | Off. The account button shows "Accounts are disabled in this standalone build". |
| License checks | Off. No supporter message, no pay reminders, no Unlock window, no saved-connection limit, and no first-run license prompt. You are still responsible for using SQLly under the PolyForm Small Business License or a commercial license. |
| Send Feedback and Improve IntelliSense (F2) | Off. Both say "Feedback is disabled in this standalone build". The feedback signing key is never embedded. |
| Update checks and in-app updates | Off. Help ▸ Check for Updates says updates are disabled; update by rebuilding from source. |
| Cloud Relay and bridge connections | Off. They depend on a sqlly.app account, so setting one up reports that this build doesn't connect to sqlly.app. |

Everything else works the same: direct database connections, the editor, IntelliSense, results, exports, local and self-configured AI providers, and every other local feature. Help menu links (docs, privacy, terms) still open sqlly.app pages in your browser if you click them, but the app itself makes no requests to sqlly.app. Every request function in `client/apps/gpui/src/store.rs` starts with a standalone guard, and a unit test fails if a new one is added without it.

`standalone` takes precedence over the `app-store` feature if both are enabled.

## Layout

| Path | What it is |
|---|---|
| `client/apps/gpui` | The desktop app |
| `client/apps/cli` | `sqlly-cli` and the database engine host |
| `client/apps/browser` | The browser (WebAssembly) build |
| `client/crates/*` | The core libraries: engines, IntelliSense, language, services, wire protocol |
| `client/vendor` | Vendored, patched third-party crates |
| `changelog/` | Release notes for every version |

## What isn't here, and why

Development happens in a separate, private repository, and this one gets a cleaned snapshot of each release. One reason for that split is so that content people send us stays private.

The clearest case is IntelliSense feedback. When someone reports a bad completion from the app, the report can include table, column and other names from their own database. We turn many of those reports into regression tests, and those tests keep the names as submitted. The person who sent the report shared it to get a bug fixed, not to have their schema published, so those tests (`client/crates/sqlly-intel/tests/feedback_*.rs`) are left out of every snapshot. The export that builds this repository refuses to publish if one slips through. Other user-submitted content, such as feedback reports and screenshots, stays private the same way.

The rest of the test suite is here, and `cargo test --workspace` runs without the excluded tests.

The SQLly bridge, relay, Cloud Relay service and SQLly Embedded aren't included either.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). Pull requests need the [contributor agreement](CLA.md).
