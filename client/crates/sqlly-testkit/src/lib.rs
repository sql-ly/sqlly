// `deny` rather than `forbid`: the `env` module needs one scoped
// `#[allow(unsafe_code)]` to centralize Edition-2024 `env::set_var` unsafety
// behind a process-wide lock; everything else stays unsafe-free.
#![deny(unsafe_code)]
pub mod conformance;
pub mod container;
pub mod env;
pub mod golden;
pub mod live;

pub use container::SqlServerContainer;
pub use env::EnvGuard;
pub use golden::GoldenTest;
pub use live::{LOCAL_SQL_CONFIG_FILE, LiveSqlConfig};

pub const DEFAULT_SQLSERVER_PORT: u16 = 1433;
pub const DEFAULT_SEED_DATABASE: &str = "sqlly_seed";
