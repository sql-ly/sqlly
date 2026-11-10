//! Natural-language dates for the SQL editor: "last wed at 3", "november",
//! "ytd" and friends, resolved against a caller-supplied `now`.
//!
//! `parse` turns a phrase into a half-open [`ParsedDate`] span; it knows
//! nothing about SQL, so it can be tested (and reused) in isolation.

mod parse;

pub use parse::*;
mod sql;

pub use sql::*;
