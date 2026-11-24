//! The notebook agent (spec 9, NB-06, ADR-006, ADR-015): Assist, Plan,
//! Build, Investigate, Review and Explain changes over a notebook, with
//! structured tool calls whose policy is decided here, in Rust, never by
//! the model.
//!
//! It lives in `sqlly-services` because this is the one crate that sees
//! both sqlly-ai's agent pieces (providers, the text tool protocol,
//! compaction, the read-only `AgentToolHost`) and the notebook core and
//! security modules, without a dependency cycle.
//!
//! - [`tools`]: the vocabulary and each mode's inventory. There is no write,
//!   export, network or policy tool.
//! - [`host`]: [`host::NotebookToolHost`] runs calls: inventory, parsing,
//!   slot authorization from the notebook's trust decision, the SELECT-only
//!   check, the budget, then the tool. Database text comes back fenced.
//! - [`budget`]: queries, rows, query time, tokens, turns and an optional
//!   cost cap, all checked before the work happens.
//! - [`context`] and [`prompts`]: what the model may see, the exact first
//!   request, and the consent preview built from the same text.
//! - [`proposals`]: proposed cells, edits and runs, refused when they would
//!   write; accepted ones apply as one undoable group of attributed cells.
//! - [`findings`]: parsing the answer and verifying each claim's evidence.
//! - [`review`] and [`explain`]: the deterministic checks that run before
//!   the model in Review and Explain changes.
//! - [`agent`]: the bounded loop.
//! - [`log`]: the per-notebook action log and the local consent store.

pub mod agent;
pub mod budget;
pub mod context;
pub mod explain;
pub mod findings;
pub mod host;
pub mod log;
pub mod prompts;
pub mod proposals;
pub mod review;
pub mod tools;

#[cfg(test)]
mod catalog_tests;
#[cfg(test)]
mod eval_tests;

pub use agent::{
    INSTRUCTIONS_ECHOED_DETAIL, NO_MODEL_DETAIL, NotebookAgentEvent, is_stub_provider,
    model_location, run_notebook_agent,
};
pub use budget::{BudgetMeter, Clock, Pricing, system_clock};
pub use context::{CellRuntime, ContextPreview, NotebookContext};
pub use host::NotebookToolHost;
pub use prompts::{AgentRequest, preview};
pub use proposals::{AppliedGroup, Refusal};
