//! Everything the pane derives from one statement's plan, computed once per
//! capture so no view re-derives it while painting.

use sqlly_core::DisplayMessage;
use sqlly_services::plans::{
    ExecutionPlan, FindingSeverity, FlameLayout, NumberStyle, OperatorTableRow, PlanAnalysis,
    PlanBreakdown, PlanContext, PlanFinding, PlanStory, SchemaContext, analyze_plan, batch_story,
    findings_for_plan_with_context, plan_breakdown, plan_flame_layout, plan_operator_table,
    resolve_batch_aliases, with_number_style,
};

/// How many lines each "where the time goes" list shows before the rest is
/// folded into one "everything else" line.
const BREAKDOWN_TOP: usize = 8;

/// One statement's plan with its analysis, story, findings and the table and
/// chart projections.
#[derive(Debug, Clone)]
pub struct StatementPlans {
    /// The plan as analysed: query aliases already replaced by table names
    /// where the statement text settles them.
    pub plan: ExecutionPlan,
    pub statement_text: String,
    pub statement_type: String,
    pub analysis: PlanAnalysis,
    pub story: PlanStory,
    pub findings: Vec<PlanFinding>,
    pub table: Vec<OperatorTableRow>,
    pub flame: Option<FlameLayout>,
    pub breakdown: Option<PlanBreakdown>,
    /// This statement's share of the batch, 0..1, when the batch has more
    /// than one statement and they can be weighed against each other.
    pub batch_share: Option<f64>,
    /// The findings were checked against table definitions from the
    /// connection's catalog.
    pub used_schema: bool,
    /// Tables whose alias in the plan was replaced by the real name.
    pub resolved_aliases: usize,
}

/// A whole capture: every statement plus the line that says which one
/// matters.
#[derive(Debug, Clone, Default)]
pub struct BatchPlans {
    pub headline: Option<DisplayMessage>,
    pub statements: Vec<StatementPlans>,
}

impl BatchPlans {
    /// Analyse `plans` with numbers written for the interface language.
    /// `query_text` is the SQL that produced them, used to turn the aliases
    /// MySQL, MariaDB and SQLite print back into table names. `schema` lets
    /// the findings check real column types and keys. Call on the UI thread;
    /// a background analysis uses [`Self::build_styled`] with the style read
    /// here.
    pub fn build(
        plans: Vec<ExecutionPlan>,
        query_text: Option<&str>,
        schema: Option<&SchemaContext>,
    ) -> Self {
        Self::build_styled(super::plan_number_style(), plans, query_text, schema)
    }

    /// [`Self::build`] with an explicit number style, for threads where the
    /// interface language is not visible.
    pub fn build_styled(
        style: NumberStyle,
        plans: Vec<ExecutionPlan>,
        query_text: Option<&str>,
        schema: Option<&SchemaContext>,
    ) -> Self {
        with_number_style(style, || Self::analyse(plans, query_text, schema))
    }

    fn analyse(
        mut plans: Vec<ExecutionPlan>,
        query_text: Option<&str>,
        schema: Option<&SchemaContext>,
    ) -> Self {
        let context = PlanContext::new()
            .with_schema(schema)
            .with_query_text(query_text);
        let aliases = resolve_batch_aliases(&mut plans, &context);
        let batch = batch_story(&plans);
        let headline = batch.headline;
        let mut stories = batch.statements.into_iter();
        let statements = plans
            .into_iter()
            .enumerate()
            .map(|(index, plan)| {
                let (mut analysis, mut story, batch_share) = match stories.next() {
                    Some(statement) => (statement.analysis, statement.story, statement.share),
                    None => {
                        let analysis = analyze_plan(&plan);
                        let story = sqlly_services::plans::plan_story(&plan, &analysis);
                        (analysis, story, None)
                    }
                };
                keep_named_bottleneck(&mut analysis, &mut story);
                let mut findings = findings_for_plan_with_context(&plan, &context);
                let used_schema = schema.is_some_and(|schema| {
                    plan.nodes.iter().any(|node| {
                        node.relation.as_deref().is_some_and(|name| {
                            schema.table(node.schema.as_deref(), name).is_some()
                        })
                    })
                });
                if schema.is_some() {
                    findings.retain(super::schema::finding_is_reliable);
                }
                let table = plan_operator_table(&analysis);
                let flame = plan_flame_layout(&analysis, None);
                let breakdown = plan_breakdown(&analysis, BREAKDOWN_TOP);
                StatementPlans {
                    statement_text: plan.statement_text.clone(),
                    statement_type: plan.statement_type.clone(),
                    analysis,
                    story,
                    findings,
                    table,
                    flame,
                    breakdown,
                    batch_share,
                    used_schema,
                    resolved_aliases: aliases.get(index).map_or(0, Vec::len),
                    plan,
                }
            })
            .collect();
        Self {
            headline,
            statements,
        }
    }
}

/// The analysis marks the step with the largest own share as the
/// bottleneck even when the story says no single step dominates. Every view
/// here labels that mark "Bottleneck", so it is kept only when the story
/// names one, with the story's own rule; the table, chart and diagram are
/// built from the cleared analysis and agree with the sentence.
pub(crate) fn keep_named_bottleneck(analysis: &mut PlanAnalysis, story: &mut PlanStory) {
    let named = story.named_bottleneck();
    for node in &mut analysis.nodes {
        node.flags.bottleneck = named == Some(node.node_id);
    }
    for step in &mut story.steps {
        step.is_bottleneck = named == Some(step.node_id);
    }
}

impl StatementPlans {
    /// Findings attached to `node_id`, as indexes into [`Self::findings`].
    pub fn findings_for_node(&self, node_id: i32) -> impl Iterator<Item = usize> + '_ {
        self.findings
            .iter()
            .enumerate()
            .filter(move |(_, finding)| finding_touches(finding, node_id))
            .map(|(index, _)| index)
    }

    /// The worst severity among the findings on `node_id`.
    pub fn node_severity(&self, node_id: i32) -> Option<FindingSeverity> {
        self.findings
            .iter()
            .filter(|finding| finding_touches(finding, node_id))
            .map(|finding| finding.severity)
            .min_by_key(|severity| severity_rank(*severity))
    }
}

pub(crate) fn finding_touches(finding: &PlanFinding, node_id: i32) -> bool {
    finding.node_id == Some(node_id) || finding.node_ids.contains(&node_id)
}

pub(crate) fn severity_rank(severity: FindingSeverity) -> u8 {
    match severity {
        FindingSeverity::Critical => 0,
        FindingSeverity::Warning => 1,
        FindingSeverity::Info => 2,
    }
}

/// Build the bounded AI prompt for a set of captured statement plans. Pure so
/// the digest (headline, ranked findings, heaviest steps, all capped) is unit
/// tested without a window. Returns `None` when there are no statements. The
/// digest never includes the raw plan document.
pub(crate) fn ai_explain_prompt_for(statements: &[StatementPlans]) -> Option<String> {
    if statements.is_empty() {
        return None;
    }
    const MAX_FINDINGS: usize = 8;
    const MAX_OPERATORS: usize = 8;
    let say = |message: &DisplayMessage| crate::localization::message(message);
    let mut out = String::from(
        "Explain this SQL execution plan in plain language: what is expensive, why, \
         and what could make it faster. Do not rewrite my SQL.\n",
    );
    for statement in statements.iter().take(3) {
        out.push('\n');
        out.push_str(say(&statement.story.headline).trim());
    }

    let findings: Vec<&PlanFinding> = statements.iter().flat_map(|s| &s.findings).collect();
    if !findings.is_empty() {
        out.push_str("\n\nFindings:");
        for finding in findings.iter().take(MAX_FINDINGS) {
            out.push_str(&format!(
                "\n- [{:?}] {} — {} Suggested: {}",
                finding.severity,
                say(&finding.summary).trim(),
                say(&finding.why).trim(),
                say(&finding.action).trim(),
            ));
        }
    }

    // The heaviest steps by their own share of the statement, with the
    // sentence the story already wrote for them.
    let mut steps: Vec<(f64, String)> = statements
        .iter()
        .flat_map(|statement| statement.story.steps.iter())
        .map(|step| {
            (
                step.share.unwrap_or(0.0),
                format!(
                    "{}: {}",
                    say(&step.title).trim(),
                    say(&step.sentence).trim()
                ),
            )
        })
        .collect();
    steps.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    if !steps.is_empty() {
        out.push_str("\n\nMost expensive operators:");
        for (share, line) in steps.iter().take(MAX_OPERATORS) {
            if *share > 0.0 {
                out.push_str(&format!(
                    "\n- {line} ({})",
                    sqlly_services::plans::human_share(*share)
                ));
            } else {
                out.push_str(&format!("\n- {line}"));
            }
        }
    }
    Some(out)
}
