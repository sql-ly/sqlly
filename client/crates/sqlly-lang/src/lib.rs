//! SQL language layer: the canonical model, the lexers, the parsers, the
//! formatter, and the dialect-invariant filesystem tokenizer.
//!
//! Each module here was previously its own crate. The split survives as module
//! boundaries, and two of them are load-bearing rather than cosmetic:
//!
//! * `tokenizer` is dialect-INVARIANT (ADR-016). It must never reach into
//!   `sqllex`, whose lexer is parameterised by the connected database's
//!   dialect — routing safety classification or parameter scanning through it
//!   would make those verdicts vary with the connection. The
//!   `tests/adr016_guard.rs` tripwire pins this at the module level now that
//!   the crate boundary that used to enforce it is gone.
//! * `seclex` is the security lexer: the one per-engine model of where
//!   literals, quoted identifiers and comments begin and end, read under every
//!   server configuration that could move a boundary. Every security decision
//!   (safety gates, mutation review, execution splitting) and the dialect
//!   lexer's own boundaries go through it.
//! * `fsmodel` and `sqlmodel` are pure data models with no behaviour, so
//!   everything above them can depend on them freely.

pub mod fsmodel;
pub mod message;
pub mod seclex;
pub mod sqlformat;
pub mod sqllex;
pub mod sqlmodel;
pub mod sqlparse;
pub mod tokenizer;

pub use message::DisplayMessage;
