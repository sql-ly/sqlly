//! The plan told as a story: a headline, a short summary of where the time
//! or cost went, and one numbered step per operator in the order the work
//! happens.
//!
//! Everything is built from [`PlanAnalysis`], so the numbers in the story are
//! the same numbers the diagram, the operator table and the comparison show.
//! An executed plan is described in the past tense with measured rows and
//! times; an estimated plan says what the optimizer *expects*; a plan with
//! no numbers at all (SQLite) just says what happens.

use std::collections::{HashMap, HashSet};

use sqlly_core::DisplayMessage;

use crate::plans::analysis::{
    EstimateDirection, NodeAnalysis, PlanAnalysis, PlanMetric, PlanObject, PlanObjectKind,
    analyze_plan,
};
use crate::plans::dialects::PlanDialect;
use crate::plans::glossary::OperatorCategory;
use crate::plans::model::{ExecutionPlan, PlanNode};
use crate::plans::phrases::{
    Arg, PlanPhrase, human_cost, human_duration_ms, human_factor, human_number, human_share,
    human_size_kb, phrase, rows_phrase, say, say0, truncate,
};

/// What a note under a step is about, so a renderer can pick an icon or
/// filter notes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StoryNoteKind {
    /// The condition an index search, filter or join uses.
    Predicate,
    /// Rows read and then discarded.
    RowsRemoved,
    /// The step ran more than once.
    Loops,
    /// The step's own time.
    Time,
    /// The step's share of the estimated cost.
    Cost,
    /// The row estimate was far from the actual count.
    Estimate,
    Memory,
    /// Work spilled to disk.
    Spill,
    /// Pages read.
    Io,
    /// Parallel workers.
    Parallel,
    /// The step is the top of a subplan or CTE.
    Subplan,
    Partitions,
    /// Any other detail.
    Detail,
}

impl StoryNoteKind {
    pub fn key(self) -> &'static str {
        match self {
            Self::Predicate => "predicate",
            Self::RowsRemoved => "rows_removed",
            Self::Loops => "loops",
            Self::Time => "time",
            Self::Cost => "cost",
            Self::Estimate => "estimate",
            Self::Memory => "memory",
            Self::Spill => "spill",
            Self::Io => "io",
            Self::Parallel => "parallel",
            Self::Subplan => "subplan",
            Self::Partitions => "partitions",
            Self::Detail => "detail",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct StoryNote {
    pub kind: StoryNoteKind,
    pub message: DisplayMessage,
}

/// One operator, as one numbered step.
#[derive(Debug, Clone, PartialEq)]
pub struct StoryStep {
    /// 1-based position in execution order. Other steps refer to it.
    pub number: usize,
    /// The plan node this step describes.
    pub node_id: i32,
    /// Depth of the node in the plan tree (0 for a root).
    pub depth: usize,
    /// Operator and what it touches, e.g. `Seq Scan on orders`.
    pub title: DisplayMessage,
    /// What the step does, with real names and row counts.
    pub sentence: DisplayMessage,
    /// Further facts worth a line each: predicate, discarded rows, loops,
    /// time, estimate misses, memory, spills.
    pub notes: Vec<StoryNote>,
    /// Step numbers of this step's direct inputs.
    pub inputs: Vec<usize>,
    /// The step's own share of the plan (0..1) by `share_metric`.
    pub share: Option<f64>,
    /// Which metric `share` is a share of: time when the plan is timed,
    /// otherwise estimated cost.
    pub share_metric: PlanMetric,
    pub is_bottleneck: bool,
    /// Bookkeeping steps (computing expressions, handing rows to the client)
    /// a renderer may fold away by default.
    pub minor: bool,
    pub never_executed: bool,
}

/// The step that dominates the plan.
#[derive(Debug, Clone, PartialEq)]
pub struct StoryBottleneck {
    pub node_id: i32,
    /// Step number in [`PlanStory::steps`].
    pub step: usize,
    /// Its own share of the plan's time (or estimated cost), 0..1.
    pub share: f64,
    /// One sentence naming the step and its share.
    pub message: DisplayMessage,
    /// One more sentence with the fact that best explains it, when the plan
    /// carries one (rows read versus returned, loops, a spill, a bad
    /// estimate).
    pub detail: Option<DisplayMessage>,
    /// The step dominates: its share is large and no other step is close
    /// behind, so the message calls it the bottleneck. When false the
    /// message says no single step dominates and only names the largest.
    pub dominant: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PlanStory {
    /// The verdict: what the query did and how long it took or is expected
    /// to cost.
    pub headline: DisplayMessage,
    /// A few sentences on the plan as a whole.
    pub summary: Vec<DisplayMessage>,
    pub bottleneck: Option<StoryBottleneck>,
    /// One step per plan node, in execution order.
    pub steps: Vec<StoryStep>,
}

impl PlanStory {
    /// The step that describes `node_id`.
    pub fn step_for_node(&self, node_id: i32) -> Option<&StoryStep> {
        self.steps.iter().find(|step| step.node_id == node_id)
    }

    /// The operator the story calls the bottleneck, if it calls one. The
    /// analysis always marks the largest step (`flags.bottleneck`); this is
    /// `None` when that step does not dominate, so a viewer that labels it
    /// "bottleneck" agrees with the sentence the story wrote.
    pub fn named_bottleneck(&self) -> Option<i32> {
        self.bottleneck
            .as_ref()
            .filter(|bottleneck| bottleneck.dominant)
            .map(|bottleneck| bottleneck.node_id)
    }

    /// The story as plain English text (the fallback strings), for copying
    /// to the clipboard and for tests.
    pub fn to_text(&self) -> String {
        let mut out = String::new();
        out.push_str(self.headline.fallback());
        out.push('\n');
        if let Some(bottleneck) = &self.bottleneck {
            out.push_str(bottleneck.message.fallback());
            if let Some(detail) = &bottleneck.detail {
                out.push(' ');
                out.push_str(detail.fallback());
            }
            out.push('\n');
        }
        for line in &self.summary {
            out.push_str(line.fallback());
            out.push('\n');
        }
        for step in &self.steps {
            out.push('\n');
            out.push_str(&format!(
                "{}. {}: {}",
                step.number,
                step.title.fallback(),
                step.sentence.fallback()
            ));
            for note in &step.notes {
                out.push(' ');
                out.push_str(note.message.fallback());
            }
        }
        out.push('\n');
        out
    }
}

/// One statement of a batch.
#[derive(Debug, Clone, PartialEq)]
pub struct StatementStory {
    /// 0-based position in the batch.
    pub index: usize,
    /// This statement's share of the batch's time (or estimated cost), 0..1.
    pub share: Option<f64>,
    pub analysis: PlanAnalysis,
    pub story: PlanStory,
}

/// A multi-statement document: one story per statement plus a line saying
/// which statement matters.
#[derive(Debug, Clone, PartialEq)]
pub struct BatchStory {
    /// `None` for a single statement.
    pub headline: Option<DisplayMessage>,
    pub statements: Vec<StatementStory>,
}

/// Analyse and narrate every statement of a batch.
pub fn batch_story(plans: &[ExecutionPlan]) -> BatchStory {
    let analyses: Vec<PlanAnalysis> = plans.iter().map(analyze_plan).collect();
    let timed = !analyses.is_empty()
        && analyses
            .iter()
            .all(|analysis| batch_time(analysis).is_some());
    let costed = !analyses.is_empty()
        && analyses
            .iter()
            .all(|analysis| analysis.total_cost.is_some());
    let weights: Vec<Option<f64>> = analyses
        .iter()
        .map(|analysis| {
            if timed {
                batch_time(analysis)
            } else if costed {
                analysis.total_cost
            } else {
                None
            }
        })
        .collect();
    let total: f64 = weights.iter().flatten().sum();
    let shares: Vec<Option<f64>> = weights
        .iter()
        .map(|weight| weight.filter(|_| total > 0.0).map(|weight| weight / total))
        .collect();
    let mut headline = None;
    if plans.len() > 1 {
        let heaviest = shares
            .iter()
            .enumerate()
            .filter_map(|(index, share)| share.map(|share| (index, share)))
            .fold(None, |best: Option<(usize, f64)>, item| match best {
                Some(best) if best.1 >= item.1 => Some(best),
                _ => Some(item),
            });
        headline = Some(match heaviest {
            Some((index, share)) => say(
                if timed {
                    "story.batch.heaviest_time"
                } else {
                    "story.batch.heaviest_cost"
                },
                vec![
                    ("count", plans.len().into()),
                    ("statement", (index + 1).into()),
                    ("share", human_share(share).into()),
                ],
            ),
            None => say("story.batch.plain", vec![("count", plans.len().into())]),
        });
    }
    let statements = plans
        .iter()
        .zip(analyses)
        .zip(shares)
        .enumerate()
        .map(|(index, ((plan, analysis), share))| {
            let story = plan_story(plan, &analysis);
            StatementStory {
                index,
                share,
                analysis,
                story,
            }
        })
        .collect();
    BatchStory {
        headline,
        statements,
    }
}

/// [`batch_story`] with query aliases resolved to real table names first
/// (MySQL, MariaDB and SQLite print only the alias); see
/// [`resolve_batch_aliases`](crate::plans::context::resolve_batch_aliases).
pub fn batch_story_with_context(
    plans: &[ExecutionPlan],
    context: &crate::plans::context::PlanContext<'_>,
) -> BatchStory {
    if plans.iter().any(crate::plans::context::needs_resolution) {
        let mut resolved = plans.to_vec();
        crate::plans::context::resolve_batch_aliases(&mut resolved, context);
        batch_story(&resolved)
    } else {
        batch_story(plans)
    }
}

fn batch_time(analysis: &PlanAnalysis) -> Option<f64> {
    analysis.execution_time_ms.or(analysis.total_time_ms)
}

/// Tell the story of one statement's plan.
pub fn plan_story(plan: &ExecutionPlan, analysis: &PlanAnalysis) -> PlanStory {
    let mut step_of = vec![0usize; analysis.nodes.len()];
    for (number, node_id) in analysis.execution_order.iter().enumerate() {
        if let Some(position) = analysis.position(*node_id) {
            step_of[position] = number + 1;
        }
    }
    let mut aliases: HashMap<String, Option<String>> = HashMap::new();
    let mut aliased_twice = HashSet::new();
    for node in &analysis.nodes {
        let Some(object) = &node.object else {
            continue;
        };
        let name = object.name.to_ascii_lowercase();
        let alias = object
            .alias
            .as_ref()
            .map(|alias| alias.to_ascii_lowercase());
        match aliases.get(&name) {
            Some(known) if *known != alias => {
                aliased_twice.insert(name);
            }
            Some(_) => {}
            None => {
                aliases.insert(name, alias);
            }
        }
    }
    let teller = Teller {
        plan,
        analysis,
        step_of,
        aliased_twice,
    };
    let steps: Vec<StoryStep> = analysis
        .execution_order
        .iter()
        .filter_map(|node_id| analysis.position(*node_id))
        .map(|position| teller.step(position))
        .collect();
    let (bottleneck, detail_is_estimate) = match teller.bottleneck() {
        Some((bottleneck, detail_is_estimate)) => (Some(bottleneck), detail_is_estimate),
        None => (None, false),
    };
    let bottleneck_with_estimate = bottleneck
        .as_ref()
        .filter(|_| detail_is_estimate)
        .map(|bottleneck| bottleneck.node_id);
    PlanStory {
        headline: teller.headline(),
        summary: teller.summary(bottleneck_with_estimate),
        bottleneck,
        steps,
    }
}

/// Longest predicate shown inside a sentence.
const PREDICATE_CHARS: usize = 160;
const KEYS_CHARS: usize = 120;
/// A step's own time or cost is mentioned from this share up.
const NOTABLE_SHARE: f64 = 0.05;
/// Estimate misses are mentioned from this factor up.
const NOTABLE_ESTIMATE_FACTOR: f64 = 5.0;
/// Below this share the largest step is not called a bottleneck.
const DOMINANT_SHARE: f64 = 0.25;
/// A second step with at least this fraction of the largest step's share is
/// named next to it.
const CLOSE_SECOND: f64 = 0.85;

/// Which of the three tellings a sentence uses.
#[derive(Clone, Copy)]
enum Telling {
    /// Measured rows.
    Actual(f64),
    /// Estimated rows.
    Expected(f64),
    /// The plan carries no row counts.
    Plain,
}

struct Teller<'a> {
    plan: &'a ExecutionPlan,
    analysis: &'a PlanAnalysis,
    /// Step number per node position.
    step_of: Vec<usize>,
    /// Lower-cased names of tables read under two or more aliases.
    aliased_twice: HashSet<String>,
}

impl Teller<'_> {
    fn raw(&self, position: usize) -> &PlanNode {
        &self.plan.nodes[position]
    }

    fn step_number(&self, node_id: i32) -> usize {
        self.analysis
            .position(node_id)
            .map(|position| self.step_of[position])
            .unwrap_or(0)
    }

    fn telling(&self, node: &NodeAnalysis) -> Telling {
        if let Some(rows) = node.metrics.rows_out {
            return Telling::Actual(rows);
        }
        if !self.analysis.has_actuals
            && let Some(rows) = node.metrics.estimated_rows_total
            && self.analysis.has_estimates
            && rows > 0.0
        {
            return Telling::Expected(rows.max(1.0));
        }
        Telling::Plain
    }

    fn sentence(
        &self,
        kind: &str,
        telling: Telling,
        mut args: Vec<(&'static str, Arg)>,
    ) -> DisplayMessage {
        let suffix = match telling {
            Telling::Actual(rows) => {
                args.push(("rows", rows_phrase(rows).into()));
                "actual"
            }
            Telling::Expected(rows) => {
                args.push(("rows", rows_phrase(rows).into()));
                "expected"
            }
            Telling::Plain => "plain",
        };
        say(&format!("story.step.{kind}.{suffix}"), args)
    }

    fn step(&self, position: usize) -> StoryStep {
        let analysis = self.analysis;
        let node = &analysis.nodes[position];
        let raw = self.raw(position);
        let category = node.category();
        let metrics = &node.metrics;
        let telling = self.telling(node);
        let inputs: Vec<usize> = node
            .children
            .iter()
            .map(|child| self.step_number(*child))
            .collect();
        let operator = || -> Arg { node.operator.name.clone().into() };
        let table = node.object.as_ref().map(|object| self.table_name(object));
        let index = node.index.clone();
        let mut rows_in_sentence = true;
        let mut predicate_in_sentence = false;

        let sentence = if node.flags.never_executed {
            say0("story.never_ran")
        } else {
            match category {
                OperatorCategory::Scan => match (&node.object, &index) {
                    (Some(object), _) if object.kind == PlanObjectKind::Cte => self.sentence(
                        "scan_cte",
                        telling,
                        vec![("table", self.table_name(object).into())],
                    ),
                    (Some(object), _) if object.kind == PlanObjectKind::Temporary => {
                        self.sentence("scan_temp", telling, Vec::new())
                    }
                    (Some(object), Some(index)) => self.sentence(
                        "scan_index",
                        telling,
                        vec![
                            ("table", self.table_name(object).into()),
                            ("index", index.into()),
                        ],
                    ),
                    (Some(object), None) => self.sentence(
                        "scan_table",
                        telling,
                        vec![("table", self.table_name(object).into())],
                    ),
                    (None, Some(index)) => {
                        self.sentence("scan_index_only", telling, vec![("index", index.into())])
                    }
                    (None, None) => self.sentence("other", telling, vec![("operator", operator())]),
                },
                OperatorCategory::Seek => match (&table, &index) {
                    (Some(table), Some(index)) => self.sentence(
                        "seek_index",
                        telling,
                        vec![("table", table.into()), ("index", index.into())],
                    ),
                    (Some(table), None) => {
                        self.sentence("seek_key", telling, vec![("table", table.into())])
                    }
                    (None, Some(index)) => {
                        self.sentence("seek_index_only", telling, vec![("index", index.into())])
                    }
                    (None, None) => self.sentence("other", telling, vec![("operator", operator())]),
                },
                OperatorCategory::Lookup => match &table {
                    Some(table) => self.sentence("lookup", telling, vec![("table", table.into())]),
                    None => self.sentence("other", telling, vec![("operator", operator())]),
                },
                OperatorCategory::JoinHash if inputs.len() == 2 => {
                    // `execution_order` already puts the build side first.
                    let (build, probe) = if inputs[0] < inputs[1] {
                        (inputs[0], inputs[1])
                    } else {
                        (inputs[1], inputs[0])
                    };
                    self.sentence(
                        "join_hash",
                        telling,
                        vec![("build", build.into()), ("probe", probe.into())],
                    )
                }
                OperatorCategory::JoinNestedLoop if inputs.len() == 2 => self.sentence(
                    "join_loop",
                    telling,
                    vec![("outer", inputs[0].into()), ("inner", inputs[1].into())],
                ),
                OperatorCategory::JoinMerge if inputs.len() == 2 => self.sentence(
                    "join_merge",
                    telling,
                    vec![("left", inputs[0].into()), ("right", inputs[1].into())],
                ),
                category if category.is_join() => {
                    if inputs.len() == 2 {
                        self.sentence(
                            "join_two",
                            telling,
                            vec![("left", inputs[0].into()), ("right", inputs[1].into())],
                        )
                    } else {
                        self.sentence("join_any", telling, Vec::new())
                    }
                }
                OperatorCategory::Sort => {
                    if node.sort_keys.is_empty() {
                        self.sentence("sort", telling, Vec::new())
                    } else {
                        self.sentence(
                            "sort_keys",
                            telling,
                            vec![("keys", keys_text(&node.sort_keys).into())],
                        )
                    }
                }
                OperatorCategory::Aggregate => {
                    let input = metrics
                        .rows_in
                        .filter(|_| matches!(telling, Telling::Actual(_)) && !inputs.is_empty());
                    match (node.group_keys.is_empty(), input, telling) {
                        (false, Some(input), Telling::Actual(rows)) => say(
                            "story.step_from.group_keys",
                            vec![
                                ("input", rows_phrase(input).into()),
                                ("keys", keys_text(&node.group_keys).into()),
                                ("rows", rows_phrase(rows).into()),
                            ],
                        ),
                        (true, Some(input), Telling::Actual(rows)) => say(
                            "story.step_from.aggregate",
                            vec![
                                ("input", rows_phrase(input).into()),
                                ("rows", rows_phrase(rows).into()),
                            ],
                        ),
                        (false, _, _) => self.sentence(
                            "group_keys",
                            telling,
                            vec![("keys", keys_text(&node.group_keys).into())],
                        ),
                        (true, _, _) => self.sentence("aggregate", telling, Vec::new()),
                    }
                }
                OperatorCategory::Window => self.sentence("window", telling, Vec::new()),
                OperatorCategory::Spool if node.operator.key.ends_with(".hash") => {
                    self.sentence("hash_build", telling, Vec::new())
                }
                // The glossary line says what this particular operator does
                // better than a sentence written for its whole category.
                OperatorCategory::Spool
                | OperatorCategory::Subquery
                | OperatorCategory::Function
                | OperatorCategory::Other
                    if node.operator.known =>
                {
                    rows_in_sentence = false;
                    node.operator.description.clone()
                }
                OperatorCategory::Result if node.parent_id.is_some() && node.operator.known => {
                    rows_in_sentence = false;
                    node.operator.description.clone()
                }
                OperatorCategory::Spool => self.sentence("spool", telling, Vec::new()),
                OperatorCategory::Parallelism => {
                    let key = node.operator.key;
                    if key.contains("gather") || key.contains("coordinator") {
                        self.sentence("gather", telling, Vec::new())
                    } else {
                        self.sentence("exchange", telling, Vec::new())
                    }
                }
                OperatorCategory::Filter => {
                    let input = metrics
                        .rows_in
                        .filter(|_| matches!(telling, Telling::Actual(_)) && !inputs.is_empty());
                    match (&node.filter, input, telling) {
                        (Some(filter), Some(input), Telling::Actual(rows)) => {
                            predicate_in_sentence = true;
                            say(
                                "story.step_from.filter_where",
                                vec![
                                    ("input", rows_phrase(input).into()),
                                    ("predicate", predicate_text(&filter.text).into()),
                                    ("rows", rows_phrase(rows).into()),
                                ],
                            )
                        }
                        (Some(filter), _, _) => {
                            predicate_in_sentence = true;
                            self.sentence(
                                "filter_where",
                                telling,
                                vec![("predicate", predicate_text(&filter.text).into())],
                            )
                        }
                        (None, Some(input), Telling::Actual(rows)) => say(
                            "story.step_from.filter",
                            vec![
                                ("input", rows_phrase(input).into()),
                                ("rows", rows_phrase(rows).into()),
                            ],
                        ),
                        (None, _, _) => self.sentence("filter", telling, Vec::new()),
                    }
                }
                OperatorCategory::Compute => self.sentence("compute", telling, Vec::new()),
                OperatorCategory::Modify => match &table {
                    Some(table) => self.sentence(
                        "modify_table",
                        telling,
                        vec![("operator", operator()), ("table", table.into())],
                    ),
                    None => self.sentence("modify", telling, vec![("operator", operator())]),
                },
                OperatorCategory::Remote => self.sentence("remote", telling, Vec::new()),
                OperatorCategory::SetOp => {
                    self.sentence("set_op", telling, vec![("operator", operator())])
                }
                OperatorCategory::Limit => self.sentence("limit", telling, Vec::new()),
                OperatorCategory::Subquery => self.sentence("subquery", telling, Vec::new()),
                OperatorCategory::Function => {
                    self.sentence("function", telling, vec![("operator", operator())])
                }
                OperatorCategory::Constant => self.sentence("constant", telling, Vec::new()),
                OperatorCategory::Result if node.parent_id.is_none() => {
                    self.sentence("result", telling, Vec::new())
                }
                OperatorCategory::Lock => self.sentence("lock", telling, Vec::new()),
                _ => self.sentence("other", telling, vec![("operator", operator())]),
            }
        };

        // ---- Notes --------------------------------------------------------
        let mut notes: Vec<StoryNote> = Vec::new();
        let mut note = |kind: StoryNoteKind, message: DisplayMessage| {
            notes.push(StoryNote { kind, message });
        };
        if !rows_in_sentence && !node.flags.never_executed {
            match telling {
                Telling::Actual(rows) => note(
                    StoryNoteKind::Detail,
                    say("story.note.rows", vec![("rows", rows_phrase(rows).into())]),
                ),
                Telling::Expected(rows) => note(
                    StoryNoteKind::Detail,
                    say(
                        "story.note.rows_expected",
                        vec![("rows", rows_phrase(rows).into())],
                    ),
                ),
                Telling::Plain => {}
            }
        }
        if let Some(name) = node
            .subplan_name
            .as_ref()
            .filter(|_| node.parent_id.is_some())
        {
            let lowered = name.to_ascii_lowercase();
            let relationship = raw
                .parent_relationship
                .as_deref()
                .unwrap_or("")
                .to_ascii_lowercase();
            let key = if lowered.starts_with("cte ") {
                "story.note.subplan_cte"
            } else if lowered.starts_with("initplan") || relationship == "initplan" {
                "story.note.subplan_init"
            } else {
                "story.note.subplan"
            };
            let shown = match key {
                "story.note.subplan_cte" => name.get(4..).unwrap_or(name).trim().to_string(),
                _ => name.clone(),
            };
            note(
                StoryNoteKind::Subplan,
                say(key, vec![("name", shown.into())]),
            );
        }
        if !node.flags.never_executed {
            if let Some(seek) = node
                .seek_predicate
                .as_ref()
                .filter(|seek| !compares_with_itself(&seek.text))
            {
                let key = if node.index.is_some() {
                    "story.note.seek_index"
                } else {
                    "story.note.seek"
                };
                note(
                    StoryNoteKind::Predicate,
                    say(key, vec![("predicate", predicate_text(&seek.text).into())]),
                );
            }
            if let Some(join) = &node.join_predicate {
                note(
                    StoryNoteKind::Predicate,
                    say(
                        "story.note.join_on",
                        vec![("predicate", predicate_text(&join.text).into())],
                    ),
                );
            }
            if let Some(kind) = node.join_type.as_deref().and_then(join_type_note) {
                note(StoryNoteKind::Detail, say0(kind));
            }
            let removed = metrics.rows_removed.zip(metrics.rows_removed_share);
            // A join that repeats its join condition as a filter says it once.
            let filter_said = predicate_in_sentence
                || matches!(
                    (&node.filter, &node.join_predicate),
                    (Some(filter), Some(join)) if filter.text == join.text
                );
            // A SQL Server bitmap test is the join's doing, not a condition
            // the query wrote.
            let probe = node
                .filter
                .as_ref()
                .filter(|_| !filter_said)
                .and_then(|filter| crate::plans::predicate::bitmap_probe(&filter.text));
            let own_filter: Option<String> = match &probe {
                Some(probe) => probe.rest.clone(),
                None => node.filter.as_ref().map(|filter| filter.text.clone()),
            };
            if let Some(probe) = &probe {
                let columns = ("columns", Arg::from(list_phrase(&probe.columns, 3)));
                match (removed, &probe.rest) {
                    (Some((removed, share)), None) => note(
                        StoryNoteKind::RowsRemoved,
                        say(
                            "story.note.bitmap_filter_removed",
                            vec![
                                columns,
                                ("removed", rows_phrase(removed).into()),
                                ("share", human_share(share).into()),
                            ],
                        ),
                    ),
                    _ => note(
                        StoryNoteKind::Predicate,
                        say("story.note.bitmap_filter", vec![columns]),
                    ),
                }
            }
            let removed =
                removed.filter(|_| !matches!(&probe, Some(probe) if probe.rest.is_none()));
            match (&own_filter, removed, filter_said) {
                (None, None, _) => {}
                (Some(filter), Some((removed, share)), false) => note(
                    StoryNoteKind::RowsRemoved,
                    say(
                        "story.note.filter_removed",
                        vec![
                            ("predicate", predicate_text(filter).into()),
                            ("removed", rows_phrase(removed).into()),
                            ("share", human_share(share).into()),
                        ],
                    ),
                ),
                (Some(filter), None, false) => note(
                    StoryNoteKind::Predicate,
                    say(
                        "story.note.filter",
                        vec![("predicate", predicate_text(filter).into())],
                    ),
                ),
                (_, Some((removed, share)), _) => note(
                    StoryNoteKind::RowsRemoved,
                    say(
                        "story.note.removed",
                        vec![
                            ("removed", rows_phrase(removed).into()),
                            ("share", human_share(share).into()),
                        ],
                    ),
                ),
                _ => {}
            }
            if let Some(loops) = metrics.loops.filter(|loops| *loops >= 1.5)
                && let Some(per_loop) = metrics.rows_per_loop
            {
                let key = if metrics.processes.is_some() {
                    "story.note.processes"
                } else {
                    "story.note.loops"
                };
                note(
                    StoryNoteKind::Loops,
                    say(
                        key,
                        vec![
                            ("loops", human_number(loops).into()),
                            ("rows", rows_phrase(per_loop).into()),
                        ],
                    ),
                );
            }
            if let (Some(scanned), Some(total)) = (raw.partitions_scanned, raw.partitions_total)
                && total > 0
            {
                note(
                    StoryNoteKind::Partitions,
                    say(
                        "story.note.partitions",
                        vec![
                            ("scanned", scanned.to_string().into()),
                            ("total", total.to_string().into()),
                        ],
                    ),
                );
            }
            if category == OperatorCategory::Parallelism
                && let Some(planned) = raw.workers_planned
            {
                match raw.workers_launched {
                    Some(launched) if launched < planned => note(
                        StoryNoteKind::Parallel,
                        say(
                            "story.note.workers_short",
                            vec![
                                ("launched", launched.to_string().into()),
                                ("planned", planned.to_string().into()),
                            ],
                        ),
                    ),
                    Some(launched) => note(
                        StoryNoteKind::Parallel,
                        say(
                            "story.note.workers",
                            vec![("launched", launched.to_string().into())],
                        ),
                    ),
                    None => note(
                        StoryNoteKind::Parallel,
                        say(
                            "story.note.workers_planned",
                            vec![("planned", planned.to_string().into())],
                        ),
                    ),
                }
            }
            if let Some(message) = self.estimate_note(node) {
                note(StoryNoteKind::Estimate, message);
            }
            if node.flags.spilled {
                let message = match metrics.spilled_kb.filter(|kb| *kb > 0.0) {
                    Some(kb) => say(
                        "story.note.spill_size",
                        vec![("size", human_size_kb(kb).into())],
                    ),
                    None => match raw.hash_batches.filter(|batches| *batches > 1) {
                        Some(batches) => say(
                            "story.note.spill_batches",
                            vec![("batches", batches.to_string().into())],
                        ),
                        None => say0("story.note.spill"),
                    },
                };
                note(StoryNoteKind::Spill, message);
            } else if let Some(memory) = metrics.memory_kb.filter(|kb| *kb >= 1024.0) {
                note(
                    StoryNoteKind::Memory,
                    say(
                        "story.note.memory",
                        vec![("size", human_size_kb(memory).into())],
                    ),
                );
            }
            if let (Some(pages), Some(share)) = (metrics.io_pages_self, metrics.io_share)
                && share >= 0.10
                && pages >= 1.0
            {
                note(
                    StoryNoteKind::Io,
                    say(
                        "story.note.io",
                        vec![
                            ("pages", human_number(pages).into()),
                            ("share", human_share(share).into()),
                        ],
                    ),
                );
            }
        }
        let (share, share_metric) = match analysis.primary_metric {
            PlanMetric::Time => (metrics.time_share, PlanMetric::Time),
            _ => (metrics.cost_share, PlanMetric::Cost),
        };
        if !node.flags.never_executed
            && let Some(share) = share
            && (share >= NOTABLE_SHARE || node.flags.bottleneck)
        {
            match share_metric {
                PlanMetric::Time => {
                    if let Some(time) = metrics.self_time_ms {
                        note(
                            StoryNoteKind::Time,
                            say(
                                "story.note.time",
                                vec![
                                    ("time", human_duration_ms(time).into()),
                                    ("share", human_share(share).into()),
                                ],
                            ),
                        );
                    }
                }
                _ => note(
                    StoryNoteKind::Cost,
                    say(
                        "story.note.cost",
                        vec![("share", human_share(share).into())],
                    ),
                ),
            }
        }

        let minor = !node.flags.bottleneck
            && share.unwrap_or(0.0) < NOTABLE_SHARE
            && notes.is_empty()
            && (matches!(
                category,
                OperatorCategory::Compute | OperatorCategory::Result | OperatorCategory::Lock
            ) || node.operator.key == "spark.adaptive_spark_plan");
        StoryStep {
            number: self.step_of[position],
            node_id: node.node_id,
            depth: node.depth,
            title: node.title(),
            sentence,
            notes,
            inputs,
            share,
            share_metric,
            is_bottleneck: node.flags.bottleneck,
            minor,
            never_executed: node.flags.never_executed,
        }
    }

    fn estimate_note(&self, node: &NodeAnalysis) -> Option<DisplayMessage> {
        let metrics = &node.metrics;
        let factor = metrics.estimate_factor?;
        let direction = metrics.estimate_direction?;
        let estimated = metrics.estimated_rows?;
        let actual = metrics.estimate_compared_rows?;
        // Operators above a bad estimate inherit it; only the step where it
        // starts says so.
        if !node.flags.misestimate_origin
            || factor < NOTABLE_ESTIMATE_FACTOR
            || (estimated - actual).abs() < 50.0
        {
            return None;
        }
        let looped = metrics.loops.is_some_and(|loops| loops >= 1.5);
        let per_process = metrics.processes.is_some();
        let key = match (direction, looped, per_process) {
            (EstimateDirection::Under, _, true) => "story.note.estimate_under_process",
            (EstimateDirection::Over, _, true) => "story.note.estimate_over_process",
            (EstimateDirection::Under, false, _) => "story.note.estimate_under",
            (EstimateDirection::Under, true, _) => "story.note.estimate_under_loop",
            (EstimateDirection::Over, false, _) => "story.note.estimate_over",
            (EstimateDirection::Over, true, _) => "story.note.estimate_over_loop",
            (EstimateDirection::Exact, _, _) => return None,
        };
        Some(say(
            key,
            vec![
                ("estimated", rows_phrase(estimated).into()),
                ("actual", rows_phrase(actual).into()),
                ("factor", human_factor(factor).into()),
            ],
        ))
    }

    /// How a sentence names a table: its name, plus the alias only when the
    /// plan reads the same table under more than one alias.
    fn table_name(&self, object: &PlanObject) -> String {
        if self
            .aliased_twice
            .contains(&object.name.to_ascii_lowercase())
        {
            object.label()
        } else {
            object.name.clone()
        }
    }

    /// Tables the plan reads or writes, in execution order, without repeats.
    fn tables(&self) -> Vec<String> {
        let mut tables: Vec<String> = Vec::new();
        for node_id in &self.analysis.execution_order {
            let Some(node) = self.analysis.node(*node_id) else {
                continue;
            };
            let Some(object) = &node.object else {
                continue;
            };
            if object.kind != PlanObjectKind::Table {
                continue;
            }
            if !tables
                .iter()
                .any(|known| known.eq_ignore_ascii_case(&object.name))
            {
                tables.push(object.name.clone());
            }
        }
        tables
    }

    fn headline(&self) -> DisplayMessage {
        let analysis = self.analysis;
        let tables = self.tables();
        let table_list = (!tables.is_empty()).then(|| list_phrase(&tables, 4));
        let modified = analysis
            .nodes
            .iter()
            .find(|node| node.category() == OperatorCategory::Modify)
            .and_then(|node| node.object.as_ref())
            .map(|object| object.name.clone());
        let time = analysis.execution_time_ms.or(analysis.total_time_ms);
        if analysis.has_actuals {
            if let Some(table) = modified {
                return match time {
                    Some(time) => say(
                        "story.headline.modify_time",
                        vec![
                            ("table", table.into()),
                            ("time", human_duration_ms(time).into()),
                        ],
                    ),
                    None => say("story.headline.modify", vec![("table", table.into())]),
                };
            }
            let rows = analysis.result_rows.map(rows_phrase);
            return match (table_list, rows, time) {
                (Some(tables), Some(rows), Some(time)) => say(
                    "story.headline.actual_tables_rows_time",
                    vec![
                        ("tables", tables.into()),
                        ("rows", rows.into()),
                        ("time", human_duration_ms(time).into()),
                    ],
                ),
                (None, Some(rows), Some(time)) => say(
                    "story.headline.actual_rows_time",
                    vec![
                        ("rows", rows.into()),
                        ("time", human_duration_ms(time).into()),
                    ],
                ),
                (Some(tables), Some(rows), None) => say(
                    "story.headline.actual_tables_rows",
                    vec![("tables", tables.into()), ("rows", rows.into())],
                ),
                (None, Some(rows), None) => {
                    say("story.headline.actual_rows", vec![("rows", rows.into())])
                }
                (Some(tables), None, Some(time)) => say(
                    "story.headline.actual_tables_time",
                    vec![
                        ("tables", tables.into()),
                        ("time", human_duration_ms(time).into()),
                    ],
                ),
                (None, None, Some(time)) => say(
                    "story.headline.actual_time",
                    vec![("time", human_duration_ms(time).into())],
                ),
                (Some(tables), None, None) => say(
                    "story.headline.plain_tables",
                    vec![("tables", tables.into())],
                ),
                (None, None, None) => say(
                    "story.headline.plain",
                    vec![("steps", analysis.nodes.len().into())],
                ),
            };
        }
        let rows = analysis
            .result_rows
            .filter(|_| analysis.has_estimates)
            .map(|rows| rows_phrase(rows.max(1.0)));
        let cost = analysis.total_cost.map(human_cost);
        if let Some(table) = modified {
            return match cost {
                Some(cost) => say(
                    "story.headline.expected_modify_cost",
                    vec![("table", table.into()), ("cost", cost.into())],
                ),
                None => say(
                    "story.headline.expected_modify",
                    vec![("table", table.into())],
                ),
            };
        }
        match (table_list, rows, cost) {
            (Some(tables), Some(rows), Some(cost)) => say(
                "story.headline.expected_tables_rows_cost",
                vec![
                    ("tables", tables.into()),
                    ("rows", rows.into()),
                    ("cost", cost.into()),
                ],
            ),
            (None, Some(rows), Some(cost)) => say(
                "story.headline.expected_rows_cost",
                vec![("rows", rows.into()), ("cost", cost.into())],
            ),
            (Some(tables), Some(rows), None) => say(
                "story.headline.expected_tables_rows",
                vec![("tables", tables.into()), ("rows", rows.into())],
            ),
            (None, Some(rows), None) => {
                say("story.headline.expected_rows", vec![("rows", rows.into())])
            }
            (Some(tables), None, Some(cost)) => say(
                "story.headline.expected_tables_cost",
                vec![("tables", tables.into()), ("cost", cost.into())],
            ),
            (None, None, Some(cost)) => {
                say("story.headline.expected_cost", vec![("cost", cost.into())])
            }
            (Some(tables), None, None) => say(
                "story.headline.plain_tables",
                vec![("tables", tables.into())],
            ),
            (None, None, None) => say(
                "story.headline.plain",
                vec![("steps", analysis.nodes.len().into())],
            ),
        }
    }

    /// The call-out, and whether its detail sentence is the estimate miss.
    fn bottleneck(&self) -> Option<(StoryBottleneck, bool)> {
        let analysis = self.analysis;
        let node = analysis.node(analysis.bottleneck?)?;
        let position = analysis.position(node.node_id)?;
        let step = self.step_of[position];
        let title = node.title();
        let metrics = &node.metrics;
        // The next-largest step: when it is close behind, or the largest is
        // small, nothing is called "the" bottleneck.
        let runner_up = analysis
            .nodes
            .iter()
            .filter(|other| other.node_id != node.node_id)
            .filter_map(|other| {
                let share = match analysis.primary_metric {
                    PlanMetric::Time => other.metrics.time_share,
                    _ => other.metrics.cost_share,
                }?;
                Some((other, share))
            })
            .fold(
                None,
                |best: Option<(&NodeAnalysis, f64)>, item| match best {
                    Some(best) if best.1 >= item.1 => Some(best),
                    _ => Some(item),
                },
            )
            .filter(|(_, share)| *share >= NOTABLE_SHARE);
        let (share, timed) = match analysis.primary_metric {
            PlanMetric::Time => (metrics.time_share?, true),
            PlanMetric::Cost => (metrics.cost_share?, false),
            _ => return None,
        };
        let close_second = runner_up.filter(|(_, other)| *other >= share * CLOSE_SECOND);
        let spread = share < DOMINANT_SHARE || close_second.is_some();
        let mut args: Vec<(&'static str, Arg)> = vec![
            ("step", step.into()),
            ("title", title.into()),
            ("share", human_share(share).into()),
        ];
        if timed {
            args.push(("time", human_duration_ms(metrics.self_time_ms?).into()));
        }
        let second = if spread { runner_up } else { None };
        if let Some((other, other_share)) = second {
            args.push(("other_step", self.step_number(other.node_id).into()));
            args.push(("other_title", other.title().into()));
            args.push(("other_share", human_share(other_share).into()));
        }
        let key = match (timed, spread, second.is_some()) {
            (true, false, _) => "story.bottleneck.time",
            (true, true, false) => "story.bottleneck.time_spread",
            (true, true, true) => "story.bottleneck.time_spread_two",
            (false, false, _) => "story.bottleneck.cost",
            (false, true, false) => "story.bottleneck.cost_spread",
            (false, true, true) => "story.bottleneck.cost_spread_two",
        };
        let message = say(key, args);
        let mut detail_is_estimate = false;
        let detail = {
            let read = metrics.rows_in.filter(|_| node.children.is_empty());
            let removed_share = metrics.rows_removed_share.unwrap_or(0.0);
            if let (Some(read), Some(out)) = (read, metrics.rows_out)
                && removed_share >= 0.5
            {
                Some(say(
                    "story.bottleneck.read_vs_returned",
                    vec![
                        ("read", rows_phrase(read).into()),
                        ("rows", rows_phrase(out).into()),
                    ],
                ))
            } else if node.flags.spilled {
                Some(say0("story.bottleneck.spilled"))
            } else if let Some(loops) = metrics.loops.filter(|loops| *loops >= 100.0) {
                Some(say(
                    "story.bottleneck.loops",
                    vec![("loops", human_number(loops).into())],
                ))
            } else {
                let estimate = self.estimate_note(node);
                detail_is_estimate = estimate.is_some();
                estimate
            }
        };
        Some((
            StoryBottleneck {
                node_id: node.node_id,
                step,
                share,
                message,
                detail,
                dominant: !spread,
            },
            detail_is_estimate,
        ))
    }

    fn summary(&self, bottleneck_with_estimate: Option<i32>) -> Vec<DisplayMessage> {
        let analysis = self.analysis;
        let plan = self.plan;
        let mut lines = Vec::new();
        if !analysis.has_actuals {
            lines.push(if analysis.has_estimates || analysis.has_costs {
                say0("story.summary.estimated_only")
            } else {
                say0("story.summary.no_numbers")
            });
        } else if analysis.timing_too_coarse {
            let resolution = analysis.time_resolution_ms.unwrap_or_default();
            lines.push(say(
                if analysis.has_costs {
                    "story.summary.coarse_timing_cost"
                } else {
                    "story.summary.coarse_timing"
                },
                vec![("resolution", human_duration_ms(resolution).into())],
            ));
        } else if !analysis.has_timing && analysis.has_costs {
            lines.push(say0("story.summary.no_timing"));
        }
        if let (Some(planning), Some(execution)) =
            (analysis.planning_time_ms, analysis.execution_time_ms)
            && planning >= 1.0
        {
            lines.push(say(
                "story.summary.planning",
                vec![
                    ("planning", human_duration_ms(planning).into()),
                    ("execution", human_duration_ms(execution).into()),
                ],
            ));
        }
        if let (Some(total), Some(elapsed)) = (analysis.total_time_ms, analysis.execution_time_ms)
            && total > elapsed * 1.2
            && (analysis.dialect == PlanDialect::DuckDb
                || analysis.nodes.iter().any(|node| node.flags.parallel))
        {
            lines.push(say(
                "story.summary.thread_time",
                vec![
                    ("total", human_duration_ms(total).into()),
                    ("elapsed", human_duration_ms(elapsed).into()),
                ],
            ));
        }
        // The worst estimate, when it is not already the bottleneck's detail.
        if analysis.has_actuals {
            let worst = analysis
                .nodes
                .iter()
                .filter(|node| !node.flags.never_executed && node.flags.misestimate_origin)
                .filter(|node| Some(node.node_id) != bottleneck_with_estimate)
                .filter(|node| {
                    let metrics = &node.metrics;
                    match (metrics.estimated_rows, metrics.estimate_compared_rows) {
                        (Some(estimated), Some(actual)) => (estimated - actual).abs() >= 50.0,
                        _ => false,
                    }
                })
                .filter_map(|node| node.metrics.estimate_factor.map(|factor| (node, factor)))
                .filter(|(_, factor)| *factor >= 10.0)
                .fold(
                    None,
                    |best: Option<(&NodeAnalysis, f64)>, item| match best {
                        Some(best) if best.1 >= item.1 => Some(best),
                        _ => Some(item),
                    },
                );
            if let Some((node, factor)) = worst
                && let (Some(estimated), Some(actual), Some(direction)) = (
                    node.metrics.estimated_rows,
                    node.metrics.estimate_compared_rows,
                    node.metrics.estimate_direction,
                )
            {
                let key = match direction {
                    EstimateDirection::Under => "story.summary.worst_estimate_under",
                    _ => "story.summary.worst_estimate_over",
                };
                lines.push(say(
                    key,
                    vec![
                        ("step", self.step_number(node.node_id).into()),
                        ("title", node.title().into()),
                        ("estimated", rows_phrase(estimated).into()),
                        ("actual", rows_phrase(actual).into()),
                        ("factor", human_factor(factor).into()),
                    ],
                ));
            }
        }
        let spilled: Vec<String> = analysis
            .execution_order
            .iter()
            .filter(|node_id| {
                analysis
                    .node(**node_id)
                    .is_some_and(|node| node.flags.spilled)
            })
            .map(|node_id| self.step_number(*node_id).to_string())
            .collect();
        match spilled.len() {
            0 => {}
            1 => lines.push(say(
                "story.summary.spill_one",
                vec![("step", spilled[0].clone().into())],
            )),
            _ => lines.push(say(
                "story.summary.spill_many",
                vec![("steps", spilled.join(", ").into())],
            )),
        }
        if let Some(dop) = plan.degree_of_parallelism.filter(|dop| *dop > 1) {
            lines.push(say(
                "story.summary.parallel_dop",
                vec![("dop", dop.to_string().into())],
            ));
        } else if analysis.dialect != PlanDialect::SqlServer || plan.degree_of_parallelism.is_none()
        {
            let any_parallel = analysis.nodes.iter().any(|node| node.flags.parallel);
            if any_parallel && plan.degree_of_parallelism.is_none() {
                lines.push(say0("story.summary.parallel"));
            }
        }
        if let Some(grant) = &plan.memory_grant
            && let (Some(granted), Some(used)) = (grant.granted_kb, grant.max_used_kb)
            && granted >= 1024.0
        {
            lines.push(say(
                "story.summary.memory_grant",
                vec![
                    ("granted", human_size_kb(granted).into()),
                    ("used", human_size_kb(used).into()),
                ],
            ));
        }
        let never = analysis
            .nodes
            .iter()
            .filter(|node| node.flags.never_executed)
            .count();
        if never > 0 {
            lines.push(say(
                "story.summary.never_executed",
                vec![("count", never.into())],
            ));
        }
        lines
    }
}

/// `Orders.OrderID = Orders.OrderID`: a key lookup's seek on its own key,
/// which says nothing a reader can use.
fn compares_with_itself(predicate: &str) -> bool {
    predicate
        .split_once(" = ")
        .is_some_and(|(left, right)| left.trim() == right.trim())
}

fn predicate_text(text: &str) -> String {
    truncate(text.trim(), PREDICATE_CHARS)
}

fn keys_text(keys: &[String]) -> String {
    truncate(&keys.join(", "), KEYS_CHARS)
}

/// `a`, `a and b`, `a, b and c`, `a, b, c and 3 more`.
pub(crate) fn list_phrase(items: &[String], max: usize) -> DisplayMessage {
    match items {
        [] => DisplayMessage::raw(String::new()),
        [only] => DisplayMessage::raw(only.clone()),
        [first, second] => say(
            "story.list.two",
            vec![("first", first.into()), ("second", second.into())],
        ),
        _ if items.len() <= max => {
            let (last, head) = items.split_last().expect("at least three items");
            say(
                "story.list.many",
                vec![("head", head.join(", ").into()), ("last", last.into())],
            )
        }
        _ => say(
            "story.list.more",
            vec![
                ("head", items[..max].join(", ").into()),
                ("count", (items.len() - max).into()),
            ],
        ),
    }
}

/// The note that explains a non-inner join type, by the words engines use.
fn join_type_note(join_type: &str) -> Option<&'static str> {
    let lowered = join_type.to_ascii_lowercase();
    if lowered.contains("anti") {
        Some("story.note.join_anti")
    } else if lowered.contains("semi") {
        Some("story.note.join_semi")
    } else if lowered.contains("full") {
        Some("story.note.join_full")
    } else if lowered.contains("left") {
        Some("story.note.join_left")
    } else if lowered.contains("right") {
        Some("story.note.join_right")
    } else if lowered.contains("cross") {
        Some("story.note.join_cross")
    } else {
        None
    }
}

/// Three tellings of each step sentence: measured, estimated, no numbers.
macro_rules! step_phrases {
    ($( ($kind:literal, $what:literal, $actual:literal, $expected:literal, $plain:literal) ),* $(,)?) => {
        &[
            $(
                PlanPhrase {
                    key: concat!("services.plans.story.step.", $kind, ".actual"),
                    text: $actual,
                    location: concat!(
                        "Execution Plan → Story: what a step did, for ", $what,
                        ", on a plan that was executed. {rows} is a row-count phrase such as '8,210 rows'; step placeholders are step numbers; the others are names taken from the plan."
                    ),
                },
                PlanPhrase {
                    key: concat!("services.plans.story.step.", $kind, ".expected"),
                    text: $expected,
                    location: concat!(
                        "Execution Plan → Story: what the optimizer expects a step to do, for ", $what,
                        ", on an estimated plan. {rows} is a row-count phrase such as '8,210 rows'; step placeholders are step numbers; the others are names taken from the plan."
                    ),
                },
                PlanPhrase {
                    key: concat!("services.plans.story.step.", $kind, ".plain"),
                    text: $plain,
                    location: concat!(
                        "Execution Plan → Story: what a step does, for ", $what,
                        ", on a plan with no row counts. Step placeholders are step numbers; the others are names taken from the plan."
                    ),
                },
            )*
        ]
    };
}

pub(crate) static STEP_PHRASES: &[PlanPhrase] = step_phrases![
    (
        "scan_table",
        "a table scan",
        "Scans {table} and returns {rows}.",
        "Expects to scan {table} and return about {rows}.",
        "Scans {table}."
    ),
    (
        "scan_index",
        "a scan of an index",
        "Scans the index {index} on {table} and returns {rows}.",
        "Expects to scan the index {index} on {table} and return about {rows}.",
        "Scans the index {index} on {table}."
    ),
    (
        "scan_index_only",
        "a scan of an index when the plan does not name the table",
        "Scans the index {index} and returns {rows}.",
        "Expects to scan the index {index} and return about {rows}.",
        "Scans the index {index}."
    ),
    (
        "scan_cte",
        "a read of a CTE result",
        "Reads the result of the CTE {table} and returns {rows}.",
        "Expects to read about {rows} from the result of the CTE {table}.",
        "Reads the result of the CTE {table}."
    ),
    (
        "scan_temp",
        "a read of an internal temporary table",
        "Reads the internal temporary table and returns {rows}.",
        "Expects to read about {rows} from the internal temporary table.",
        "Reads the internal temporary table."
    ),
    (
        "seek_index",
        "an index seek or index lookup",
        "Uses the index {index} to find rows in {table} and returns {rows}.",
        "Expects to use the index {index} to find about {rows} in {table}.",
        "Uses the index {index} to find rows in {table}."
    ),
    (
        "seek_key",
        "a lookup by key when the plan names no index",
        "Finds rows in {table} by key and returns {rows}.",
        "Expects to find about {rows} in {table} by key.",
        "Finds rows in {table} by key."
    ),
    (
        "seek_index_only",
        "an index seek when the plan does not name the table",
        "Searches the index {index} and returns {rows}.",
        "Expects to search the index {index} and return about {rows}.",
        "Searches the index {index}."
    ),
    (
        "lookup",
        "a key lookup or row fetch after an index search",
        "Fetches the full rows from {table} for the entries an index found, returning {rows}.",
        "Expects to fetch about {rows} from {table} for the entries an index found.",
        "Fetches the full rows from {table} for the entries an index found."
    ),
    (
        "join_hash",
        "a hash join; {build} and {probe} are step numbers",
        "Builds a hash table from step {build}, probes it with the rows from step {probe} and returns {rows}.",
        "Expects to build a hash table from step {build}, probe it with the rows from step {probe} and return about {rows}.",
        "Builds a hash table from step {build} and probes it with the rows from step {probe}."
    ),
    (
        "join_loop",
        "a nested loop join; {outer} and {inner} are step numbers",
        "For each row from step {outer}, looks for matches in step {inner}, and returns {rows}.",
        "Expects to look for matches in step {inner} for each row from step {outer}, and to return about {rows}.",
        "For each row from step {outer}, looks for matches in step {inner}."
    ),
    (
        "join_merge",
        "a merge join; {left} and {right} are step numbers",
        "Walks the sorted rows of step {left} and step {right} side by side and returns {rows}.",
        "Expects to walk the sorted rows of step {left} and step {right} side by side and return about {rows}.",
        "Walks the sorted rows of step {left} and step {right} side by side."
    ),
    (
        "join_two",
        "a join whose algorithm the plan does not state; {left} and {right} are step numbers",
        "Joins step {left} with step {right} and returns {rows}.",
        "Expects to join step {left} with step {right} and return about {rows}.",
        "Joins step {left} with step {right}."
    ),
    (
        "join_any",
        "a join that does not have exactly two inputs",
        "Joins its inputs and returns {rows}.",
        "Expects to join its inputs and return about {rows}.",
        "Joins its inputs."
    ),
    (
        "sort_keys",
        "a sort; {keys} is the list of sort columns",
        "Sorts {rows} by {keys}.",
        "Expects to sort about {rows} by {keys}.",
        "Sorts its input by {keys}."
    ),
    (
        "sort",
        "a sort when the plan does not list the sort columns",
        "Sorts {rows}.",
        "Expects to sort about {rows}.",
        "Sorts its input."
    ),
    (
        "group_keys",
        "a GROUP BY or DISTINCT; {keys} is the list of grouping columns",
        "Groups its input by {keys}, producing {rows}.",
        "Expects to group its input by {keys} into about {rows}.",
        "Groups its input by {keys}."
    ),
    (
        "aggregate",
        "an aggregate without grouping columns",
        "Aggregates its input into {rows}.",
        "Expects to aggregate its input into about {rows}.",
        "Aggregates its input."
    ),
    (
        "window",
        "a window function operator",
        "Computes window functions over {rows}.",
        "Expects to compute window functions over about {rows}.",
        "Computes window functions."
    ),
    (
        "hash_build",
        "the hash-table build below a hash join",
        "Builds a hash table from {rows} for the join above it.",
        "Expects to build a hash table from about {rows} for the join above it.",
        "Builds a hash table for the join above it."
    ),
    (
        "spool",
        "a spool, materialize or temporary-table operator",
        "Stores {rows} so they can be read again without being recomputed.",
        "Expects to store about {rows} so they can be read again without being recomputed.",
        "Stores its rows so they can be read again without being recomputed."
    ),
    (
        "gather",
        "the operator that collects rows from parallel workers",
        "Collects {rows} from the parallel workers into one stream.",
        "Expects to collect about {rows} from the parallel workers into one stream.",
        "Collects the rows from the parallel workers into one stream."
    ),
    (
        "exchange",
        "an operator that moves rows between parallel workers",
        "Moves {rows} between parallel workers.",
        "Expects to move about {rows} between parallel workers.",
        "Moves rows between parallel workers."
    ),
    (
        "filter_where",
        "a filter; {predicate} is the condition from the plan",
        "Keeps only rows where {predicate}, leaving {rows}.",
        "Expects to keep about {rows} where {predicate}.",
        "Keeps only rows where {predicate}."
    ),
    (
        "filter",
        "a filter when the plan does not show the condition",
        "Filters its input down to {rows}.",
        "Expects to filter its input down to about {rows}.",
        "Filters its input."
    ),
    (
        "compute",
        "an operator that only evaluates expressions",
        "Computes expressions for {rows}.",
        "Expects to compute expressions for about {rows}.",
        "Computes expressions for each row."
    ),
    (
        "modify_table",
        "an INSERT, UPDATE, DELETE or MERGE operator; {operator} is the engine's operator name",
        "{operator} changes {rows} in {table}.",
        "{operator} expects to change about {rows} in {table}.",
        "{operator} changes rows in {table}."
    ),
    (
        "modify",
        "a data-modifying operator when the plan does not name the table; {operator} is the engine's operator name",
        "{operator} changes {rows}.",
        "{operator} expects to change about {rows}.",
        "{operator} changes rows."
    ),
    (
        "remote",
        "work done on another server",
        "Runs on a remote server and returns {rows}.",
        "Expects the remote server to return about {rows}.",
        "Runs on a remote server."
    ),
    (
        "set_op",
        "UNION, INTERSECT, EXCEPT or concatenation; {operator} is the engine's operator name",
        "{operator} combines its inputs into {rows}.",
        "{operator} expects to combine its inputs into about {rows}.",
        "{operator} combines its inputs."
    ),
    (
        "limit",
        "TOP or LIMIT",
        "Stops after {rows}.",
        "Expects to stop after about {rows}.",
        "Stops once it has enough rows."
    ),
    (
        "subquery",
        "a subquery, derived table or view run as its own unit",
        "Runs a subquery that returns {rows}.",
        "Expects the subquery to return about {rows}.",
        "Runs a subquery."
    ),
    (
        "function",
        "a table function or row generator; {operator} is the engine's operator name",
        "{operator} produces {rows}.",
        "Expects {operator} to produce about {rows}.",
        "{operator} produces rows."
    ),
    (
        "constant",
        "rows that come from constants, not a table",
        "Produces {rows} from constants.",
        "Expects to produce about {rows} from constants.",
        "Produces rows from constants."
    ),
    (
        "result",
        "the top of the plan, which hands rows to the client",
        "Returns {rows} to the client.",
        "Expects to return about {rows} to the client.",
        "Returns the result to the client."
    ),
    (
        "lock",
        "a row-locking operator",
        "Locks {rows}.",
        "Expects to lock about {rows}.",
        "Locks the rows it reads."
    ),
    (
        "other",
        "any operator without a more specific sentence; {operator} is the engine's operator name",
        "{operator} returns {rows}.",
        "Expects {operator} to return about {rows}.",
        "Runs {operator}."
    ),
];

pub(crate) static PHRASES: &[PlanPhrase] = &[
    // ---- Steps that also know their input size ---------------------------
    phrase!(
        "story.step_from.group_keys",
        "Groups {input} by {keys} into {rows}.",
        "Execution Plan → Story: what a GROUP BY step did on an executed plan. {input} and {rows} are row-count phrases such as '8,210 rows'; {keys} is the list of grouping columns."
    ),
    phrase!(
        "story.step_from.aggregate",
        "Aggregates {input} into {rows}.",
        "Execution Plan → Story: what an aggregate step without grouping columns did on an executed plan. {input} and {rows} are row-count phrases such as '8,210 rows'."
    ),
    phrase!(
        "story.step_from.filter_where",
        "Receives {input} and keeps {rows} where {predicate}.",
        "Execution Plan → Story: what a filter step did on an executed plan. {input} and {rows} are row-count phrases such as '8,210 rows'; {predicate} is the condition from the plan."
    ),
    phrase!(
        "story.step_from.filter",
        "Receives {input} and keeps {rows}.",
        "Execution Plan → Story: what a filter step did on an executed plan when the plan does not show the condition. {input} and {rows} are row-count phrases such as '8,210 rows'."
    ),
    phrase!(
        "story.never_ran",
        "This step never ran.",
        "Execution Plan → Story: sentence for an operator that an executed plan reports as never executed."
    ),
    // ---- Notes under a step ------------------------------------------------
    phrase!(
        "story.note.subplan_cte",
        "This is the top of the CTE {name}.",
        "Execution Plan → Story: note under the step that computes a common table expression; {name} is the CTE's name."
    ),
    phrase!(
        "story.note.subplan_init",
        "This is the top of {name}, which runs at most once; its result is reused.",
        "Execution Plan → Story: note under the first step of a PostgreSQL InitPlan; {name} is the plan's own label such as 'InitPlan 1 (returns $0)'."
    ),
    phrase!(
        "story.note.subplan",
        "This is the top of {name}.",
        "Execution Plan → Story: note under the first step of a subplan; {name} is the plan's own label such as 'SubPlan 2'."
    ),
    phrase!(
        "story.note.seek_index",
        "The index is searched for {predicate}.",
        "Execution Plan → Story: note under an index seek; {predicate} is the search condition from the plan."
    ),
    phrase!(
        "story.note.seek",
        "Rows are found by {predicate}.",
        "Execution Plan → Story: note under a keyed lookup when no index is named; {predicate} is the search condition from the plan."
    ),
    phrase!(
        "story.note.join_on",
        "Rows are matched on {predicate}.",
        "Execution Plan → Story: note under a join; {predicate} is the join condition from the plan."
    ),
    phrase!(
        "story.note.join_left",
        "It is a left outer join: rows from the left input are kept even when nothing matches.",
        "Execution Plan → Story: note under a join step whose type is LEFT OUTER."
    ),
    phrase!(
        "story.note.join_right",
        "It is a right outer join: rows from the right input are kept even when nothing matches.",
        "Execution Plan → Story: note under a join step whose type is RIGHT OUTER."
    ),
    phrase!(
        "story.note.join_full",
        "It is a full outer join: unmatched rows from both inputs are kept.",
        "Execution Plan → Story: note under a join step whose type is FULL OUTER."
    ),
    phrase!(
        "story.note.join_semi",
        "It is a semi join: a row is returned once if any match exists.",
        "Execution Plan → Story: note under a join step whose type is SEMI (EXISTS / IN)."
    ),
    phrase!(
        "story.note.join_anti",
        "It is an anti join: only rows with no match are returned.",
        "Execution Plan → Story: note under a join step whose type is ANTI (NOT EXISTS / NOT IN)."
    ),
    phrase!(
        "story.note.join_cross",
        "It is a cross join: every row of one input is paired with every row of the other.",
        "Execution Plan → Story: note under a join step whose type is CROSS."
    ),
    phrase!(
        "story.note.filter_removed",
        "Keeps only rows where {predicate}, which discards {removed} ({share} of the rows it read).",
        "Execution Plan → Story: note under a step that filtered rows; {predicate} is the condition, {removed} a row-count phrase such as '8,210 rows', {share} a percentage such as 97%."
    ),
    phrase!(
        "story.note.bitmap_filter_removed",
        "Drops rows whose {columns} cannot match in a join higher in the plan (the join built a bitmap filter and pushed it down to this read), which discards {removed} ({share} of the rows it read).",
        "Execution Plan → Story: note under a SQL Server read that applies a bitmap filter (PROBE) and reports the rows it removed; {columns} is a list of column names, {removed} a row-count phrase such as '8,210 rows', {share} a percentage such as 97%."
    ),
    phrase!(
        "story.note.bitmap_filter",
        "Drops rows whose {columns} cannot match in a join higher in the plan (the join built a bitmap filter and pushed it down to this read).",
        "Execution Plan → Story: note under a SQL Server read that applies a bitmap filter (PROBE); {columns} is a list of column names."
    ),
    phrase!(
        "story.note.filter",
        "Keeps only rows where {predicate}.",
        "Execution Plan → Story: note under a step that filters rows; {predicate} is the condition from the plan."
    ),
    phrase!(
        "story.note.removed",
        "Of the rows it read, discards {removed} ({share}).",
        "Execution Plan → Story: note under a step that read rows and threw some away; {removed} is a row-count phrase such as '8,210 rows', {share} a percentage."
    ),
    phrase!(
        "story.note.loops",
        "Ran {loops} times, returning about {rows} each time.",
        "Execution Plan → Story: note under a step that executed more than once (inner side of a nested loop); {loops} is a number, {rows} a row-count phrase such as '3 rows'."
    ),
    phrase!(
        "story.note.processes",
        "Ran in {loops} parallel processes, each returning about {rows}.",
        "Execution Plan → Story: note under a step of a PostgreSQL parallel plan that ran once in the leader and once in each worker; {loops} is a number, {rows} a row-count phrase such as '200K rows'."
    ),
    phrase!(
        "story.note.rows",
        "It returned {rows}.",
        "Execution Plan → Story: note under a step whose sentence is the operator's glossary description, on an executed plan; {rows} is a row-count phrase such as '8,210 rows'."
    ),
    phrase!(
        "story.note.rows_expected",
        "The optimizer expects about {rows} from it.",
        "Execution Plan → Story: note under a step whose sentence is the operator's glossary description, on an estimated plan; {rows} is a row-count phrase such as '8,210 rows'."
    ),
    phrase!(
        "story.note.partitions",
        "Read {scanned} of {total} partitions.",
        "Execution Plan → Story: note under a scan of a partitioned table; {scanned} and {total} are numbers."
    ),
    phrase!(
        "story.note.workers",
        "Started {launched} parallel workers.",
        "Execution Plan → Story: note under the step that collects rows from parallel workers; {launched} is a number."
    ),
    phrase!(
        "story.note.workers_short",
        "Only {launched} of the {planned} planned parallel workers started.",
        "Execution Plan → Story: note under the step that collects rows from parallel workers, when fewer workers started than planned; both are numbers."
    ),
    phrase!(
        "story.note.workers_planned",
        "Plans to use {planned} parallel workers.",
        "Execution Plan → Story: note under the step that collects rows from parallel workers on an estimated plan; {planned} is a number."
    ),
    phrase!(
        "story.note.estimate_under",
        "The optimizer expected {estimated} here and got {actual}, {factor} times more.",
        "Execution Plan → Story: note under a step whose row estimate was too low; {estimated} and {actual} are row-count phrases such as '8,210 rows', {factor} a number."
    ),
    phrase!(
        "story.note.estimate_over",
        "The optimizer expected {estimated} here and got {actual}, {factor} times fewer.",
        "Execution Plan → Story: note under a step whose row estimate was too high; {estimated} and {actual} are row-count phrases such as '8,210 rows', {factor} a number."
    ),
    phrase!(
        "story.note.estimate_under_loop",
        "The optimizer expected {estimated} per run and got {actual}, {factor} times more.",
        "Execution Plan → Story: note under a step that ran several times and whose per-run row estimate was too low; {estimated} and {actual} are row-count phrases, {factor} a number."
    ),
    phrase!(
        "story.note.estimate_over_loop",
        "The optimizer expected {estimated} per run and got {actual}, {factor} times fewer.",
        "Execution Plan → Story: note under a step that ran several times and whose per-run row estimate was too high; {estimated} and {actual} are row-count phrases, {factor} a number."
    ),
    phrase!(
        "story.note.estimate_under_process",
        "The optimizer expected {estimated} per process and got {actual}, {factor} times more.",
        "Execution Plan → Story: note under a step of a parallel plan whose per-process row estimate was too low; {estimated} and {actual} are row-count phrases, {factor} a number."
    ),
    phrase!(
        "story.note.estimate_over_process",
        "The optimizer expected {estimated} per process and got {actual}, {factor} times fewer.",
        "Execution Plan → Story: note under a step of a parallel plan whose per-process row estimate was too high; {estimated} and {actual} are row-count phrases, {factor} a number."
    ),
    phrase!(
        "story.note.spill_size",
        "It did not fit in memory and wrote {size} to disk.",
        "Execution Plan → Story: note under a sort or hash step that spilled; {size} is a size such as 9.2 MB."
    ),
    phrase!(
        "story.note.spill_batches",
        "The hash table did not fit in memory and was split into {batches} batches on disk.",
        "Execution Plan → Story: note under a hash step that spilled; {batches} is a number."
    ),
    phrase!(
        "story.note.spill",
        "It did not fit in memory and spilled to disk.",
        "Execution Plan → Story: note under a sort or hash step that spilled when the plan does not say how much."
    ),
    phrase!(
        "story.note.memory",
        "Used {size} of memory.",
        "Execution Plan → Story: note under a step that reports its peak memory; {size} is a size such as 9.2 MB."
    ),
    phrase!(
        "story.note.io",
        "Read {pages} pages itself, {share} of the plan's page reads.",
        "Execution Plan → Story: note under a step with a large share of the page reads; {pages} is a formatted number, {share} a percentage."
    ),
    phrase!(
        "story.note.time",
        "Took {time} of its own, {share} of the total.",
        "Execution Plan → Story: note under a step with a notable share of the time; {time} is a duration such as 84 ms (the step alone, without its inputs), {share} a percentage."
    ),
    phrase!(
        "story.note.cost",
        "Accounts for {share} of the estimated cost.",
        "Execution Plan → Story: note under a step with a notable share of the optimizer's estimated cost; {share} is a percentage."
    ),
    // ---- Bottleneck ----------------------------------------------------------
    phrase!(
        "story.bottleneck.time",
        "Step {step} ({title}) is the bottleneck: {time} of its own work, {share} of the total.",
        "Execution Plan → Story: the call-out naming the slowest step; {step} is a step number, {title} the operator title such as 'Seq Scan on orders', {time} a duration, {share} a percentage."
    ),
    phrase!(
        "story.bottleneck.time_spread",
        "No single step dominates. The slowest is step {step} ({title}) with {time}, {share} of the total.",
        "Execution Plan → Story: the call-out when time is spread across steps; {step} is a step number, {title} the operator title, {time} a duration, {share} a percentage."
    ),
    phrase!(
        "story.bottleneck.time_spread_two",
        "No single step dominates. The slowest are step {step} ({title}) with {time}, {share} of the total, and step {other_step} ({other_title}) with {other_share}.",
        "Execution Plan → Story: the call-out when two steps take a similar share of the time; {step} and {other_step} are step numbers, {title} and {other_title} operator titles, {time} a duration, {share} and {other_share} percentages."
    ),
    phrase!(
        "story.bottleneck.cost_spread_two",
        "The estimated cost is spread out. The largest shares are step {step} ({title}) with {share} and step {other_step} ({other_title}) with {other_share}.",
        "Execution Plan → Story: the call-out when two steps carry a similar share of the estimated cost; {step} and {other_step} are step numbers, {title} and {other_title} operator titles, {share} and {other_share} percentages."
    ),
    phrase!(
        "story.bottleneck.cost",
        "Step {step} ({title}) carries {share} of the estimated cost, more than any other step.",
        "Execution Plan → Story: the call-out naming the most expensive step of an estimated plan; {step} is a step number, {title} the operator title, {share} a percentage."
    ),
    phrase!(
        "story.bottleneck.cost_spread",
        "The estimated cost is spread out. The largest share is step {step} ({title}) with {share}.",
        "Execution Plan → Story: the call-out when estimated cost is spread across steps; {step} is a step number, {title} the operator title, {share} a percentage."
    ),
    phrase!(
        "story.bottleneck.read_vs_returned",
        "It read {read} to return {rows}.",
        "Execution Plan → Story: second sentence of the bottleneck call-out when the step discarded most of what it read; {read} and {rows} are row-count phrases such as '8,210 rows'."
    ),
    phrase!(
        "story.bottleneck.spilled",
        "It ran out of memory and spilled to disk.",
        "Execution Plan → Story: second sentence of the bottleneck call-out when the step spilled to disk."
    ),
    phrase!(
        "story.bottleneck.loops",
        "It ran {loops} times.",
        "Execution Plan → Story: second sentence of the bottleneck call-out when the step executed many times; {loops} is a formatted number."
    ),
    // ---- Headline --------------------------------------------------------------
    phrase!(
        "story.headline.actual_tables_rows_time",
        "This query read from {tables} and returned {rows} in {time}.",
        "Execution Plan → Story: headline for an executed plan; {tables} is a list of table names, {rows} a row-count phrase such as '12 rows', {time} a duration such as 512 ms."
    ),
    phrase!(
        "story.headline.actual_rows_time",
        "This query returned {rows} in {time}.",
        "Execution Plan → Story: headline for an executed plan that names no tables; {rows} is a row-count phrase such as '12 rows', {time} a duration."
    ),
    phrase!(
        "story.headline.actual_tables_rows",
        "This query read from {tables} and returned {rows}.",
        "Execution Plan → Story: headline for an executed plan without timings; {tables} is a list of table names, {rows} a row-count phrase such as '12 rows'."
    ),
    phrase!(
        "story.headline.actual_rows",
        "This query returned {rows}.",
        "Execution Plan → Story: headline for an executed plan without timings or table names; {rows} is a row-count phrase such as '12 rows'."
    ),
    phrase!(
        "story.headline.actual_tables_time",
        "This query read from {tables} and took {time}.",
        "Execution Plan → Story: headline for an executed plan without a result row count; {tables} is a list of table names, {time} a duration."
    ),
    phrase!(
        "story.headline.actual_time",
        "This query took {time}.",
        "Execution Plan → Story: headline for an executed plan with only a duration; {time} is a duration such as 512 ms."
    ),
    phrase!(
        "story.headline.modify_time",
        "This statement changed {table} in {time}.",
        "Execution Plan → Story: headline for an executed INSERT, UPDATE, DELETE or MERGE; {table} is the table name, {time} a duration."
    ),
    phrase!(
        "story.headline.modify",
        "This statement changed {table}.",
        "Execution Plan → Story: headline for an executed INSERT, UPDATE, DELETE or MERGE without timings; {table} is the table name."
    ),
    phrase!(
        "story.headline.expected_tables_rows_cost",
        "The optimizer expects this query to read from {tables} and return about {rows}, at an estimated cost of {cost}.",
        "Execution Plan → Story: headline for an estimated plan; {tables} is a list of table names, {rows} a row-count phrase such as '12 rows', {cost} the optimizer's cost number."
    ),
    phrase!(
        "story.headline.expected_rows_cost",
        "The optimizer expects this query to return about {rows}, at an estimated cost of {cost}.",
        "Execution Plan → Story: headline for an estimated plan that names no tables; {rows} is a row-count phrase, {cost} the optimizer's cost number."
    ),
    phrase!(
        "story.headline.expected_tables_rows",
        "The optimizer expects this query to read from {tables} and return about {rows}.",
        "Execution Plan → Story: headline for an estimated plan from an engine without cost numbers; {tables} is a list of table names, {rows} a row-count phrase."
    ),
    phrase!(
        "story.headline.expected_rows",
        "The optimizer expects this query to return about {rows}.",
        "Execution Plan → Story: headline for an estimated plan with only a row estimate; {rows} is a row-count phrase such as '12 rows'."
    ),
    phrase!(
        "story.headline.expected_tables_cost",
        "The optimizer expects this query to read from {tables}, at an estimated cost of {cost}.",
        "Execution Plan → Story: headline for an estimated plan without a row estimate; {tables} is a list of table names, {cost} the optimizer's cost number."
    ),
    phrase!(
        "story.headline.expected_cost",
        "The optimizer puts this query at an estimated cost of {cost}.",
        "Execution Plan → Story: headline for an estimated plan with only a cost; {cost} is the optimizer's cost number."
    ),
    phrase!(
        "story.headline.expected_modify_cost",
        "The optimizer expects this statement to change {table}, at an estimated cost of {cost}.",
        "Execution Plan → Story: headline for an estimated INSERT, UPDATE, DELETE or MERGE; {table} is the table name, {cost} the optimizer's cost number."
    ),
    phrase!(
        "story.headline.expected_modify",
        "The optimizer expects this statement to change {table}.",
        "Execution Plan → Story: headline for an estimated INSERT, UPDATE, DELETE or MERGE without a cost; {table} is the table name."
    ),
    phrase!(
        "story.headline.plain_tables",
        "This query reads from {tables}.",
        "Execution Plan → Story: headline for a plan with no numbers at all (for example SQLite EXPLAIN QUERY PLAN); {tables} is a list of table names."
    ),
    phrase!(
        "story.headline.plain",
        "This plan has {steps} steps.",
        "Execution Plan → Story: headline for a plan with no numbers and no table names; {steps} is a number."
    ),
    // ---- Lists -----------------------------------------------------------------
    phrase!(
        "story.list.two",
        "{first} and {second}",
        "Execution Plan → Story: a list of exactly two table names placed inside the headline sentence."
    ),
    phrase!(
        "story.list.many",
        "{head} and {last}",
        "Execution Plan → Story: a list of three or more table names placed inside the headline; {head} is the comma-separated start of the list, {last} the final name."
    ),
    phrase!(
        "story.list.more",
        "{head} and {count} more",
        "Execution Plan → Story: a shortened list of table names placed inside the headline; {head} is the comma-separated start of the list, {count} how many names were left out."
    ),
    // ---- Summary ---------------------------------------------------------------
    phrase!(
        "story.summary.estimated_only",
        "This is an estimated plan: the query was not run, so every number is the optimizer's estimate.",
        "Execution Plan → Story: summary line shown for plans captured without executing the query."
    ),
    phrase!(
        "story.summary.no_numbers",
        "This plan format carries no row counts, costs or timings, so the steps only say what happens.",
        "Execution Plan → Story: summary line shown for plan formats without any numbers (for example SQLite EXPLAIN QUERY PLAN)."
    ),
    phrase!(
        "story.summary.no_timing",
        "The plan has actual row counts but no per-step timings, so shares are of the estimated cost.",
        "Execution Plan → Story: summary line shown for executed plans that lack per-operator timings."
    ),
    phrase!(
        "story.summary.coarse_timing",
        "The plan prints step times to the nearest {resolution}, which is too coarse to say which step took the time in a statement this short.",
        "Execution Plan → Story: summary line for executed plans whose format rounds operator times too heavily to rank the steps (DuckDB text profiles, Oracle DBMS_XPLAN A-Time); {resolution} is a duration such as 10 ms."
    ),
    phrase!(
        "story.summary.coarse_timing_cost",
        "The plan prints step times to the nearest {resolution}, which is too coarse to say which step took the time in a statement this short, so shares are of the estimated cost.",
        "Execution Plan → Story: summary line for executed plans whose format rounds operator times too heavily to rank the steps, when the plan has optimizer costs to rank by instead; {resolution} is a duration such as 10 ms."
    ),
    phrase!(
        "story.summary.planning",
        "Planning took {planning} and execution {execution}.",
        "Execution Plan → Story: summary line; {planning} and {execution} are durations such as 1.2 ms."
    ),
    phrase!(
        "story.summary.thread_time",
        "Operator times add up to {total}, more than the {elapsed} the query took, because operators ran on several threads at once. Shares are of the operator total.",
        "Execution Plan → Story: summary line for plans whose per-operator times sum to more than the elapsed time (DuckDB, parallel plans); {total} and {elapsed} are durations such as 5.7 ms."
    ),
    phrase!(
        "story.summary.worst_estimate_under",
        "The largest estimate miss is at step {step} ({title}): the optimizer expected {estimated} and got {actual}, {factor} times more.",
        "Execution Plan → Story: summary line about the worst row estimate when the step returned more rows than the optimizer expected (an underestimate); {step} is a step number, {title} the operator title, {estimated} and {actual} row-count phrases, {factor} a number."
    ),
    phrase!(
        "story.summary.worst_estimate_over",
        "The largest estimate miss is at step {step} ({title}): the optimizer expected {estimated} and got {actual}, {factor} times fewer.",
        "Execution Plan → Story: summary line about the worst row estimate when the step returned fewer rows than the optimizer expected (an overestimate); {step} is a step number, {title} the operator title, {estimated} and {actual} row-count phrases, {factor} a number."
    ),
    phrase!(
        "story.summary.spill_one",
        "Step {step} spilled to disk.",
        "Execution Plan → Story: summary line when one sort or hash step spilled to disk; {step} is a step number."
    ),
    phrase!(
        "story.summary.spill_many",
        "Steps {steps} spilled to disk.",
        "Execution Plan → Story: summary line when several steps spilled to disk; {steps} is a comma-separated list of step numbers."
    ),
    phrase!(
        "story.summary.parallel_dop",
        "The query ran with a degree of parallelism of {dop}.",
        "Execution Plan → Story: summary line for SQL Server parallel plans; {dop} is a number."
    ),
    phrase!(
        "story.summary.parallel",
        "Part of the plan runs on parallel workers.",
        "Execution Plan → Story: summary line for plans with a parallel section."
    ),
    phrase!(
        "story.summary.memory_grant",
        "The query was granted {granted} of memory and used {used}.",
        "Execution Plan → Story: summary line for SQL Server memory grants; {granted} and {used} are sizes such as 66 MB."
    ),
    phrase!(
        "story.summary.never_executed",
        "{count} steps never ran.",
        "Execution Plan → Story: summary line counting operators the executed plan reports as never executed; {count} is a number."
    ),
    // ---- Batches ---------------------------------------------------------------
    phrase!(
        "story.batch.heaviest_time",
        "This batch has {count} statements. Statement {statement} took {share} of the time.",
        "Execution Plan → Story: line above a multi-statement batch of executed plans; {count} and {statement} are numbers, {share} a percentage."
    ),
    phrase!(
        "story.batch.heaviest_cost",
        "This batch has {count} statements. Statement {statement} has {share} of the estimated cost.",
        "Execution Plan → Story: line above a multi-statement batch without timings; {count} and {statement} are numbers, {share} a percentage."
    ),
    phrase!(
        "story.batch.plain",
        "This batch has {count} statements.",
        "Execution Plan → Story: line above a multi-statement batch with no costs or timings; {count} is a number."
    ),
];

#[cfg(test)]
mod tests;
