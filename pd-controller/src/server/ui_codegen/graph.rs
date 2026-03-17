use super::abi_render::{
    additional_flow_action_statement, is_additional_flow_block, is_additional_mixed_flow_block,
    is_additional_pure_value_block,
};
use super::catalog::ui_block_catalog;
use super::render::*;
use super::*;

#[derive(Clone, Debug)]
struct ResolvedUiNode {
    id: String,
    block: UiBlockInstance,
}

#[derive(Clone, Debug)]
struct ResolvedFlowEdge {
    source_output: String,
    target: String,
}

#[derive(Clone, Debug)]
struct ResolvedUiGraph {
    ordered_nodes: Vec<ResolvedUiNode>,
    flow_outgoing: HashMap<String, Vec<ResolvedFlowEdge>>,
    flow_incoming_count: HashMap<String, usize>,
    has_flow_edges: bool,
}

pub(super) fn render_ui_sources(
    blocks: &[UiBlockInstance],
    nodes: &[UiGraphNode],
    edges: &[UiGraphEdge],
) -> Result<UiSourceBundle, (StatusCode, Json<ErrorResponse>)> {
    if !blocks.is_empty() {
        return render_sources(blocks);
    }
    if nodes.is_empty() {
        return render_sources(&[]);
    }

    let resolved = resolve_ui_graph(nodes, edges)?;
    if !resolved.has_flow_edges {
        let ordered = resolved
            .ordered_nodes
            .into_iter()
            .map(|node| node.block)
            .collect::<Vec<_>>();
        return render_sources(&ordered);
    }
    render_sources_with_flow(&resolved)
}

fn resolve_ui_graph(
    nodes: &[UiGraphNode],
    edges: &[UiGraphEdge],
) -> Result<ResolvedUiGraph, (StatusCode, Json<ErrorResponse>)> {
    if nodes.len() > MAX_UI_BLOCKS {
        return Err(bad_request(&format!(
            "too many graph nodes: {} (limit {})",
            nodes.len(),
            MAX_UI_BLOCKS
        )));
    }

    let catalog = ui_block_catalog();
    let definition_map = catalog
        .iter()
        .map(|definition| (definition.id.to_string(), definition))
        .collect::<HashMap<_, _>>();
    let node_map = nodes
        .iter()
        .map(|node| (node.id.clone(), node))
        .collect::<HashMap<_, _>>();

    let mut data_incoming: HashMap<String, Vec<&UiGraphEdge>> = HashMap::new();
    let mut flow_outgoing: HashMap<String, Vec<ResolvedFlowEdge>> = HashMap::new();
    let mut flow_incoming_count: HashMap<String, usize> = HashMap::new();
    let mut indegree: HashMap<String, usize> = HashMap::new();
    let mut adjacency: HashMap<String, Vec<String>> = HashMap::new();
    let mut seen_data_targets: HashSet<(String, String)> = HashSet::new();
    let mut seen_flow_outputs: HashSet<(String, String)> = HashSet::new();
    let mut has_flow_edges = false;

    for node in nodes {
        indegree.insert(node.id.clone(), 0);
        flow_incoming_count.insert(node.id.clone(), 0);
    }

    for edge in edges {
        let Some(source_node) = node_map.get(&edge.source) else {
            return Err(bad_request(&format!(
                "edge references missing source node '{}'",
                edge.source
            )));
        };
        let Some(target_node) = node_map.get(&edge.target) else {
            return Err(bad_request(&format!(
                "edge references missing target node '{}'",
                edge.target
            )));
        };

        let Some(source_definition) = definition_map.get(&source_node.block_id) else {
            return Err(bad_request(&format!(
                "unknown block_id '{}' for source node '{}'",
                source_node.block_id, source_node.id
            )));
        };
        let Some(source_output) = source_definition
            .outputs
            .iter()
            .find(|output| output.key == edge.source_output)
        else {
            return Err(bad_request(&format!(
                "source output '{}' not found on block '{}'",
                edge.source_output, source_definition.id
            )));
        };

        let Some(target_definition) = definition_map.get(&target_node.block_id) else {
            return Err(bad_request(&format!(
                "unknown block_id '{}' for target node '{}'",
                target_node.block_id, target_node.id
            )));
        };

        match source_output.expr_from_input {
            Some(_) => {
                let Some(target_input) = target_definition
                    .inputs
                    .iter()
                    .find(|input| input.key == edge.target_input)
                else {
                    return Err(bad_request(&format!(
                        "target input '{}' not found on block '{}'",
                        edge.target_input, target_definition.id
                    )));
                };
                if !target_input.connectable {
                    return Err(bad_request(&format!(
                        "target input '{}' on block '{}' is not connectable",
                        edge.target_input, target_definition.id
                    )));
                }
                let data_target_key = (edge.target.clone(), edge.target_input.clone());
                if !seen_data_targets.insert(data_target_key) {
                    return Err(bad_request(&format!(
                        "target input '{}' on node '{}' has multiple data connections",
                        edge.target_input, edge.target
                    )));
                }
                data_incoming
                    .entry(edge.target.clone())
                    .or_default()
                    .push(edge);
            }
            None => {
                if edge.target_input != "__flow" {
                    return Err(bad_request(&format!(
                        "control output '{}' must connect to target_input='__flow'",
                        edge.source_output
                    )));
                }
                if !target_definition.accepts_flow {
                    return Err(bad_request(&format!(
                        "target block '{}' does not accept flow edges",
                        target_definition.id
                    )));
                }
                let flow_key = (edge.source.clone(), edge.source_output.clone());
                if !seen_flow_outputs.insert(flow_key) {
                    return Err(bad_request(&format!(
                        "source output '{}' on node '{}' already connected",
                        edge.source_output, edge.source
                    )));
                }
                flow_outgoing
                    .entry(edge.source.clone())
                    .or_default()
                    .push(ResolvedFlowEdge {
                        source_output: edge.source_output.clone(),
                        target: edge.target.clone(),
                    });
                let flow_incoming = flow_incoming_count.entry(edge.target.clone()).or_default();
                *flow_incoming += 1;
                if *flow_incoming > 1 {
                    return Err(bad_request(&format!(
                        "target node '{}' has multiple incoming flow edges",
                        edge.target
                    )));
                }
                has_flow_edges = true;
            }
        }

        *indegree.entry(edge.target.clone()).or_default() += 1;
        adjacency
            .entry(edge.source.clone())
            .or_default()
            .push(edge.target.clone());
    }

    let order_hint = nodes
        .iter()
        .enumerate()
        .map(|(index, node)| (node.id.clone(), index))
        .collect::<HashMap<_, _>>();
    for targets in adjacency.values_mut() {
        targets.sort_by_key(|node_id| order_hint.get(node_id).copied().unwrap_or(usize::MAX));
    }

    let mut queue = nodes
        .iter()
        .filter(|node| indegree.get(&node.id).copied().unwrap_or(0) == 0)
        .map(|node| node.id.clone())
        .collect::<VecDeque<_>>();
    let mut ordered_ids = Vec::with_capacity(nodes.len());
    while let Some(node_id) = queue.pop_front() {
        ordered_ids.push(node_id.clone());
        if let Some(targets) = adjacency.get(&node_id) {
            for target_id in targets {
                if let Some(entry) = indegree.get_mut(target_id) {
                    *entry = entry.saturating_sub(1);
                    if *entry == 0 {
                        queue.push_back(target_id.clone());
                    }
                }
            }
        }
    }
    if ordered_ids.len() != nodes.len() {
        return Err(bad_request(
            "graph contains a cycle; connect blocks as a directed acyclic graph",
        ));
    }

    for outgoing in flow_outgoing.values_mut() {
        outgoing.sort_by_key(|edge| order_hint.get(&edge.target).copied().unwrap_or(usize::MAX));
    }

    let mut ordered_nodes = Vec::with_capacity(nodes.len());
    for node_id in ordered_ids {
        let node = node_map
            .get(&node_id)
            .ok_or_else(|| bad_request("failed to resolve graph node"))?;
        let mut values = node.values.clone();

        if let Some(node_incoming) = data_incoming.get(&node_id) {
            for edge in node_incoming {
                let source_node = node_map
                    .get(&edge.source)
                    .ok_or_else(|| bad_request("failed to resolve edge source"))?;
                let source_definition = definition_map
                    .get(&source_node.block_id)
                    .ok_or_else(|| bad_request("failed to resolve source block definition"))?;
                let source_output = source_definition
                    .outputs
                    .iter()
                    .find(|output| output.key == edge.source_output)
                    .ok_or_else(|| bad_request("source output handle no longer exists"))?;
                let Some(expr_key) = source_output.expr_from_input else {
                    return Err(bad_request("source output does not expose an expression"));
                };
                let expr_name = source_node
                    .values
                    .get(expr_key)
                    .map(String::as_str)
                    .unwrap_or("value");
                let ident = sanitize_identifier(Some(&expr_name.to_string()), "value");
                values.insert(edge.target_input.clone(), format!("${ident}"));
            }
        }

        ordered_nodes.push(ResolvedUiNode {
            id: node.id.clone(),
            block: UiBlockInstance {
                block_id: node.block_id.clone(),
                values,
            },
        });
    }

    Ok(ResolvedUiGraph {
        ordered_nodes,
        flow_outgoing,
        flow_incoming_count,
        has_flow_edges,
    })
}

fn render_sources_with_flow(
    graph: &ResolvedUiGraph,
) -> Result<UiSourceBundle, (StatusCode, Json<ErrorResponse>)> {
    let mut rss_lines = vec!["use vm;".to_string(), String::new()];
    let mut js_lines = vec!["import * as vm from \"vm\";".to_string(), String::new()];
    let mut lua_lines = vec!["local vm = require(\"vm\")".to_string(), String::new()];
    let mut scm_lines = vec![
        "(require (prefix-in vm. \"vm\"))".to_string(),
        String::new(),
    ];

    let mut order_index = HashMap::new();
    let mut node_map = HashMap::new();
    for (index, node) in graph.ordered_nodes.iter().enumerate() {
        order_index.insert(node.id.clone(), index);
        node_map.insert(node.id.clone(), &node.block);
    }

    for node in &graph.ordered_nodes {
        if should_render_before_flow(node, graph) {
            render_single_block(
                &node.block,
                &mut rss_lines,
                &mut js_lines,
                &mut lua_lines,
                &mut scm_lines,
            )?;
        }
    }

    for node in &graph.ordered_nodes {
        if should_render_before_flow(node, graph) {
            continue;
        }
        if !is_flow_block(&node.block.block_id) {
            return Err(bad_request(&format!(
                "block '{}' is not flow-compatible when control edges are present",
                node.block.block_id
            )));
        }
    }

    let mut roots = graph
        .ordered_nodes
        .iter()
        .filter(|node| {
            !should_render_before_flow(node, graph)
                && graph
                    .flow_incoming_count
                    .get(&node.id)
                    .copied()
                    .unwrap_or(0)
                    == 0
        })
        .map(|node| node.id.clone())
        .collect::<Vec<_>>();
    roots.sort_by_key(|node_id| order_index.get(node_id).copied().unwrap_or(usize::MAX));

    let mut rendered = HashSet::new();
    let mut visiting = HashSet::new();
    for root in roots {
        let statements =
            render_flow_node(&root, graph, &node_map, &mut rendered, &mut visiting, 0)?;
        rss_lines.extend(statements.rustscript);
        js_lines.extend(statements.javascript);
        lua_lines.extend(statements.lua);
        scm_lines.extend(statements.scheme);
    }

    ensure_host_namespace_imports(
        &mut rss_lines,
        &mut js_lines,
        &mut lua_lines,
        &mut scm_lines,
    );

    Ok(UiSourceBundle {
        rustscript: join_lines(&rss_lines),
        javascript: join_lines(&js_lines),
        lua: join_lines(&lua_lines),
        scheme: join_lines(&scm_lines),
    })
}

fn should_render_before_flow(node: &ResolvedUiNode, graph: &ResolvedUiGraph) -> bool {
    is_value_block(&node.block.block_id)
        || (is_mixed_flow_block(&node.block.block_id) && !node_has_flow_connection(node, graph))
}

fn node_has_flow_connection(node: &ResolvedUiNode, graph: &ResolvedUiGraph) -> bool {
    graph
        .flow_incoming_count
        .get(&node.id)
        .copied()
        .unwrap_or(0)
        > 0
        || graph
            .flow_outgoing
            .get(&node.id)
            .map(|edges| !edges.is_empty())
            .unwrap_or(false)
}

fn is_value_block(block_id: &str) -> bool {
    matches!(
        block_id,
        "const_string"
            | "const_number"
            | "get_header"
            | "get_request_headers"
            | "get_request_query_arg"
            | "get_request_query_args"
            | "get_request_id"
            | "get_request_method"
            | "get_request_path"
            | "get_request_query"
            | "get_request_path_with_query"
            | "get_request_scheme"
            | "get_request_host"
            | "get_request_http_version"
            | "get_request_port"
            | "get_request_client_ip"
            | "get_request_body"
            | "get_request_body_next_chunk"
            | "get_request_body_eof"
            | "get_response_status"
            | "get_response_header"
            | "get_response_headers"
            | "get_response_body"
            | "get_upstream_response_status"
            | "get_upstream_response_header"
            | "get_upstream_response_headers"
            | "get_upstream_response_body"
            | "string_concat"
            | "string_length"
            | "string_slice"
            | "math_add"
            | "math_subtract"
            | "math_multiply"
            | "math_divide"
            | "array_new"
            | "array_push"
            | "array_get"
            | "array_set"
            | "map_new"
            | "map_get"
            | "map_set"
            | "json_encode"
            | "json_decode"
            | "rate_limit_allow"
    ) || is_additional_pure_value_block(block_id)
}

fn is_mixed_flow_block(block_id: &str) -> bool {
    is_additional_mixed_flow_block(block_id)
}

fn is_flow_block(block_id: &str) -> bool {
    matches!(
        block_id,
        "set_request_header"
            | "add_request_header"
            | "clear_request_header"
            | "set_request_method"
            | "set_request_path"
            | "set_request_query"
            | "set_request_query_arg"
            | "set_request_body"
            | "set_header"
            | "add_response_header"
            | "clear_response_header"
            | "set_response_content"
            | "set_response_status"
            | "set_upstream"
            | "runtime_sleep"
            | "rate_limit_if_else"
            | "if"
            | "loop"
    ) || is_additional_flow_block(block_id)
}

#[derive(Default)]
struct FlowStatements {
    rustscript: Vec<String>,
    javascript: Vec<String>,
    lua: Vec<String>,
    scheme: Vec<String>,
}

impl FlowStatements {
    fn extend(&mut self, mut other: FlowStatements) {
        self.rustscript.append(&mut other.rustscript);
        self.javascript.append(&mut other.javascript);
        self.lua.append(&mut other.lua);
        self.scheme.append(&mut other.scheme);
    }
}

fn render_flow_node(
    node_id: &str,
    graph: &ResolvedUiGraph,
    node_map: &HashMap<String, &UiBlockInstance>,
    rendered: &mut HashSet<String>,
    visiting: &mut HashSet<String>,
    indent: usize,
) -> Result<FlowStatements, (StatusCode, Json<ErrorResponse>)> {
    if rendered.contains(node_id) {
        return Ok(FlowStatements::default());
    }

    if !visiting.insert(node_id.to_string()) {
        return Err(bad_request(
            "flow graph contains a cycle; use loop blocks instead of back edges",
        ));
    }

    let block = node_map
        .get(node_id)
        .ok_or_else(|| bad_request("flow path references missing node"))?;

    let result = match block.block_id.as_str() {
        "set_request_header"
        | "add_request_header"
        | "clear_request_header"
        | "set_request_method"
        | "set_request_path"
        | "set_request_query"
        | "set_request_query_arg"
        | "set_request_body"
        | "set_header"
        | "add_response_header"
        | "clear_response_header"
        | "set_response_content"
        | "set_response_status"
        | "set_upstream"
        | "runtime_sleep" => {
            let mut statements = FlowStatements::default();
            let action = flow_action_statement(block)?;
            statements
                .rustscript
                .push(indent_line(indent, action.rustscript));
            statements
                .javascript
                .push(indent_line(indent, action.javascript));
            statements.lua.push(indent_line(indent, action.lua));
            statements.scheme.push(action.scheme);

            if let Some(next_target) = next_flow_target(node_id, graph)? {
                statements.extend(render_flow_node(
                    &next_target,
                    graph,
                    node_map,
                    rendered,
                    visiting,
                    indent,
                )?);
            }

            Ok(statements)
        }
        other if is_additional_flow_block(other) => {
            let mut statements = FlowStatements::default();
            let action = flow_action_statement(block)?;
            statements
                .rustscript
                .push(indent_line(indent, action.rustscript));
            statements
                .javascript
                .push(indent_line(indent, action.javascript));
            statements.lua.push(indent_line(indent, action.lua));
            statements.scheme.push(action.scheme);

            if let Some(next_target) = next_flow_target(node_id, graph)? {
                statements.extend(render_flow_node(
                    &next_target,
                    graph,
                    node_map,
                    rendered,
                    visiting,
                    indent,
                )?);
            }

            Ok(statements)
        }
        "rate_limit_if_else" => {
            let key_expr = block_value(block, "key_expr", "$header");
            let limit = sanitize_number(block.values.get("limit"), "3");
            let window = sanitize_number(block.values.get("window_seconds"), "60");
            let allowed_target =
                required_flow_target(node_id, graph, "allowed", "rate_limit_if_else")?;
            let blocked_target =
                required_flow_target(node_id, graph, "blocked", "rate_limit_if_else")?;

            let allowed_branch = render_flow_node(
                &allowed_target,
                graph,
                node_map,
                rendered,
                visiting,
                indent + 1,
            )?;
            let blocked_branch = render_flow_node(
                &blocked_target,
                graph,
                node_map,
                rendered,
                visiting,
                indent + 1,
            )?;

            let mut statements = FlowStatements::default();
            statements.rustscript.push(indent_line(
                indent,
                format!(
                    "if rate_limit::allow({}, {}, {}) {{",
                    render_expr_rss(key_expr),
                    limit,
                    window
                ),
            ));
            statements
                .rustscript
                .extend(allowed_branch.rustscript.clone());
            statements
                .rustscript
                .push(indent_line(indent, "} else {".to_string()));
            statements
                .rustscript
                .extend(blocked_branch.rustscript.clone());
            statements
                .rustscript
                .push(indent_line(indent, "}".to_string()));

            statements.javascript.push(indent_line(
                indent,
                format!(
                    "if (vm.rate_limit.allow({}, {}, {})) {{",
                    render_expr_js(key_expr),
                    limit,
                    window
                ),
            ));
            statements
                .javascript
                .extend(allowed_branch.javascript.clone());
            statements
                .javascript
                .push(indent_line(indent, "} else {".to_string()));
            statements
                .javascript
                .extend(blocked_branch.javascript.clone());
            statements
                .javascript
                .push(indent_line(indent, "}".to_string()));

            statements.lua.push(indent_line(
                indent,
                format!(
                    "if vm.rate_limit.allow({}, {}, {}) then",
                    render_expr_lua(key_expr),
                    limit,
                    window
                ),
            ));
            statements.lua.extend(allowed_branch.lua.clone());
            statements.lua.push(indent_line(indent, "else".to_string()));
            statements.lua.extend(blocked_branch.lua.clone());
            statements.lua.push(indent_line(indent, "end".to_string()));

            statements.scheme.push(format!(
                "(if (vm.rate_limit.allow {} {} {}) {} {})",
                render_expr_scheme(key_expr),
                limit,
                window,
                scheme_branch_expr(&allowed_branch.scheme),
                scheme_branch_expr(&blocked_branch.scheme)
            ));
            Ok(statements)
        }
        "if" => {
            let lhs = block_value(block, "lhs", "left");
            let rhs = block_value(block, "rhs", "right");
            let true_target = required_flow_target(node_id, graph, "true", "if")?;
            let false_target = optional_flow_target(node_id, graph, "false");

            let true_branch = render_flow_node(
                &true_target,
                graph,
                node_map,
                rendered,
                visiting,
                indent + 1,
            )?;
            let false_branch = if let Some(target) = false_target {
                Some(render_flow_node(
                    &target,
                    graph,
                    node_map,
                    rendered,
                    visiting,
                    indent + 1,
                )?)
            } else {
                None
            };

            let mut statements = FlowStatements::default();
            statements.rustscript.push(indent_line(
                indent,
                format!("if {} == {} {{", render_expr_rss(lhs), render_expr_rss(rhs)),
            ));
            statements.rustscript.extend(true_branch.rustscript.clone());
            if let Some(false_branch) = &false_branch {
                statements
                    .rustscript
                    .push(indent_line(indent, "} else {".to_string()));
                statements
                    .rustscript
                    .extend(false_branch.rustscript.clone());
            }
            statements
                .rustscript
                .push(indent_line(indent, "}".to_string()));

            statements.javascript.push(indent_line(
                indent,
                format!(
                    "if ({} === {}) {{",
                    render_expr_js(lhs),
                    render_expr_js(rhs)
                ),
            ));
            statements.javascript.extend(true_branch.javascript.clone());
            if let Some(false_branch) = &false_branch {
                statements
                    .javascript
                    .push(indent_line(indent, "} else {".to_string()));
                statements
                    .javascript
                    .extend(false_branch.javascript.clone());
            }
            statements
                .javascript
                .push(indent_line(indent, "}".to_string()));

            statements.lua.push(indent_line(
                indent,
                format!(
                    "if {} == {} then",
                    render_expr_lua(lhs),
                    render_expr_lua(rhs)
                ),
            ));
            statements.lua.extend(true_branch.lua.clone());
            if let Some(false_branch) = &false_branch {
                statements.lua.push(indent_line(indent, "else".to_string()));
                statements.lua.extend(false_branch.lua.clone());
            }
            statements.lua.push(indent_line(indent, "end".to_string()));

            let scheme_false = false_branch
                .as_ref()
                .map(|branch| scheme_branch_expr(&branch.scheme))
                .unwrap_or_else(|| "null".to_string());
            statements.scheme.push(format!(
                "(if (== {} {}) {} {})",
                render_expr_scheme(lhs),
                render_expr_scheme(rhs),
                scheme_branch_expr(&true_branch.scheme),
                scheme_false
            ));
            Ok(statements)
        }
        "loop" => {
            let count = render_number_expr(block_value(block, "count", "1"), "1");
            let body_target = required_flow_target(node_id, graph, "body", "loop")?;
            let done_target = required_flow_target(node_id, graph, "done", "loop")?;

            let body_branch = render_flow_node(
                &body_target,
                graph,
                node_map,
                rendered,
                visiting,
                indent + 1,
            )?;
            let done_branch =
                render_flow_node(&done_target, graph, node_map, rendered, visiting, indent)?;

            let mut statements = FlowStatements::default();
            statements.rustscript.push(indent_line(
                indent,
                format!("for (let mut i = 0; i < {count}; i = i + 1) {{"),
            ));
            statements.rustscript.extend(body_branch.rustscript.clone());
            statements
                .rustscript
                .push(indent_line(indent, "}".to_string()));
            statements.rustscript.extend(done_branch.rustscript.clone());

            statements.javascript.push(indent_line(
                indent,
                format!("for (let i = 0; i < {count}; i = i + 1) {{"),
            ));
            statements.javascript.extend(body_branch.javascript.clone());
            statements
                .javascript
                .push(indent_line(indent, "}".to_string()));
            statements.javascript.extend(done_branch.javascript.clone());

            statements
                .lua
                .push(indent_line(indent, format!("for i = 1, {count}, 1 do")));
            statements.lua.extend(body_branch.lua.clone());
            statements.lua.push(indent_line(indent, "end".to_string()));
            statements.lua.extend(done_branch.lua.clone());

            statements.scheme.push(format!(
                "(let loop ((i 0)) (if (< i {count}) (begin {} (loop (+ i 1))) 'done))",
                scheme_branch_expr(&body_branch.scheme)
            ));
            statements.scheme.extend(done_branch.scheme.clone());
            Ok(statements)
        }
        other => Err(bad_request(&format!(
            "unsupported flow node block '{}'",
            other
        ))),
    };

    visiting.remove(node_id);
    if result.is_ok() {
        rendered.insert(node_id.to_string());
    }
    result
}

fn indent_line(level: usize, line: String) -> String {
    format!("{}{}", "    ".repeat(level), line)
}

fn next_flow_target(
    node_id: &str,
    graph: &ResolvedUiGraph,
) -> Result<Option<String>, (StatusCode, Json<ErrorResponse>)> {
    let outgoing = graph
        .flow_outgoing
        .get(node_id)
        .cloned()
        .unwrap_or_default();
    if outgoing.is_empty() {
        return Ok(None);
    }
    if outgoing.len() != 1 || outgoing[0].source_output != "next" {
        return Err(bad_request(
            "action blocks can only use a single 'next' outgoing flow edge",
        ));
    }
    Ok(Some(outgoing[0].target.clone()))
}

fn required_flow_target(
    node_id: &str,
    graph: &ResolvedUiGraph,
    output: &str,
    block_id: &str,
) -> Result<String, (StatusCode, Json<ErrorResponse>)> {
    graph
        .flow_outgoing
        .get(node_id)
        .and_then(|edges| {
            edges
                .iter()
                .find(|edge| edge.source_output == output)
                .map(|edge| edge.target.clone())
        })
        .ok_or_else(|| {
            bad_request(&format!(
                "{block_id} requires a '{output}' outgoing flow edge"
            ))
        })
}

fn optional_flow_target(node_id: &str, graph: &ResolvedUiGraph, output: &str) -> Option<String> {
    graph.flow_outgoing.get(node_id).and_then(|edges| {
        edges
            .iter()
            .find(|edge| edge.source_output == output)
            .map(|edge| edge.target.clone())
    })
}

pub(super) fn flow_action_statement(
    block: &UiBlockInstance,
) -> Result<FlowActionStatement, (StatusCode, Json<ErrorResponse>)> {
    match block.block_id.as_str() {
        "set_request_header" => {
            let name = block_value(block, "name", "x-added");
            let value = block_value(block, "value", "1");
            Ok(FlowActionStatement {
                rustscript: format!(
                    "upstream_request::set_header({}, {});",
                    rust_string(name),
                    render_expr_rss(value)
                ),
                javascript: format!(
                    "upstream_request.set_header({}, {});",
                    js_string(name),
                    render_expr_js(value)
                ),
                lua: format!(
                    "upstream_request.set_header({}, {})",
                    lua_string(name),
                    render_expr_lua(value)
                ),
                scheme: format!(
                    "(upstream_request:set_header {} {})",
                    scheme_string(name),
                    render_expr_scheme(value)
                ),
            })
        }
        "add_request_header" => {
            let name = block_value(block, "name", "x-added");
            let value = block_value(block, "value", "1");
            Ok(FlowActionStatement {
                rustscript: format!(
                    "upstream_request::add_header({}, {});",
                    rust_string(name),
                    render_expr_rss(value)
                ),
                javascript: format!(
                    "upstream_request.add_header({}, {});",
                    js_string(name),
                    render_expr_js(value)
                ),
                lua: format!(
                    "upstream_request.add_header({}, {})",
                    lua_string(name),
                    render_expr_lua(value)
                ),
                scheme: format!(
                    "(upstream_request:add_header {} {})",
                    scheme_string(name),
                    render_expr_scheme(value)
                ),
            })
        }
        "clear_request_header" => {
            let name = block_value(block, "name", "x-remove");
            Ok(FlowActionStatement {
                rustscript: format!("upstream_request::clear_header({});", rust_string(name)),
                javascript: format!("upstream_request.clear_header({});", js_string(name)),
                lua: format!("upstream_request.clear_header({})", lua_string(name)),
                scheme: format!("(upstream_request:clear_header {})", scheme_string(name)),
            })
        }
        "set_request_method" => {
            let method = block_value(block, "method", "GET");
            Ok(FlowActionStatement {
                rustscript: format!("upstream_request::set_method({});", render_expr_rss(method)),
                javascript: format!("upstream_request.set_method({});", render_expr_js(method)),
                lua: format!("upstream_request.set_method({})", render_expr_lua(method)),
                scheme: format!(
                    "(upstream_request:set_method {})",
                    render_expr_scheme(method)
                ),
            })
        }
        "set_request_path" => {
            let path = block_value(block, "path", "/");
            Ok(FlowActionStatement {
                rustscript: format!("upstream_request::set_path({});", render_expr_rss(path)),
                javascript: format!("upstream_request.set_path({});", render_expr_js(path)),
                lua: format!("upstream_request.set_path({})", render_expr_lua(path)),
                scheme: format!("(upstream_request:set_path {})", render_expr_scheme(path)),
            })
        }
        "set_request_query" => {
            let query = block_value(block, "query", "x=1");
            Ok(FlowActionStatement {
                rustscript: format!("upstream_request::set_query({});", render_expr_rss(query)),
                javascript: format!("upstream_request.set_query({});", render_expr_js(query)),
                lua: format!("upstream_request.set_query({})", render_expr_lua(query)),
                scheme: format!("(upstream_request:set_query {})", render_expr_scheme(query)),
            })
        }
        "set_request_query_arg" => {
            let name = block_value(block, "name", "id");
            let value = block_value(block, "value", "1");
            Ok(FlowActionStatement {
                rustscript: format!(
                    "upstream_request::set_query_arg({}, {});",
                    rust_string(name),
                    render_expr_rss(value)
                ),
                javascript: format!(
                    "upstream_request.set_query_arg({}, {});",
                    js_string(name),
                    render_expr_js(value)
                ),
                lua: format!(
                    "upstream_request.set_query_arg({}, {})",
                    lua_string(name),
                    render_expr_lua(value)
                ),
                scheme: format!(
                    "(upstream_request:set_query_arg {} {})",
                    scheme_string(name),
                    render_expr_scheme(value)
                ),
            })
        }
        "set_request_body" => {
            let value = block_value(block, "value", "payload");
            Ok(FlowActionStatement {
                rustscript: format!("upstream_request::set_body({});", render_expr_rss(value)),
                javascript: format!("upstream_request.set_body({});", render_expr_js(value)),
                lua: format!("upstream_request.set_body({})", render_expr_lua(value)),
                scheme: format!("(upstream_request:set_body {})", render_expr_scheme(value)),
            })
        }
        "set_header" => {
            let name = block_value(block, "name", "x-vm");
            let value = block_value(block, "value", "ok");
            Ok(FlowActionStatement {
                rustscript: format!(
                    "vm::http::response::set_header({}, {});",
                    rust_string(name),
                    render_expr_rss(value)
                ),
                javascript: format!(
                    "vm.http.response.set_header({}, {});",
                    js_string(name),
                    render_expr_js(value)
                ),
                lua: format!(
                    "vm.http.response.set_header({}, {})",
                    lua_string(name),
                    render_expr_lua(value)
                ),
                scheme: format!(
                    "(vm.http.response.set_header {} {})",
                    scheme_string(name),
                    render_expr_scheme(value)
                ),
            })
        }
        "add_response_header" => {
            let name = block_value(block, "name", "set-cookie");
            let value = block_value(block, "value", "a=1");
            Ok(FlowActionStatement {
                rustscript: format!(
                    "vm::http::response::add_header({}, {});",
                    rust_string(name),
                    render_expr_rss(value)
                ),
                javascript: format!(
                    "vm.http.response.add_header({}, {});",
                    js_string(name),
                    render_expr_js(value)
                ),
                lua: format!(
                    "vm.http.response.add_header({}, {})",
                    lua_string(name),
                    render_expr_lua(value)
                ),
                scheme: format!(
                    "(vm.http.response.add_header {} {})",
                    scheme_string(name),
                    render_expr_scheme(value)
                ),
            })
        }
        "clear_response_header" => {
            let name = block_value(block, "name", "x-remove");
            Ok(FlowActionStatement {
                rustscript: format!("vm::http::response::clear_header({});", rust_string(name)),
                javascript: format!("vm.http.response.clear_header({});", js_string(name)),
                lua: format!("vm.http.response.clear_header({})", lua_string(name)),
                scheme: format!("(vm.http.response.clear_header {})", scheme_string(name)),
            })
        }
        "set_response_content" => {
            let value = block_value(block, "value", "request allowed");
            Ok(FlowActionStatement {
                rustscript: format!("vm::http::response::set_body({});", render_expr_rss(value)),
                javascript: format!("vm.http.response.set_body({});", render_expr_js(value)),
                lua: format!("vm.http.response.set_body({})", render_expr_lua(value)),
                scheme: format!("(vm.http.response.set_body {})", render_expr_scheme(value)),
            })
        }
        "set_response_status" => {
            let status = sanitize_status_code(block.values.get("status"), "429");
            Ok(FlowActionStatement {
                rustscript: format!("vm::http::response::set_status({status});"),
                javascript: format!("vm.http.response.set_status({status});"),
                lua: format!("vm.http.response.set_status({status})"),
                scheme: format!("(vm.http.response.set_status {status})"),
            })
        }
        "set_upstream" => {
            let host = block_value(block, "host", "127.0.0.1");
            let port = render_number_expr(block_value(block, "port", "8088"), "8088");
            Ok(FlowActionStatement {
                rustscript: format!(
                    "upstream_request::set_target({}, {});",
                    render_expr_rss(host),
                    port
                ),
                javascript: format!(
                    "upstream_request.set_target({}, {});",
                    render_expr_js(host),
                    port
                ),
                lua: format!(
                    "upstream_request.set_target({}, {})",
                    render_expr_lua(host),
                    port
                ),
                scheme: format!(
                    "(upstream_request:set_target {} {})",
                    render_expr_scheme(host),
                    port
                ),
            })
        }
        "runtime_sleep" => {
            let millis = render_number_expr(block_value(block, "millis", "10"), "10");
            Ok(FlowActionStatement {
                rustscript: format!("runtime::sleep({millis});"),
                javascript: format!("vm.runtime.sleep({millis});"),
                lua: format!("vm.runtime.sleep({millis})"),
                scheme: format!("(vm.runtime.sleep {millis})"),
            })
        }
        other => {
            if let Some(result) = additional_flow_action_statement(block) {
                result
            } else {
                Err(bad_request(&format!(
                    "unsupported flow action block '{}'",
                    other
                )))
            }
        }
    }
}
