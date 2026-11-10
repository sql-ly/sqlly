# Contributing

Thanks for helping. A few things work differently here.

## How changes land

This repository is a published snapshot of each SQLly release. Development happens in a private repository, and every release adds one commit here named after its version. Pull requests are welcome; we review them here and apply them upstream with your authorship kept, and they appear in the next release. We don't merge pull requests directly on GitHub.

## The contributor agreement

Every pull request needs [`CLA.md`](CLA.md). Include this line in the pull request description:

> I have read and agree to the SQLly Contributor License Agreement (CLA.md).

We can't take a contribution without it.

## Building and testing

```sh
cd client
cargo build -p sqlly-gpui --release      # the desktop app
cargo test --workspace                   # the test suite
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
```

- macOS builds target Apple Silicon only.
- Linux and Windows build for x86_64 and arm64.
- Some tests need live databases; `client/docker-compose.yml` starts them.

## Reporting bugs

Open an issue with your SQLly version, operating system, database engine and version, and the steps to reproduce. Report security problems privately; see [`SECURITY.md`](SECURITY.md).
