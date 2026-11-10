//! `SQLLY.ON <target> <command …>`: run one command on the cluster nodes
//! the target names instead of routing it by key slot (Redis Insight #2809:
//! "run this on that node", `FLUSHALL` on every master, a replica's
//! `INFO replication`).
//!
//! The target is `masters`, `replicas`, `all`, a node's `host:port` as the
//! Cluster Topology view lists it, or several of those separated by commas.
//! Each targeted node gets its own short-lived connection for the line,
//! dialed through the node map and any per-node tunnel exactly like the
//! routed connection, so the command reaches that node and only that node:
//! no `MOVED` / `ASK` redirect is followed (like `redis-cli` without `-c`),
//! and a replica is put in `READONLY` mode first so it answers key reads.
//! Because the line never goes through slot routing, the commands a cluster
//! connection refuses by rule (`KEYS`, `SCAN`, …) run here on the chosen
//! node.
//!
//! Every node is connected before anything runs, so an unreachable node
//! stops the line before any node has run the command. A node that answers
//! with an error gets its own row; the others keep their answers. One
//! `host:port` target returns the command's own result; anything else
//! returns one grid with `node` and `role` leading each row.
//!
//! Safety is unchanged: the gates classify the wrapped command (see
//! `sqlly_intel::query::safety::redis::RedisCommand::on_node`), so a write
//! under `SQLLY.ON` is still a write for read-only connections, mutation
//! review and the destructive-command confirmation.

use redis::ProtocolVersion;
use sqlly_core::{CellValue, StudioError, StudioResult};
use sqlly_intel::query::safety::redis::{ON_NODE_COMMAND, RedisCommand};

use super::RedisClient;
use super::cluster::{self, TopologyNode};
use super::shape::{self, Shaped, shape_reply};

/// One part of a `SQLLY.ON` target list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeTarget {
    Masters,
    Replicas,
    All,
    /// A node's `host:port` (as advertised, or its announced hostname).
    Node {
        host: String,
        port: u16,
    },
}

fn invalid(text: String) -> StudioError {
    StudioError::QueryExecution(text)
}

/// Parse a target list: keywords (any case) and `host:port` addresses
/// (`[v6]:port` for IPv6), separated by commas.
pub fn parse_targets(text: &str) -> StudioResult<Vec<NodeTarget>> {
    let mut out = Vec::new();
    for part in text.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        let target = match part.to_ascii_lowercase().as_str() {
            "masters" | "master" => NodeTarget::Masters,
            "replicas" | "replica" => NodeTarget::Replicas,
            "all" => NodeTarget::All,
            _ => match sqlly_core::redis_cluster::split_host_port(part, 0) {
                Some((host, port)) if port != 0 => NodeTarget::Node {
                    host: host.to_string(),
                    port,
                },
                _ => {
                    return Err(invalid(format!(
                        "{ON_NODE_COMMAND} target '{part}' is not masters, replicas, all or a node's host:port"
                    )));
                }
            },
        };
        if !out.contains(&target) {
            out.push(target);
        }
    }
    if out.is_empty() {
        return Err(invalid(format!(
            "{ON_NODE_COMMAND} needs a target: masters, replicas, all, or a node's host:port"
        )));
    }
    Ok(out)
}

/// True when the target list names exactly one node by address, which
/// answers with the command's own result rather than the per-node grid.
pub fn is_single_address(targets: &[NodeTarget]) -> bool {
    matches!(targets, [NodeTarget::Node { .. }])
}

fn strip_brackets(host: &str) -> &str {
    host.trim_start_matches('[').trim_end_matches(']')
}

/// Whether `node` answers to `host:port`: its advertised address, the
/// endpoint the cluster tells clients to use, or its announced hostname.
fn node_matches(node: &TopologyNode, host: &str, port: u16) -> bool {
    if node.port() != Some(port) {
        return false;
    }
    let host = strip_brackets(host);
    [
        Some(node.host()),
        Some(node.endpoint.clone()),
        node.hostname.clone(),
    ]
    .into_iter()
    .flatten()
    .any(|candidate| strip_brackets(&candidate).eq_ignore_ascii_case(host))
}

/// The nodes a target list selects, in topology order (masters first, each
/// followed by its replicas), each once. Keywords skip nodes the cluster
/// has marked failed; an address names its node whatever its health.
pub fn resolve_targets<'a>(
    targets: &[NodeTarget],
    nodes: &'a [TopologyNode],
) -> StudioResult<Vec<&'a TopologyNode>> {
    let mut picked = vec![false; nodes.len()];
    for target in targets {
        match target {
            NodeTarget::Node { host, port } => {
                let Some(i) = nodes.iter().position(|n| node_matches(n, host, *port)) else {
                    let known: Vec<&str> = nodes.iter().map(|n| n.address.as_str()).collect();
                    return Err(invalid(format!(
                        "{ON_NODE_COMMAND}: no cluster node answers to {host}:{port}; the cluster's nodes are {}",
                        known.join(", ")
                    )));
                };
                picked[i] = true;
            }
            keyword => {
                for (i, node) in nodes.iter().enumerate() {
                    let role_fits = match keyword {
                        NodeTarget::Masters => node.role == "master",
                        NodeTarget::Replicas => node.role != "master",
                        _ => true,
                    };
                    if role_fits && node.health != "failed" {
                        picked[i] = true;
                    }
                }
            }
        }
    }
    let selected: Vec<&TopologyNode> = nodes
        .iter()
        .zip(picked)
        .filter_map(|(node, picked)| picked.then_some(node))
        .collect();
    if selected.is_empty() {
        return Err(invalid(format!(
            "{ON_NODE_COMMAND}: the target names no node that is online (the cluster lists {} nodes)",
            nodes.len()
        )));
    }
    Ok(selected)
}

/// Why a wrapped command cannot run under `SQLLY.ON`, if it cannot.
pub fn refusal(inner: &RedisCommand) -> Option<String> {
    let name = inner.name();
    if name.starts_with("SQLLY.") {
        return Some(format!(
            "{ON_NODE_COMMAND} runs server commands; {name} is SQLly's own command and already covers the whole cluster"
        ));
    }
    if matches!(
        name.as_str(),
        "MULTI" | "EXEC" | "DISCARD" | "WATCH" | "UNWATCH"
    ) {
        return Some(format!(
            "{name} cannot run under {ON_NODE_COMMAND}: each line opens its own connection to the node, so a transaction would not span lines"
        ));
    }
    None
}

/// One node's answer, for [`combine`].
pub struct NodeAnswer {
    /// The node's advertised `host:port`.
    pub node: String,
    /// `master` or `replica`.
    pub role: String,
    /// The shaped reply, or the node's error text.
    pub result: Result<Shaped, String>,
}

/// Several nodes' answers as one grid: `node` and `role`, then every
/// column any answer has (in first-seen order, `NULL` where a node's answer
/// lacks it), then `error` when some node answered with one.
pub fn combine(answers: Vec<NodeAnswer>) -> Shaped {
    let mut columns: Vec<sqlly_core::ResultColumn> = Vec::new();
    let mut any_error = false;
    for answer in &answers {
        match &answer.result {
            Ok(shaped) => {
                for column in &shaped.columns {
                    if !columns.iter().any(|c| c.name == column.name) {
                        columns.push(column.clone());
                    }
                }
            }
            Err(_) => any_error = true,
        }
    }
    let width = columns.len();
    let mut rows = Vec::new();
    for answer in answers {
        let lead = |cells: &mut Vec<CellValue>| {
            cells.push(CellValue::String(answer.node.clone()));
            cells.push(CellValue::String(answer.role.clone()));
        };
        match answer.result {
            Ok(shaped) => {
                let positions: Vec<usize> = shaped
                    .columns
                    .iter()
                    .map(|c| {
                        columns
                            .iter()
                            .position(|u| u.name == c.name)
                            .unwrap_or_default()
                    })
                    .collect();
                for row in shaped.rows {
                    let mut cells = Vec::with_capacity(width + 3);
                    lead(&mut cells);
                    let mut values = vec![CellValue::Null; width];
                    for (cell, &at) in row.into_iter().zip(&positions) {
                        values[at] = cell;
                    }
                    cells.extend(values);
                    if any_error {
                        cells.push(CellValue::Null);
                    }
                    rows.push(cells);
                }
            }
            Err(text) => {
                let mut cells = Vec::with_capacity(width + 3);
                lead(&mut cells);
                cells.extend(std::iter::repeat_n(CellValue::Null, width));
                cells.push(CellValue::String(text));
                rows.push(cells);
            }
        }
    }
    let mut all_columns = vec![
        shape::column("node", "string"),
        shape::column("role", "string"),
    ];
    all_columns.extend(columns);
    if any_error {
        all_columns.push(shape::column("error", "string"));
    }
    Shaped {
        columns: all_columns,
        rows,
    }
}

/// The address a node is dialed by: the endpoint the cluster tells
/// clients to use, or its own address when it announces none; a node that
/// announces an empty address is the seed the connection opened with (the
/// routed connection makes the same substitution).
fn advertised_host(node: &TopologyNode, seed_host: &str) -> String {
    let host = if node.endpoint.is_empty() || node.endpoint == "?" {
        node.host()
    } else {
        node.endpoint.clone()
    };
    if host.is_empty() {
        seed_host.to_string()
    } else {
        host
    }
}

/// A connection of its own to one node: the profile cut to one host and
/// database 0, the node map and per-node tunnel applied, and the TLS name
/// the routed connection would send (the advertised host) when the socket
/// goes elsewhere.
async fn open_node(
    client: &RedisClient,
    handle: &cluster::ClusterHandle,
    node: &TopologyNode,
) -> StudioResult<RedisClient> {
    let profile = client.entra_fresh_profile(handle.dialer().profile());
    let mut node_profile = cluster::single_host(&profile).into_owned();
    node_profile.database = None;
    let Some(port) = node.port() else {
        return Err(StudioError::Connection(format!(
            "cluster node {} announces no port",
            node.address
        )));
    };
    let host = advertised_host(node, handle.seed_host());
    let (dial_host, dial_port) = handle.dialer().dial_target(&host, port).await?;
    let tls_name = (!(strip_brackets(&host).eq_ignore_ascii_case(strip_brackets(&dial_host))
        && port == dial_port))
        .then(|| (host.clone(), port));
    let opened = match super::connect_named(
        &node_profile,
        &dial_host,
        dial_port,
        client.protocol(),
        tls_name.clone(),
    )
    .await
    {
        Err(super::Refusal::Hello) => {
            super::connect_named(
                &node_profile,
                &dial_host,
                dial_port,
                ProtocolVersion::RESP2,
                tls_name,
            )
            .await
        }
        other => other,
    }
    .map_err(super::Refusal::into_error)?;
    let opened = super::with_entra_session(opened, &node_profile)?;
    super::setname::name_connection(&opened, &node_profile).await;
    Ok(opened)
}

/// Run the wrapped command on one node's connection: `READONLY` first on
/// a replica (so it serves key reads instead of redirecting them), then
/// the command, shaped like any other reply.
async fn run_on_node(
    conn: &RedisClient,
    node: &TopologyNode,
    inner: &RedisCommand,
) -> StudioResult<Shaped> {
    if node.role != "master" {
        // A server that does not know READONLY still runs keyless commands.
        let _ = conn.run(&redis::cmd("READONLY")).await;
    }
    let value = conn.run_args(&inner.args).await?;
    Ok(shape_reply(&inner.args, value))
}

fn error_text(error: StudioError) -> String {
    match error {
        StudioError::QueryExecution(text)
        | StudioError::Connection(text)
        | StudioError::PermissionDenied(text) => text,
        other => other.to_string(),
    }
}

/// `SQLLY.ON <target> <command …>` (see the module docs).
pub(super) async fn run(
    client: &RedisClient,
    target: &str,
    inner: &RedisCommand,
) -> StudioResult<Shaped> {
    if let Some(reason) = refusal(inner) {
        return Err(invalid(reason));
    }
    let targets = parse_targets(target)?;
    let Some(handle) = client.cluster() else {
        return Err(invalid(format!(
            "{ON_NODE_COMMAND} needs a Redis Cluster connection; this server is standalone, so run the command without the prefix"
        )));
    };
    // The command may change the keyspace: later analyzer reports in the
    // same script must resample.
    client.analysis_cache.clear();
    let topology = cluster::topology(client).await?;
    let nodes = resolve_targets(&targets, &topology)?;
    // Connect to every node first: an unreachable node stops the line
    // before any node has run the command.
    let opened = tokio::select! {
        biased;
        _ = client.cancel.cancelled() => return Err(StudioError::QueryCancelled),
        opened = futures::future::join_all(nodes.iter().map(|node| open_node(client, &handle, node))) => opened,
    };
    let mut conns = Vec::with_capacity(opened.len());
    for (node, conn) in nodes.iter().zip(opened) {
        conns.push(conn.map_err(|e| {
            StudioError::Connection(format!(
                "{ON_NODE_COMMAND}: cannot reach {} ({}), so the command ran on no node: {}",
                node.address,
                node.role,
                error_text(e)
            ))
        })?);
    }
    let answers = tokio::select! {
        biased;
        _ = client.cancel.cancelled() => return Err(StudioError::QueryCancelled),
        answers = futures::future::join_all(
            nodes.iter().zip(&conns).map(|(node, conn)| run_on_node(conn, node, inner)),
        ) => answers,
    };
    if is_single_address(&targets) && nodes.len() == 1 {
        return answers
            .into_iter()
            .next()
            .unwrap_or_else(|| Err(invalid(format!("{ON_NODE_COMMAND}: no answer"))));
    }
    let mut out = Vec::with_capacity(answers.len());
    for (node, answer) in nodes.iter().zip(answers) {
        let result = match answer {
            Ok(shaped) => Ok(shaped),
            Err(StudioError::QueryCancelled) => return Err(StudioError::QueryCancelled),
            Err(error) => Err(error_text(error)),
        };
        out.push(NodeAnswer {
            node: node.address.clone(),
            role: node.role.clone(),
            result,
        });
    }
    Ok(combine(out))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlly_intel::query::safety::redis::parse_script;

    #[test]
    fn targets_parse_keywords_addresses_and_lists() {
        assert_eq!(parse_targets("masters").unwrap(), vec![NodeTarget::Masters]);
        assert_eq!(
            parse_targets("REPLICAS").unwrap(),
            vec![NodeTarget::Replicas]
        );
        assert_eq!(parse_targets("All").unwrap(), vec![NodeTarget::All]);
        assert_eq!(
            parse_targets("10.0.0.1:7000").unwrap(),
            vec![NodeTarget::Node {
                host: "10.0.0.1".into(),
                port: 7000
            }]
        );
        assert_eq!(
            parse_targets("[::1]:7001, masters,masters").unwrap(),
            vec![
                NodeTarget::Node {
                    host: "::1".into(),
                    port: 7001
                },
                NodeTarget::Masters
            ]
        );
        assert!(is_single_address(
            &parse_targets("node-a.example:7000").unwrap()
        ));
        assert!(!is_single_address(&parse_targets("a:1,b:2").unwrap()));
        assert!(!is_single_address(&parse_targets("masters").unwrap()));
        for bad in ["", " , ", "10.0.0.1", "nodes-a", "host:notaport"] {
            assert!(parse_targets(bad).is_err(), "{bad:?}");
        }
    }

    /// The shared six-node fixture: three masters, three replicas, one
    /// replica suspected and one disconnected, one master announcing a
    /// hostname.
    fn fixture() -> Vec<TopologyNode> {
        cluster::parse_cluster_nodes(cluster::tests::NODES_FIXTURE)
    }

    fn addresses(nodes: &[&TopologyNode]) -> Vec<String> {
        nodes.iter().map(|n| n.address.clone()).collect()
    }

    #[test]
    fn targets_resolve_to_nodes_in_topology_order() {
        let nodes = fixture();
        let masters = resolve_targets(&[NodeTarget::Masters], &nodes).unwrap();
        assert_eq!(
            addresses(&masters),
            vec!["10.0.0.1:7000", "10.0.0.2:7001", "10.0.0.3:7002"]
        );
        let replicas = resolve_targets(&[NodeTarget::Replicas], &nodes).unwrap();
        assert_eq!(replicas.len(), 3);
        assert!(replicas.iter().all(|n| n.role == "replica"));
        assert_eq!(
            resolve_targets(&[NodeTarget::All], &nodes).unwrap().len(),
            6
        );
        // An address by its advertised IP or its announced hostname, once.
        let by_name = resolve_targets(
            &parse_targets("node-b.example.com:7001,10.0.0.2:7001,10.0.0.4:7003").unwrap(),
            &nodes,
        )
        .unwrap();
        assert_eq!(addresses(&by_name), vec!["10.0.0.2:7001", "10.0.0.4:7003"]);
        // Masters plus one replica keeps topology order.
        let mixed =
            resolve_targets(&parse_targets("10.0.0.6:7005,masters").unwrap(), &nodes).unwrap();
        assert_eq!(mixed.len(), 4);
        assert_eq!(mixed[0].address, "10.0.0.1:7000");
        let unknown = resolve_targets(&parse_targets("10.9.9.9:7000").unwrap(), &nodes)
            .err()
            .unwrap()
            .to_string();
        assert!(unknown.contains("10.9.9.9:7000"), "{unknown}");
        assert!(unknown.contains("10.0.0.1:7000"), "{unknown}");
    }

    #[test]
    fn keywords_skip_failed_nodes_but_addresses_do_not() {
        let mut nodes = fixture();
        nodes
            .iter_mut()
            .find(|n| n.address == "10.0.0.3:7002")
            .unwrap()
            .health = "failed".into();
        let masters = resolve_targets(&[NodeTarget::Masters], &nodes).unwrap();
        assert_eq!(masters.len(), 2);
        let direct = resolve_targets(&parse_targets("10.0.0.3:7002").unwrap(), &nodes).unwrap();
        assert_eq!(direct.len(), 1);
    }

    #[test]
    fn wrapped_sqlly_and_transaction_commands_are_refused() {
        let inner = |text: &str| parse_script(text).unwrap().remove(0);
        assert!(refusal(&inner("SQLLY.KEYS *")).is_some());
        assert!(refusal(&inner("MULTI")).is_some());
        assert!(refusal(&inner("exec")).is_some());
        assert!(refusal(&inner("FLUSHALL")).is_none());
        assert!(refusal(&inner("KEYS *")).is_none());
        assert!(refusal(&inner("SCAN 0")).is_none());
    }

    #[test]
    fn answers_combine_into_one_grid_with_node_role_and_errors() {
        let dbsize = |n: i64| Shaped::new(&[("result", "integer")], vec![vec![CellValue::I64(n)]]);
        let combined = combine(vec![
            NodeAnswer {
                node: "10.0.0.1:7000".into(),
                role: "master".into(),
                result: Ok(dbsize(5)),
            },
            NodeAnswer {
                node: "10.0.0.4:7003".into(),
                role: "replica".into(),
                result: Err("LOADING Redis is loading the dataset in memory".into()),
            },
            NodeAnswer {
                node: "10.0.0.2:7001".into(),
                role: "master".into(),
                result: Ok(Shaped::new(
                    &[("result", "integer"), ("extra", "string")],
                    vec![
                        vec![CellValue::I64(7), CellValue::String("x".into())],
                        vec![CellValue::I64(8), CellValue::String("y".into())],
                    ],
                )),
            },
        ]);
        let names: Vec<&str> = combined.columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["node", "role", "result", "extra", "error"]);
        assert_eq!(combined.rows.len(), 4);
        assert_eq!(
            combined.rows[0],
            vec![
                CellValue::String("10.0.0.1:7000".into()),
                CellValue::String("master".into()),
                CellValue::I64(5),
                CellValue::Null,
                CellValue::Null,
            ]
        );
        assert_eq!(
            combined.rows[1][4],
            CellValue::String("LOADING Redis is loading the dataset in memory".into())
        );
        assert_eq!(combined.rows[1][2], CellValue::Null);
        assert_eq!(combined.rows[3][3], CellValue::String("y".into()));
        // No error column when every node answered.
        let clean = combine(vec![NodeAnswer {
            node: "a:1".into(),
            role: "master".into(),
            result: Ok(dbsize(1)),
        }]);
        let names: Vec<&str> = clean.columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["node", "role", "result"]);
    }
}
