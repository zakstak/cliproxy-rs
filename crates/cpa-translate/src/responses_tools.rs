//! Responses tool declarations shared by the non-Claude Responses translators
//! (internal/util/responses_tools.go). The Claude Responses translator keeps its own
//! variant in `claude_responses` because Go's Claude package collects built-in tools too.

use std::collections::{HashMap, HashSet};

use cpa_common::json::{self as gj, Res};
use sha2::{Digest, Sha256};

use crate::claude_responses::{qualify_namespace_name, tool_name};
use crate::common::{sanitize_function_name, trim_space};

/// util.ResponsesToolSearchFunctionName: the function exposed for a client tool_search.
pub(crate) const TOOL_SEARCH_NAME: &[u8] = b"tool_search";
/// util.ResponsesDiscoveredToolPriority: tools loaded by a tool_search_output.
const DISCOVERED_PRIORITY: u8 = 2;

/// util.IsResponsesClientToolSearchItem: a tool_search_call or tool_search_output of a
/// client-executed search (hosted search sets `execution` to "server").
pub(crate) fn is_client_tool_search_item(item: &Res<'_>) -> bool {
    matches!(trim_space(&item.get("execution").bytes()), b"" | b"client")
}

/// util.ResponsesToolDescriptor.
pub(crate) struct Descriptor<'a> {
    pub name: Vec<u8>,
    pub local_name: Vec<u8>,
    pub namespace: Vec<u8>,
    pub tool: Res<'a>,
    priority: u8,
    direct: bool,
    pub order: usize,
    /// A client-executed tool_search declaration.
    client_search: bool,
}

/// util.ResponsesToolDescription.
pub(crate) fn tool_description(tool: &Res<'_>) -> Vec<u8> {
    let description = tool.get("description").bytes();
    if !description.is_empty() {
        return description.into_owned();
    }
    tool.get("function.description").bytes().into_owned()
}

/// util.ResponsesToolParameters.
pub(crate) fn tool_parameters<'a>(tool: &Res<'a>) -> Option<Res<'a>> {
    [
        "parameters",
        "parametersJsonSchema",
        "input_schema",
        "function.parameters",
        "function.parametersJsonSchema",
    ]
    .iter()
    .map(|path| tool.get(*path))
    .find(Res::exists)
}

/// util.CollectResponsesToolDescriptors: function and custom tools from `tools` and from
/// `additional_tools` input items, with namespace children qualified.
pub(crate) fn descriptors<'a>(root: &Res<'a>) -> Vec<Descriptor<'a>> {
    descriptors_with(root, false)
}

/// collectResponsesToolDescriptors: with `client_search`, also the client tool_search
/// declaration and the tools its `tool_search_output` items loaded; when a client search is
/// declared, `defer_loading` tools stay out until a search loads them.
fn descriptors_with<'a>(root: &Res<'a>, client_search: bool) -> Vec<Descriptor<'a>> {
    let mut sources: Vec<(Res<'a>, u8)> = vec![];
    let tools = root.get("tools");
    if tools.is_array() {
        sources.push((tools, 0));
    }
    let input = root.get("input");
    if input.is_array() {
        input.each(|_, item| {
            if item.get("type").bytes().as_ref() == b"additional_tools" {
                let tools = item.get("tools");
                if tools.is_array() {
                    sources.push((tools, 1));
                }
            }
            true
        });
        if client_search {
            input.each(|_, item| {
                if item.get("type").bytes().as_ref() == b"tool_search_output" && is_client_tool_search_item(&item) {
                    let tools = item.get("tools");
                    if tools.is_array() {
                        sources.push((tools, DISCOVERED_PRIORITY));
                    }
                }
                true
            });
        }
    }
    let mut out: Vec<Descriptor<'a>> = vec![];
    let mut add = |tool: &Res<'a>,
                   name: Vec<u8>,
                   local: Vec<u8>,
                   namespace: Vec<u8>,
                   priority: u8,
                   direct: bool,
                   search: bool| {
        if name.is_empty() {
            return;
        }
        let order = out.len();
        out.push(Descriptor {
            name,
            local_name: local,
            namespace,
            tool: tool.clone(),
            priority,
            direct,
            order,
            client_search: search,
        });
    };
    for (tools, priority) in sources {
        tools.each(|_, tool| {
            match trim_space(&tool.get("type").bytes()) {
                b"" | b"function" | b"custom" => {
                    let name = tool_name(&tool);
                    add(&tool, name.clone(), name, vec![], priority, true, false);
                }
                b"namespace" => {
                    let namespace = trim_space(&tool.get("name").bytes()).to_vec();
                    let mut children = tool.get("tools");
                    if !children.is_array() {
                        children = tool.get("children");
                    }
                    if children.is_array() {
                        children.each(|_, child| {
                            let child_name = tool_name(&child);
                            if child_name.is_empty() {
                                return true;
                            }
                            let qualified = qualify_namespace_name(&namespace, &child_name);
                            if let b"" | b"function" | b"custom" = trim_space(&child.get("type").bytes()) {
                                add(&child, qualified, child_name, namespace.clone(), priority, false, false);
                            }
                            true
                        });
                    }
                }
                b"tool_search"
                    if client_search
                        && priority < DISCOVERED_PRIORITY
                        && trim_space(&tool.get("execution").bytes()) == b"client" =>
                {
                    let name = TOOL_SEARCH_NAME.to_vec();
                    add(&tool, name.clone(), name, vec![], priority, true, true);
                }
                _ => {}
            }
            true
        });
    }
    if !client_search || !out.iter().any(|d| d.client_search) {
        return out;
    }
    let mut kept: Vec<Descriptor<'a>> = out
        .into_iter()
        .filter(|d| d.client_search || d.priority == DISCOVERED_PRIORITY || !d.tool.get("defer_loading").bool())
        .collect();
    for (order, d) in kept.iter_mut().enumerate() {
        d.order = order;
    }
    kept
}

/// util.CollectResponsesToolWinners: per name, top-level beats additional_tools, direct
/// beats namespaced, then first wins. Values are descriptor orders.
pub(crate) fn winners(descriptors: &[Descriptor<'_>]) -> HashMap<Vec<u8>, usize> {
    let mut winners: HashMap<Vec<u8>, usize> = HashMap::new();
    for d in descriptors {
        let better = winners.get(&d.name).is_none_or(|&w| {
            let c = &descriptors[w];
            if d.priority != c.priority {
                d.priority < c.priority
            } else if d.direct != c.direct {
                d.direct
            } else {
                d.order < c.order
            }
        });
        if better {
            winners.insert(d.name.clone(), d.order);
        }
    }
    winners
}

/// sanitizeResponsesToolNames: Gemini-safe names; names whose sanitized forms collide get
/// a sha256-derived suffix, assigned in sorted name order.
fn sanitize_names(names: &[&[u8]]) -> HashMap<Vec<u8>, Vec<u8>> {
    let mut seen: HashSet<&[u8]> = HashSet::new();
    let mut base_counts: HashMap<Vec<u8>, usize> = HashMap::new();
    for &name in names {
        if name.is_empty() || !seen.insert(name) {
            continue;
        }
        *base_counts.entry(sanitize_function_name(name)).or_default() += 1;
    }
    let mut unique: Vec<&[u8]> = seen.into_iter().collect();
    unique.sort_unstable();
    let mut out = HashMap::new();
    let mut used: HashMap<Vec<u8>, Vec<u8>> = HashMap::new();
    for name in unique {
        let base = sanitize_function_name(name);
        let mapped = if base_counts[&base] > 1 || used.contains_key(&base) {
            disambiguate(&base, name, &used)
        } else {
            base
        };
        used.insert(mapped.clone(), name.to_vec());
        out.insert(name.to_vec(), mapped);
    }
    out
}

/// util.functionNamesFromRequest: declared function names of a request's `tools`
/// (nested `tools`, Gemini declarations, OpenAI `function.name`, then `name`).
fn function_names_from_request(raw: &[u8]) -> Vec<Vec<u8>> {
    /// Nested `tools` lists wait on a heap stack, in document order; lists more than
    /// [`gj::WALK_DEPTH_LIMIT`] levels below the request's `tools` are skipped.
    fn collect(tool: Res<'_>, names: &mut Vec<Vec<u8>>) {
        let mut pending = vec![(tool, 0usize)];
        while let Some((tool, depth)) = pending.pop() {
            let nested = tool.get("tools");
            if nested.is_array() {
                if depth < gj::WALK_DEPTH_LIMIT {
                    let mut tools = vec![];
                    nested.each(|_, t| {
                        tools.push((t, depth + 1));
                        true
                    });
                    pending.extend(tools.into_iter().rev());
                }
                continue;
            }
            let mut declared = false;
            for key in ["functionDeclarations", "function_declarations"] {
                let declarations = tool.get(key);
                if declarations.is_array() {
                    declared = true;
                    declarations.each(|_, d| {
                        let name = d.get("name").bytes();
                        if !name.is_empty() {
                            names.push(name.into_owned());
                        }
                        true
                    });
                }
            }
            if declared {
                continue;
            }
            for path in ["function.name", "name"] {
                let name = tool.get(path).bytes();
                if !name.is_empty() {
                    names.push(name.into_owned());
                    break;
                }
            }
        }
    }
    if raw.is_empty() || !gj::valid(raw) {
        return vec![];
    }
    let tools = gj::get(raw, "tools");
    let mut names = vec![];
    if tools.is_array() {
        tools.each(|_, tool| {
            collect(tool, &mut names);
            true
        });
    }
    names
}

/// util.SanitizedFunctionNameMap: declared name -> collision-free Gemini-safe name.
pub(crate) fn sanitized_function_name_map(raw: &[u8]) -> HashMap<Vec<u8>, Vec<u8>> {
    let names = function_names_from_request(raw);
    let names: Vec<&[u8]> = names.iter().map(Vec::as_slice).collect();
    sanitize_names(&names)
}

/// util.MapSanitizedFunctionName: the request-specific name, else the sanitized name.
pub(crate) fn map_sanitized_function_name(map: &HashMap<Vec<u8>, Vec<u8>>, name: &[u8]) -> Vec<u8> {
    match map.get(name) {
        Some(mapped) if !mapped.is_empty() => mapped.clone(),
        _ => sanitize_function_name(name),
    }
}

fn disambiguate(base: &[u8], original: &[u8], used: &HashMap<Vec<u8>, Vec<u8>>) -> Vec<u8> {
    for attempt in 0u64.. {
        let mut hasher = Sha256::new();
        hasher.update(original);
        hasher.update(format!("\x00{attempt}"));
        let digest = hasher.finalize();
        let suffix = format!("_{}", crate::common::hex(&digest[..6]));
        let prefix = &base[..base.len().min(64 - suffix.len())];
        let candidate = [prefix, suffix.as_bytes()].concat();
        if !used.contains_key(&candidate) {
            return candidate;
        }
    }
    unreachable!("an unused suffix always exists")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colliding_sanitized_names_get_go_hash_suffixes() {
        // "a b" and "a_b" both sanitize to "a_b" ("." is allowed and stays); Go suffixes
        // every colliding name with sha256("<name>\x00<attempt>")[:6] in hex.
        let names: [&[u8]; 3] = [b"a_b", b"a b", b"a.b"];
        let out = sanitize_names(&names);
        assert_eq!(out[&b"a.b".to_vec()], b"a.b");
        let expect = |name: &[u8]| {
            let digest = Sha256::digest([name, b"\x000"].concat());
            format!("a_b_{}", crate::common::hex(&digest[..6])).into_bytes()
        };
        assert_eq!(out[&b"a b".to_vec()], expect(b"a b"));
        assert_eq!(out[&b"a_b".to_vec()], expect(b"a_b"));
        assert_ne!(out[&b"a b".to_vec()], out[&b"a_b".to_vec()]);
    }
}
