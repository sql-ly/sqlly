//! Oracle script splitting lives in `sqlly_lang::sqllex::oracle_script`, so
//! the driver and the safety gates (`sqlly_lang::seclex::execution_units`)
//! split scripts with one implementation and can never disagree about which
//! statement OCI receives.

pub use sqlly_lang::sqllex::oracle_script::{ScriptStatement, split_script};
