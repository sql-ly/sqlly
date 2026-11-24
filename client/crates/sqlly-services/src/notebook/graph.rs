//! The dependency DAG. Nodes are cells and parameters (`param:<name>`);
//! edges come from `{{…}}` references, `:name` / `@name` parameter uses in
//! SQL, `spec.input` / `spec.inputs`, a parameter cell's `spec.parameter`,
//! and query-filled parameter options. Edges are stored by id, so renames
//! never break them.
//!
//! Ordering is deterministic: topological, ties broken by declaration order
//! (parameters first, then cells in document order). Cycles are reported
//! with their full path and a suggested reference to remove.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use sqlly_lang::DisplayMessage;

use sqlly_lang::sqllex::StrictLexer;

use super::document::{Cell, CellKind, NotebookDoc, SourceLanguage};
use super::params::{AllowedValues, find_param_uses};
use super::references::{Dialects, Resolved, cell_references, resolve};

/// A graph node.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Node {
    Param(String),
    Cell(String),
}

impl Node {
    pub fn cell(id: impl Into<String>) -> Node {
        Node::Cell(id.into())
    }

    pub fn param(name: impl Into<String>) -> Node {
        Node::Param(name.into())
    }

    pub fn as_cell(&self) -> Option<&str> {
        match self {
            Node::Cell(id) => Some(id),
            Node::Param(_) => None,
        }
    }

    /// Parse the text form (`param:<name>` or a cell id).
    pub fn parse(text: &str) -> Node {
        match text.strip_prefix("param:") {
            Some(name) => Node::Param(name.to_string()),
            None => Node::Cell(text.to_string()),
        }
    }
}

impl fmt::Display for Node {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Node::Param(name) => write!(f, "param:{name}"),
            Node::Cell(id) => f.write_str(id),
        }
    }
}

/// Why one node depends on another.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum EdgeKind {
    /// `{{x}}` in SQL: may be inlined as a CTE.
    Sql,
    /// `{{x}}` in Markdown / AI text: shows the upstream output.
    Text,
    /// `spec.input` / `spec.inputs`: reads the upstream output.
    Input,
    /// A parameter used by a cell.
    Param,
    /// A parameter whose options come from a cell.
    Options,
}

impl EdgeKind {
    /// Whether the dependent needs the upstream's materialized output (not
    /// just its definition).
    pub fn needs_output(self) -> bool {
        matches!(self, EdgeKind::Text | EdgeKind::Input | EdgeKind::Options)
    }
}

/// A reference that names nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Unresolved {
    Cell { cell: String, token: String },
    Param { cell: String, name: String },
    Input { cell: String, input: String },
    Options { param: String, cell: String },
}

impl Unresolved {
    /// The node whose definition holds the broken reference.
    pub fn owner(&self) -> Node {
        match self {
            Unresolved::Cell { cell, .. }
            | Unresolved::Param { cell, .. }
            | Unresolved::Input { cell, .. } => Node::Cell(cell.clone()),
            Unresolved::Options { param, .. } => Node::Param(param.clone()),
        }
    }

    pub fn message(&self) -> DisplayMessage {
        match self {
            Unresolved::Cell { token, .. } => DisplayMessage::new(
                "notebook.core.graph.unknown_cell",
                format!("No cell is named {token}"),
            )
            .with_arg("token", token.clone()),
            Unresolved::Param { name, .. } => DisplayMessage::new(
                "notebook.core.graph.unknown_parameter",
                format!("No parameter is named {name}"),
            )
            .with_arg("name", name.clone()),
            Unresolved::Input { input, .. } => DisplayMessage::new(
                "notebook.core.graph.unknown_input",
                format!("The input cell {input} does not exist"),
            )
            .with_arg("input", input.clone()),
            Unresolved::Options { cell, .. } => DisplayMessage::new(
                "notebook.core.graph.unknown_options_cell",
                format!("The cell {cell} that fills this parameter's options does not exist"),
            )
            .with_arg("cell", cell.clone()),
        }
    }
}

/// A dependency cycle. `path` follows "depends on" edges and is closed
/// (`[a, b, a]`: a depends on b, which depends on a).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CycleError {
    pub path: Vec<Node>,
    /// Display labels for `path` (cell names, else ids; `param:<name>`).
    pub labels: Vec<String>,
    /// The suggested fix: drop `dependent`'s reference to `upstream`.
    pub dependent: Node,
    pub upstream: Node,
}

impl CycleError {
    pub fn message(&self) -> DisplayMessage {
        let path = self.labels.join(" → ");
        DisplayMessage::new(
            "notebook.core.cycle.loop",
            format!("These cells depend on each other in a loop: {path}"),
        )
        .with_arg("path", path)
    }

    pub fn suggestion(&self) -> DisplayMessage {
        let label = |node: &Node| {
            self.path
                .iter()
                .position(|n| n == node)
                .map(|i| self.labels[i].clone())
                .unwrap_or_else(|| node.to_string())
        };
        let (dependent, upstream) = (label(&self.dependent), label(&self.upstream));
        DisplayMessage::new(
            "notebook.core.cycle.remove_reference",
            format!("Remove the reference to {upstream} from {dependent}"),
        )
        .with_arg("dependent", dependent)
        .with_arg("upstream", upstream)
    }
}

type Adjacency = BTreeMap<Node, Vec<(Node, EdgeKind)>>;

/// The dependency graph of one document revision.
#[derive(Debug, Clone)]
pub struct Graph {
    nodes: Vec<Node>,
    rank: BTreeMap<Node, usize>,
    labels: BTreeMap<Node, String>,
    upstream: Adjacency,
    downstream: Adjacency,
    unresolved: Vec<Unresolved>,
    order: Vec<Node>,
    cyclic: BTreeSet<Node>,
    cycles: Vec<CycleError>,
}

fn add_edge(graph: &mut Graph, from: Node, to: Node, kind: EdgeKind) {
    let ups = graph.upstream.entry(to.clone()).or_default();
    if !ups.contains(&(from.clone(), kind)) {
        ups.push((from.clone(), kind));
        graph.downstream.entry(from).or_default().push((to, kind));
    }
}

/// Attachment cells `cell` reads without a `{{…}}` reference: a local SQL
/// cell naming an attachment's table outright, or a Markdown picture
/// (`attachment:<name>`). Each must have run (imported its file) first.
fn attachment_reads(doc: &NotebookDoc, cell: &Cell, dialects: &Dialects) -> Vec<String> {
    use super::cells::attachment::{AttachmentSpec, markdown_attachment_refs};
    let attachments = || {
        doc.cells
            .iter()
            .filter(|c| c.kind == CellKind::Attachment && c.id != cell.id)
    };
    if cell.kind == CellKind::Markdown {
        return markdown_attachment_refs(&cell.source)
            .iter()
            .filter_map(|name| doc.resolve(name).or_else(|| doc.cell(name)))
            .filter(|c| c.kind == CellKind::Attachment)
            .map(|c| c.id.clone())
            .collect();
    }
    if cell.kind != CellKind::Sql || !super::cells::local::targets_local(cell) {
        return Vec::new();
    }
    let db = dialects.for_cell(doc, cell);
    let names: BTreeSet<String> = StrictLexer::tokenize(&cell.source, db.family())
        .iter()
        .filter(|t| t.is_word())
        .map(|t| t.ident_text().to_lowercase())
        .collect();
    attachments()
        .filter(|c| {
            AttachmentSpec::of(c)
                .relation
                .is_some_and(|r| names.contains(&r.to_lowercase()))
        })
        .map(|c| c.id.clone())
        .collect()
}

impl Graph {
    pub fn build(doc: &NotebookDoc, dialects: &Dialects) -> Graph {
        let mut nodes: Vec<Node> = doc
            .parameters
            .iter()
            .map(|p| Node::Param(p.name.clone()))
            .collect();
        nodes.extend(doc.cells.iter().map(|c| Node::Cell(c.id.clone())));
        let mut graph = Graph {
            rank: nodes
                .iter()
                .cloned()
                .enumerate()
                .map(|(i, n)| (n, i))
                .collect(),
            labels: BTreeMap::new(),
            nodes,
            upstream: BTreeMap::new(),
            downstream: BTreeMap::new(),
            unresolved: Vec::new(),
            order: Vec::new(),
            cyclic: BTreeSet::new(),
            cycles: Vec::new(),
        };
        for def in &doc.parameters {
            graph
                .labels
                .insert(Node::Param(def.name.clone()), format!("param:{}", def.name));
            if let Some(AllowedValues::FromCell { from_cell, .. }) = &def.allowed_values {
                match doc.cell(from_cell).or_else(|| doc.resolve(from_cell)) {
                    Some(cell) => add_edge(
                        &mut graph,
                        Node::Cell(cell.id.clone()),
                        Node::Param(def.name.clone()),
                        EdgeKind::Options,
                    ),
                    None => graph.unresolved.push(Unresolved::Options {
                        param: def.name.clone(),
                        cell: from_cell.clone(),
                    }),
                }
            }
        }
        for cell in &doc.cells {
            let this = Node::Cell(cell.id.clone());
            graph
                .labels
                .insert(this.clone(), cell.relation_name().to_string());
            let text_kind = match cell.kind.source_language() {
                SourceLanguage::Sql => EdgeKind::Sql,
                _ => EdgeKind::Text,
            };
            for reference in cell_references(doc, cell, dialects) {
                match resolve(doc, &reference) {
                    Resolved::Cell { id } => {
                        add_edge(&mut graph, Node::Cell(id), this.clone(), text_kind)
                    }
                    Resolved::Param { name } => {
                        add_edge(&mut graph, Node::Param(name), this.clone(), EdgeKind::Param)
                    }
                    Resolved::UnknownCell { token } => graph.unresolved.push(Unresolved::Cell {
                        cell: cell.id.clone(),
                        token,
                    }),
                    Resolved::UnknownParam { name } => graph.unresolved.push(Unresolved::Param {
                        cell: cell.id.clone(),
                        name,
                    }),
                }
            }
            if cell.kind.source_language() == SourceLanguage::Sql {
                let db = dialects.for_cell(doc, cell);
                for u in find_param_uses(db, &cell.source, &doc.parameters) {
                    add_edge(
                        &mut graph,
                        Node::Param(u.name),
                        this.clone(),
                        EdgeKind::Param,
                    );
                }
            }
            for input in cell.spec_inputs() {
                match doc.cell(&input).or_else(|| doc.resolve(&input)) {
                    Some(up) => add_edge(
                        &mut graph,
                        Node::Cell(up.id.clone()),
                        this.clone(),
                        EdgeKind::Input,
                    ),
                    None => graph.unresolved.push(Unresolved::Input {
                        cell: cell.id.clone(),
                        input,
                    }),
                }
            }
            for up in attachment_reads(doc, cell, dialects) {
                let up = Node::Cell(up);
                let linked = graph
                    .upstream
                    .get(&this)
                    .is_some_and(|ups| ups.iter().any(|(n, _)| *n == up));
                if !linked {
                    add_edge(&mut graph, up, this.clone(), EdgeKind::Input);
                }
            }
            if cell.kind == CellKind::Parameter
                && let Some(name) = cell.spec.get("parameter").and_then(|v| v.as_str())
            {
                match doc.parameter(name) {
                    Some(def) => add_edge(
                        &mut graph,
                        Node::Param(def.name.clone()),
                        this.clone(),
                        EdgeKind::Param,
                    ),
                    None => graph.unresolved.push(Unresolved::Param {
                        cell: cell.id.clone(),
                        name: name.to_string(),
                    }),
                }
            }
        }
        for list in graph
            .upstream
            .values_mut()
            .chain(graph.downstream.values_mut())
        {
            let rank = &graph.rank;
            list.sort_by_key(|(n, k)| (rank.get(n).copied().unwrap_or(usize::MAX), *k));
        }
        graph.sort();
        graph
    }

    fn rank_of(&self, node: &Node) -> usize {
        self.rank.get(node).copied().unwrap_or(usize::MAX)
    }

    fn sort(&mut self) {
        let mut pending: BTreeMap<&Node, usize> = self
            .nodes
            .iter()
            .map(|n| {
                let ups = self.upstream.get(n).map_or(0, |u| {
                    u.iter().map(|(n, _)| n).collect::<BTreeSet<_>>().len()
                });
                (n, ups)
            })
            .collect();
        let mut ready: BTreeSet<(usize, &Node)> = pending
            .iter()
            .filter(|(_, count)| **count == 0)
            .map(|(n, _)| (self.rank_of(n), *n))
            .collect();
        let mut order = Vec::new();
        while let Some(first) = ready.pop_first() {
            let node = first.1;
            order.push(node.clone());
            let downs: BTreeSet<&Node> = self
                .downstream
                .get(node)
                .map(|d| d.iter().map(|(n, _)| n).collect())
                .unwrap_or_default();
            for down in downs {
                if let Some(count) = pending.get_mut(down) {
                    *count -= 1;
                    if *count == 0 {
                        ready.insert((self.rank_of(down), down));
                    }
                }
            }
        }
        let placed: BTreeSet<&Node> = order.iter().collect();
        let cyclic: BTreeSet<Node> = self
            .nodes
            .iter()
            .filter(|n| !placed.contains(n))
            .cloned()
            .collect();
        self.order = order;
        self.cyclic = cyclic;
        self.cycles = self.find_cycles();
    }

    fn find_cycles(&self) -> Vec<CycleError> {
        let mut out: Vec<CycleError> = Vec::new();
        let mut covered: BTreeSet<Node> = BTreeSet::new();
        let mut starts: Vec<&Node> = self.cyclic.iter().collect();
        starts.sort_by_key(|n| self.rank_of(n));
        for start in starts {
            if covered.contains(start) {
                continue;
            }
            let Some(path) = self.cycle_from(start) else {
                continue;
            };
            let members: BTreeSet<Node> = path.iter().cloned().collect();
            if out
                .iter()
                .any(|c| c.path.iter().cloned().collect::<BTreeSet<_>>() == members)
            {
                continue;
            }
            covered.extend(members);
            out.push(self.cycle_error(path));
        }
        out
    }

    /// Follow "depends on" edges inside the cyclic set from `start` until a
    /// node repeats; returns the closed loop.
    fn cycle_from(&self, start: &Node) -> Option<Vec<Node>> {
        let mut path: Vec<Node> = vec![start.clone()];
        let mut seen: BTreeMap<Node, usize> = BTreeMap::from([(start.clone(), 0)]);
        let mut current = start.clone();
        loop {
            let next = self
                .upstream
                .get(&current)?
                .iter()
                .map(|(n, _)| n)
                .find(|n| self.cyclic.contains(*n))?
                .clone();
            if let Some(&at) = seen.get(&next) {
                let mut cycle = path[at..].to_vec();
                cycle.push(next);
                return Some(cycle);
            }
            seen.insert(next.clone(), path.len());
            path.push(next.clone());
            current = next;
        }
    }

    fn cycle_error(&self, mut path: Vec<Node>) -> CycleError {
        // Rotate so the loop starts at its earliest node.
        path.pop();
        let first = (0..path.len())
            .min_by_key(|&i| self.rank_of(&path[i]))
            .unwrap_or(0);
        path.rotate_left(first);
        path.push(path[0].clone());
        // Prefer removing a reference that reads from a cell further down
        // the document; else the edge that closes the loop.
        let pairs: Vec<(&Node, &Node)> = path.windows(2).map(|w| (&w[0], &w[1])).collect();
        let (dependent, upstream) = pairs
            .iter()
            .find(|(d, u)| self.rank_of(u) > self.rank_of(d))
            .or(pairs.last())
            .map(|(d, u)| ((*d).clone(), (*u).clone()))
            .unwrap_or_else(|| (path[0].clone(), path[0].clone()));
        CycleError {
            labels: path.iter().map(|n| self.label(n)).collect(),
            path,
            dependent,
            upstream,
        }
    }

    /// Every node: parameters in declaration order, then cells in
    /// document order.
    pub fn nodes(&self) -> &[Node] {
        &self.nodes
    }

    pub fn label(&self, node: &Node) -> String {
        self.labels
            .get(node)
            .cloned()
            .unwrap_or_else(|| node.to_string())
    }

    /// Topological order of every node not in or behind a cycle.
    pub fn order(&self) -> &[Node] {
        &self.order
    }

    /// The full order, or the first cycle.
    pub fn topological_order(&self) -> Result<&[Node], &CycleError> {
        match self.cycles.first() {
            Some(cycle) => Err(cycle),
            None => Ok(&self.order),
        }
    }

    pub fn cycles(&self) -> &[CycleError] {
        &self.cycles
    }

    /// Nodes on a cycle or downstream of one; they cannot be ordered.
    pub fn cyclic(&self) -> &BTreeSet<Node> {
        &self.cyclic
    }

    pub fn unresolved(&self) -> &[Unresolved] {
        &self.unresolved
    }

    /// Direct dependencies of `node`, in declaration order.
    pub fn upstream(&self, node: &Node) -> &[(Node, EdgeKind)] {
        self.upstream.get(node).map_or(&[], Vec::as_slice)
    }

    /// Direct dependents of `node`, in declaration order.
    pub fn downstream(&self, node: &Node) -> &[(Node, EdgeKind)] {
        self.downstream.get(node).map_or(&[], Vec::as_slice)
    }

    fn closure(&self, node: &Node, adjacency: &Adjacency) -> Vec<Node> {
        let mut seen: BTreeSet<Node> = BTreeSet::new();
        let mut stack = vec![node.clone()];
        while let Some(n) = stack.pop() {
            for (next, _) in adjacency.get(&n).map_or(&[][..], Vec::as_slice) {
                if next != node && seen.insert(next.clone()) {
                    stack.push(next.clone());
                }
            }
        }
        self.in_order(seen)
    }

    /// `nodes` in topological order (cyclic leftovers last, by rank).
    pub fn in_order(&self, nodes: impl IntoIterator<Item = Node>) -> Vec<Node> {
        let set: BTreeSet<Node> = nodes.into_iter().collect();
        let mut out: Vec<Node> = self
            .order
            .iter()
            .filter(|n| set.contains(*n))
            .cloned()
            .collect();
        let mut rest: Vec<Node> = set
            .into_iter()
            .filter(|n| self.cyclic.contains(n))
            .collect();
        rest.sort_by_key(|n| self.rank_of(n));
        out.extend(rest);
        out
    }

    /// Every transitive dependency of `node`, in run order.
    pub fn upstream_closure(&self, node: &Node) -> Vec<Node> {
        self.closure(node, &self.upstream)
    }

    /// Every transitive dependent of `node`, in run order.
    pub fn downstream_closure(&self, node: &Node) -> Vec<Node> {
        self.closure(node, &self.downstream)
    }

    /// Parameters `cell` uses directly.
    pub fn params_of(&self, cell: &str) -> Vec<&str> {
        self.upstream(&Node::cell(cell))
            .iter()
            .filter_map(|(n, _)| match n {
                Node::Param(name) => Some(name.as_str()),
                Node::Cell(_) => None,
            })
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notebook::document::{Cell, SeededIds};
    use crate::notebook::params::{ParamType, ParameterDef};

    fn doc(cells: &[(&str, Option<&str>, CellKind, &str)]) -> NotebookDoc {
        let mut doc = NotebookDoc::empty("n", "t");
        for (id, name, kind, source) in cells {
            let mut cell = Cell::new(*id, kind.clone(), Default::default());
            cell.name = name.map(str::to_string);
            cell.source = source.to_string();
            doc.cells.push(cell);
        }
        doc
    }

    fn ids(nodes: &[Node]) -> Vec<String> {
        nodes.iter().map(|n| n.to_string()).collect()
    }

    fn build(d: &NotebookDoc) -> Graph {
        Graph::build(d, &Dialects::default())
    }

    #[test]
    fn a_local_cell_naming_an_attachment_table_depends_on_the_attachment() {
        let mut d = doc(&[
            ("att", Some("segments_file"), CellKind::Attachment, ""),
            ("bare", None, CellKind::Sql, "SELECT * FROM Segments s"),
            ("quoted", None, CellKind::Sql, "SELECT * FROM \"segments\""),
            ("remote", None, CellKind::Sql, "SELECT * FROM segments"),
            ("text", None, CellKind::Sql, "SELECT 'segments'"),
            (
                "pic",
                None,
                CellKind::Markdown,
                "![map](attachment:segments_file)",
            ),
        ]);
        d.cells[0].spec = serde_json::json!({"relation": "segments"});
        for id in ["bare", "quoted", "text"] {
            let cell = d.cells.iter_mut().find(|c| c.id == id).unwrap();
            cell.spec = serde_json::json!({"target": "local"});
        }
        let g = build(&d);
        for id in ["bare", "quoted"] {
            assert_eq!(
                g.upstream(&Node::cell(id)),
                &[(Node::cell("att"), EdgeKind::Input)],
                "{id}"
            );
        }
        assert_eq!(
            g.upstream(&Node::cell("pic")),
            &[(Node::cell("att"), EdgeKind::Input)]
        );
        assert!(
            g.upstream(&Node::cell("remote")).is_empty(),
            "a remote table of that name"
        );
        let mut braced = d.clone();
        braced.cells[1].source = "SELECT * FROM {{segments_file}} JOIN segments USING (id)".into();
        assert_eq!(
            build(&braced).upstream(&Node::cell("bare")),
            &[(Node::cell("att"), EdgeKind::Sql)],
            "one edge for a cell both referenced and named"
        );
        assert!(
            g.upstream(&Node::cell("text")).is_empty(),
            "a string is no table"
        );
    }

    #[test]
    fn orders_by_dependency_then_document_order() {
        let mut d = doc(&[
            ("a", Some("late"), CellKind::Sql, "SELECT * FROM {{early}}"),
            ("b", None, CellKind::Markdown, "Notes"),
            ("c", Some("early"), CellKind::Sql, "SELECT :region"),
            ("d", None, CellKind::Chart, ""),
        ]);
        d.cells[3].spec = serde_json::json!({"input": "a"});
        d.parameters
            .push(ParameterDef::new("region", ParamType::String));
        let g = build(&d);
        assert_eq!(
            ids(g.topological_order().unwrap()),
            vec!["param:region", "b", "c", "a", "d"]
        );
        assert_eq!(
            g.upstream(&Node::cell("a")),
            &[(Node::cell("c"), EdgeKind::Sql)]
        );
        assert_eq!(
            g.upstream(&Node::cell("d")),
            &[(Node::cell("a"), EdgeKind::Input)]
        );
        assert_eq!(
            ids(&g.upstream_closure(&Node::cell("d"))),
            vec!["param:region", "c", "a"]
        );
        assert_eq!(
            ids(&g.downstream_closure(&Node::param("region"))),
            vec!["c", "a", "d"]
        );
        assert_eq!(g.params_of("c"), vec!["region"]);
        assert!(g.params_of("a").is_empty());
        assert!(g.unresolved().is_empty());
    }

    #[test]
    fn records_unresolved_references() {
        let mut d = doc(&[
            (
                "a",
                None,
                CellKind::Sql,
                "SELECT * FROM {{ghost}} WHERE x = {{param.p}}",
            ),
            ("b", None, CellKind::Table, ""),
        ]);
        d.cells[1].spec = serde_json::json!({"input": "nowhere"});
        let mut p = ParameterDef::new("opts", ParamType::Enum);
        p.allowed_values = Some(AllowedValues::FromCell {
            from_cell: "missing".into(),
            value_column: "v".into(),
            label_column: None,
            limit: None,
        });
        d.parameters.push(p);
        let g = build(&d);
        assert_eq!(
            g.unresolved(),
            &[
                Unresolved::Options {
                    param: "opts".into(),
                    cell: "missing".into()
                },
                Unresolved::Cell {
                    cell: "a".into(),
                    token: "ghost".into()
                },
                Unresolved::Param {
                    cell: "a".into(),
                    name: "p".into()
                },
                Unresolved::Input {
                    cell: "b".into(),
                    input: "nowhere".into()
                },
            ]
        );
        assert!(g.topological_order().is_ok());
    }

    #[test]
    fn reports_the_full_cycle_and_a_fix() {
        let d = doc(&[
            ("a", Some("first"), CellKind::Sql, "SELECT * FROM {{third}}"),
            (
                "b",
                Some("second"),
                CellKind::Sql,
                "SELECT * FROM {{first}}",
            ),
            (
                "c",
                Some("third"),
                CellKind::Sql,
                "SELECT * FROM {{second}}",
            ),
            ("d", None, CellKind::Sql, "SELECT * FROM {{third}}"),
            ("e", None, CellKind::Sql, "SELECT 1"),
        ]);
        let g = build(&d);
        let cycle = g.topological_order().unwrap_err();
        assert_eq!(ids(&cycle.path), vec!["a", "c", "b", "a"]);
        assert_eq!(cycle.labels, vec!["first", "third", "second", "first"]);
        assert_eq!(
            (cycle.dependent.clone(), cycle.upstream.clone()),
            (Node::cell("a"), Node::cell("c"))
        );
        assert_eq!(cycle.message().key(), Some("notebook.core.cycle.loop"));
        assert_eq!(
            cycle.suggestion().fallback(),
            "Remove the reference to third from first"
        );
        assert_eq!(g.cycles().len(), 1);
        assert_eq!(ids(g.order()), vec!["e"]);
        assert!(g.cyclic().contains(&Node::cell("d")));
    }

    #[test]
    fn self_reference_is_a_cycle_of_one() {
        let d = doc(&[("a", Some("me"), CellKind::Sql, "SELECT * FROM {{me}}")]);
        let g = build(&d);
        let cycle = g.topological_order().unwrap_err();
        assert_eq!(ids(&cycle.path), vec!["a", "a"]);
        assert_eq!(cycle.dependent, Node::cell("a"));
    }

    #[test]
    fn cycles_through_parameters_are_found() {
        let mut d = doc(&[("a", None, CellKind::Sql, "SELECT id FROM t WHERE r = :pick")]);
        let mut p = ParameterDef::new("pick", ParamType::Enum);
        p.allowed_values = Some(AllowedValues::FromCell {
            from_cell: "a".into(),
            value_column: "id".into(),
            label_column: None,
            limit: Some(100),
        });
        d.parameters.push(p);
        let g = build(&d);
        let cycle = g.topological_order().unwrap_err();
        assert_eq!(ids(&cycle.path), vec!["param:pick", "a", "param:pick"]);
    }

    #[test]
    fn parameter_cells_follow_their_parameter() {
        let mut d = doc(&[("w", None, CellKind::Parameter, "")]);
        d.cells[0].spec = serde_json::json!({"parameter": "Region"});
        d.parameters
            .push(ParameterDef::new("region", ParamType::String));
        let g = build(&d);
        assert_eq!(
            g.upstream(&Node::cell("w")),
            &[(Node::param("region"), EdgeKind::Param)]
        );
    }

    #[test]
    fn duplicate_edges_collapse_and_node_text_round_trips() {
        let d = doc(&[
            ("a", None, CellKind::Sql, "SELECT 1"),
            (
                "b",
                None,
                CellKind::Sql,
                "SELECT * FROM {{a}} JOIN {{a}} ON 1=1",
            ),
        ]);
        let g = build(&d);
        assert_eq!(g.upstream(&Node::cell("b")).len(), 1);
        assert_eq!(Node::parse("param:x"), Node::param("x"));
        assert_eq!(Node::parse("c_1"), Node::cell("c_1"));
        assert!(EdgeKind::Input.needs_output() && !EdgeKind::Sql.needs_output());
    }

    /// Random DAGs (edges only point up the document) always sort with every
    /// dependency first; adding one back edge always yields a cycle whose
    /// consecutive pairs are real edges.
    #[test]
    fn randomized_graphs_sort_and_cycles_are_real() {
        let mut ids_src = SeededIds(2024);
        let mut next = || {
            let mut b = [0u8; 1];
            crate::notebook::document::IdSource::fill(&mut ids_src, &mut b);
            b[0] as usize
        };
        for _ in 0..200 {
            let n = 2 + next() % 12;
            let mut cells: Vec<(String, String)> = Vec::new();
            for i in 0..n {
                let mut refs = Vec::new();
                for j in 0..i {
                    if next() % 3 == 0 {
                        refs.push(format!("{{{{n{j}}}}}"));
                    }
                }
                cells.push((format!("n{i}"), format!("SELECT 1 {}", refs.join(" "))));
            }
            let make = |cells: &[(String, String)]| {
                let mut d = NotebookDoc::empty("x", "x");
                // Shuffle document order so ties are exercised.
                for (id, src) in cells.iter().rev() {
                    let mut c = Cell::new(id.clone(), CellKind::Sql, Default::default());
                    c.source = src.clone();
                    d.cells.push(c);
                }
                d
            };
            let g = build(&make(&cells));
            let order = g.topological_order().expect("acyclic");
            let pos: BTreeMap<&Node, usize> =
                order.iter().enumerate().map(|(i, n)| (n, i)).collect();
            for node in order {
                for (up, _) in g.upstream(node) {
                    assert!(pos[up] < pos[node]);
                }
            }
            assert_eq!(build(&make(&cells)).order(), order);
            let last = n - 1;
            cells[0].1.push_str(&format!(" {{{{n{last}}}}}"));
            cells[last].1.push_str(" {{n0}}");
            let g = build(&make(&cells));
            let cycle = g.topological_order().expect_err("cyclic");
            assert_eq!(cycle.path.first(), cycle.path.last());
            for pair in cycle.path.windows(2) {
                assert!(g.upstream(&pair[0]).iter().any(|(u, _)| *u == pair[1]));
            }
        }
    }
}
