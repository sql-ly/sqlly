//! The budget meter: every cap of [`Budget`] is checked here, in Rust,
//! before the work it limits happens. The model only ever sees the refusal.

use std::sync::{Arc, Mutex};

use crate::notebook::cells::ai::{Budget, BudgetKind, BudgetUsage};

/// Milliseconds on a monotonic-enough clock; injected so tests are exact.
pub type Clock = Arc<dyn Fn() -> u64 + Send + Sync>;

pub fn system_clock() -> Clock {
    Arc::new(|| chrono::Utc::now().timestamp_millis().max(0) as u64)
}

/// Price of a cloud model per 1,000 tokens sent (input) and received
/// (output), when the user configured one in the provider's settings. A
/// side left unset counts as free once the other is set.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Pricing {
    pub usd_per_1k_input: Option<f64>,
    pub usd_per_1k_output: Option<f64>,
}

impl Pricing {
    pub fn is_priced(&self) -> bool {
        self.usd_per_1k_input.is_some() || self.usd_per_1k_output.is_some()
    }

    /// The cost of `input` and `output` tokens, when priced.
    pub fn cost(&self, input: u64, output: u64) -> Option<f64> {
        self.is_priced().then(|| {
            input as f64 / 1000.0 * self.usd_per_1k_input.unwrap_or(0.0)
                + output as f64 / 1000.0 * self.usd_per_1k_output.unwrap_or(0.0)
        })
    }
}

#[derive(Debug)]
pub struct BudgetMeter {
    budget: Budget,
    pricing: Pricing,
    usage: Mutex<BudgetUsage>,
    /// Tokens sent and received so far (`usage.tokens` is their sum).
    split: Mutex<(u64, u64)>,
}

impl BudgetMeter {
    pub fn new(budget: &Budget, pricing: Pricing) -> BudgetMeter {
        BudgetMeter {
            budget: budget.clamped(),
            pricing,
            usage: Mutex::new(BudgetUsage {
                cost_usd: pricing.cost(0, 0),
                ..BudgetUsage::default()
            }),
            split: Mutex::new((0, 0)),
        }
    }

    fn split(&self) -> std::sync::MutexGuard<'_, (u64, u64)> {
        self.split.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn budget(&self) -> &Budget {
        &self.budget
    }

    pub fn usage(&self) -> BudgetUsage {
        self.lock().clone()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BudgetUsage> {
        self.usage.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Reserve one query and up to `wanted` rows. Returns the row limit the
    /// query may use, or the cap that refuses it.
    pub fn reserve_query(&self, wanted: u64) -> Result<u64, BudgetKind> {
        let mut usage = self.lock();
        if usage.queries >= self.budget.max_queries {
            return Err(BudgetKind::Queries);
        }
        if usage.runtime_ms >= self.budget.max_runtime_ms {
            return Err(BudgetKind::Runtime);
        }
        let remaining = self.budget.max_rows.saturating_sub(usage.rows);
        if remaining == 0 {
            return Err(BudgetKind::Rows);
        }
        usage.queries += 1;
        Ok(wanted.min(remaining).max(1))
    }

    /// Charge what a query actually read and how long it took.
    pub fn charge_query(&self, rows: u64, elapsed_ms: u64) {
        let mut usage = self.lock();
        usage.rows = usage.rows.saturating_add(rows);
        usage.runtime_ms = usage.runtime_ms.saturating_add(elapsed_ms);
    }

    /// Reserve rows read from notebook outputs (sample rows sent to the model).
    pub fn reserve_rows(&self, wanted: u64) -> Result<u64, BudgetKind> {
        let mut usage = self.lock();
        let remaining = self.budget.max_rows.saturating_sub(usage.rows);
        if wanted > 0 && remaining == 0 {
            return Err(BudgetKind::Rows);
        }
        let granted = wanted.min(remaining);
        usage.rows += granted;
        Ok(granted)
    }

    /// Start a model turn that sends about `prompt_tokens`. Refuses when the
    /// turn or token cap (or the cost cap, when priced) would be passed.
    pub fn begin_turn(&self, prompt_tokens: u64) -> Result<(), BudgetKind> {
        let mut usage = self.lock();
        if usage.turns >= self.budget.max_turns {
            return Err(BudgetKind::Turns);
        }
        if usage.tokens.saturating_add(prompt_tokens) > self.budget.max_tokens {
            return Err(BudgetKind::Tokens);
        }
        let mut split = self.split();
        let (input, output) = *split;
        if let (Some(cap), Some(cost)) = (
            self.budget.max_cost_usd,
            self.pricing
                .cost(input.saturating_add(prompt_tokens), output),
        ) && cost > cap
        {
            return Err(BudgetKind::Cost);
        }
        split.0 = input.saturating_add(prompt_tokens);
        usage.turns += 1;
        usage.tokens += prompt_tokens;
        usage.cost_usd = self.pricing.cost(split.0, split.1);
        Ok(())
    }

    /// Charge the reply's tokens.
    pub fn charge_reply(&self, reply_tokens: u64) {
        let mut usage = self.lock();
        let mut split = self.split();
        split.1 = split.1.saturating_add(reply_tokens);
        usage.tokens = usage.tokens.saturating_add(reply_tokens);
        usage.cost_usd = self.pricing.cost(split.0, split.1);
    }

    /// A cap that is already used up and stops the run outright.
    pub fn hard_stop(&self) -> Option<BudgetKind> {
        let usage = self.lock();
        if usage.tokens >= self.budget.max_tokens {
            return Some(BudgetKind::Tokens);
        }
        if let (Some(cap), Some(cost)) = (self.budget.max_cost_usd, usage.cost_usd)
            && cost >= cap
        {
            return Some(BudgetKind::Cost);
        }
        if self.budget.max_runtime_ms > 0 && usage.runtime_ms > self.budget.max_runtime_ms {
            return Some(BudgetKind::Runtime);
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn budget() -> Budget {
        Budget {
            max_queries: 2,
            max_rows: 100,
            max_runtime_ms: 1_000,
            max_tokens: 5_000,
            max_turns: 3,
            max_cost_usd: Some(0.01),
        }
    }

    #[test]
    fn queries_rows_and_runtime_are_capped_before_running() {
        let meter = BudgetMeter::new(&budget(), Pricing::default());
        assert_eq!(meter.reserve_query(80), Ok(80));
        meter.charge_query(80, 400);
        assert_eq!(
            meter.reserve_query(80),
            Ok(20),
            "rows clamp to what is left"
        );
        meter.charge_query(20, 400);
        assert_eq!(meter.reserve_query(1), Err(BudgetKind::Queries));
        let rows = BudgetMeter::new(&budget(), Pricing::default());
        rows.reserve_query(100).unwrap();
        rows.charge_query(100, 0);
        assert_eq!(rows.reserve_query(1), Err(BudgetKind::Rows));
        assert_eq!(rows.reserve_rows(5), Err(BudgetKind::Rows));
        let slow = BudgetMeter::new(&budget(), Pricing::default());
        slow.reserve_query(1).unwrap();
        slow.charge_query(1, 1_500);
        assert_eq!(slow.reserve_query(1), Err(BudgetKind::Runtime));
        assert_eq!(slow.hard_stop(), Some(BudgetKind::Runtime));
    }

    #[test]
    fn turns_tokens_and_cost_stop_turns() {
        let meter = BudgetMeter::new(&budget(), Pricing::default());
        meter.begin_turn(1_000).unwrap();
        meter.charge_reply(500);
        assert_eq!(meter.begin_turn(4_000), Err(BudgetKind::Tokens));
        meter.begin_turn(1_000).unwrap();
        meter.begin_turn(1_000).unwrap();
        assert_eq!(meter.begin_turn(10), Err(BudgetKind::Turns));
        assert_eq!(meter.usage().turns, 3);

        let priced = BudgetMeter::new(
            &budget(),
            Pricing {
                usd_per_1k_input: Some(0.005),
                usd_per_1k_output: None,
            },
        );
        priced.begin_turn(1_500).unwrap();
        assert!((priced.usage().cost_usd.unwrap() - 0.0075).abs() < 1e-9);
        assert_eq!(priced.begin_turn(1_000), Err(BudgetKind::Cost));
        priced.charge_reply(600);
        assert!(
            (priced.usage().cost_usd.unwrap() - 0.0075).abs() < 1e-9,
            "unpriced output tokens cost nothing"
        );
    }

    #[test]
    fn input_and_output_tokens_are_priced_separately() {
        let pricing = Pricing {
            usd_per_1k_input: Some(0.003),
            usd_per_1k_output: Some(0.015),
        };
        let meter = BudgetMeter::new(&budget(), pricing);
        assert_eq!(
            meter.usage().cost_usd,
            Some(0.0),
            "priced runs start at zero"
        );
        meter.begin_turn(1_000).unwrap();
        meter.charge_reply(400);
        let cost = meter.usage().cost_usd.unwrap();
        assert!((cost - (0.003 + 0.006)).abs() < 1e-9, "{cost}");
        assert_eq!(meter.hard_stop(), None);
        meter.charge_reply(100);
        assert_eq!(
            meter.hard_stop(),
            Some(BudgetKind::Cost),
            "0.0105 passes 0.01"
        );
        assert_eq!(meter.begin_turn(1), Err(BudgetKind::Cost));
        let free = BudgetMeter::new(&budget(), Pricing::default());
        free.begin_turn(1_000).unwrap();
        assert_eq!(free.usage().cost_usd, None, "no price, no estimate");
        assert!((pricing.cost(2_000, 1_000).unwrap() - 0.021).abs() < 1e-9);
    }

    #[test]
    fn a_plan_budget_allows_no_queries() {
        let meter = BudgetMeter::new(
            &Budget::for_mode(crate::notebook::cells::ai::AgentMode::Plan),
            Pricing::default(),
        );
        assert_eq!(meter.reserve_query(1), Err(BudgetKind::Queries));
    }
}
