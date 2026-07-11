use super::{
    EngineId, EngineNames, KindId, MailId, MailIdJson, MailNodeJson, MailNodeWire, MailboxId, McpError, Tag, Uuid,
    descriptors, kind_id_from_parts, tagged_id,
};
use std::collections::{HashMap, HashSet};

/// Parse a UUID-string `engine_id` (from `list_engines` /
/// `spawn_substrate`) into an `EngineId`.
pub(super) fn parse_engine_id(s: &str) -> Result<EngineId, McpError> {
    Uuid::parse_str(s)
        .map(EngineId)
        .map_err(|e| McpError::invalid_params(format!("engine_id is not a valid UUID: {e}"), None))
}

/// Parse a tagged mailbox-id string (`mbx-…`, ADR-0064) into a
/// `MailboxId`.
pub(super) fn parse_mailbox_id(s: &str) -> Result<MailboxId, McpError> {
    tagged_id::decode_with_tag(s, Tag::Mailbox)
        .map(MailboxId)
        .map_err(|e| McpError::invalid_params(format!("mailbox_id: {e}"), None))
}

/// Parse a kind-id string for the `actor_cost` filter: a tagged
/// `knd-…` id (ADR-0064) or a raw decimal `u64`. The raw form is
/// accepted because a cost row's id round-trips back through this
/// filter and a caller may paste a non-tagged synthetic id.
pub(super) fn parse_kind_id(s: &str) -> Result<KindId, McpError> {
    if let Ok(id) = tagged_id::decode_with_tag(s, Tag::Kind) {
        return Ok(KindId(id));
    }
    s.parse::<u64>().map(KindId).map_err(|_| {
        McpError::invalid_params(format!("kind_id: not a tagged `knd-…` id or a decimal u64: {s:?}"), None)
    })
}

/// Resolve a `handled_kind` filter token (ADR-0116 `list_components`) to a
/// [`KindId`]: a tagged `knd-…` id or a decimal `u64` resolves directly;
/// otherwise the token is a kind name resolved against the static substrate
/// vocabulary (`describe_kinds`'s source). An unknown name is an
/// invalid-params error.
pub(super) fn resolve_handled_kind(s: &str) -> Result<KindId, McpError> {
    if let Ok(id) = tagged_id::decode_with_tag(s, Tag::Kind) {
        return Ok(KindId(id));
    }
    if let Ok(id) = s.parse::<u64>() {
        return Ok(KindId(id));
    }
    descriptors::all()
        .into_iter()
        .find(|d| d.name == s)
        .map(|d| KindId(kind_id_from_parts(&d.name, &d.schema)))
        .ok_or_else(|| {
            McpError::invalid_params(
                format!("handled_kind: not a tagged `knd-…` id, a decimal u64, or a known kind name: {s:?}"),
                None,
            )
        })
}

/// Best-effort resolve a [`KindId`] to its name from the static kind
/// inventory the MCP harness ships with (`describe_kinds`'s source).
/// Component-defined kinds aren't in the inventory and return `None`.
/// Cold path — recomputes the inventory's ids on each call; the cost
/// dump is a diagnostic, not a hot loop.
pub(super) fn static_kind_name(id: KindId) -> Option<String> {
    descriptors::all().into_iter().find(|d| kind_id_from_parts(&d.name, &d.schema) == id.0).map(|d| d.name)
}

/// Render a raw `u64` mailbox / kind / thread id to its display string
/// (ADR-0088 §8): the engine's real name when `names` resolves it, else
/// the ADR-0064 tagged-id string (`mbx-…` / `knd-…` / `thr-…`), else a
/// hex literal if the tag bits are unencodable. `names == None` (no
/// reverse map for the engine) renders the tag directly — the unchanged
/// pre-inventory output.
pub(super) fn render_id(id: u64, names: Option<&EngineNames>) -> String {
    names.map_or_else(|| tagged_id::encode(id).unwrap_or_else(|| format!("{id:#x}")), |names| names.render(id))
}

/// Reverse-render a [`MailboxId`] through the engine's name map (or the
/// hex tag on a miss / no map). Chassis-minted ids always carry tag bits,
/// so the hex fallback never reaches the `{:#x}` arm in practice.
pub(super) fn mailbox_id_to_tagged(id: MailboxId, names: Option<&EngineNames>) -> String {
    render_id(id.0, names)
}

pub(super) fn kind_id_to_tagged(id: KindId, names: Option<&EngineNames>) -> String {
    render_id(id.0, names)
}

pub(super) fn mail_id_to_json(id: MailId, names: Option<&EngineNames>) -> MailIdJson {
    MailIdJson { sender: mailbox_id_to_tagged(id.sender, names), correlation_id: id.correlation_id }
}

pub(super) fn mail_node_to_json(node: MailNodeWire, names: Option<&EngineNames>) -> MailNodeJson {
    MailNodeJson {
        mail_id: mail_id_to_json(node.mail_id, names),
        parent: node.parent.map(|p| mail_id_to_json(p, names)),
        sender: mailbox_id_to_tagged(node.sender, names),
        recipient: mailbox_id_to_tagged(node.recipient, names),
        kind: kind_id_to_tagged(node.kind, names),
        t_construct_start: node.t_construct_start.0,
        t_sent: node.t_sent.0,
        t_received: node.t_received.map(|n| n.0),
        t_finished: node.t_finished.map(|n| n.0),
        thread_name: node.thread_name,
    }
}

/// Render resolved trace nodes as a compact causal tree. Adjacency reuses the
/// existing named [`MailIdJson`] identity; indices distinguish malformed
/// duplicate ids and give the visited set a structural key. Roots and siblings
/// preserve input order. A final input-order pass emits orphaned or cyclic
/// nodes exactly once, so malformed data cannot recurse or loop forever.
pub(super) fn render_compact_tree(nodes: &[MailNodeJson]) -> Vec<String> {
    let mut children: HashMap<&MailIdJson, Vec<usize>> = HashMap::new();
    let mut roots = Vec::new();
    for (index, node) in nodes.iter().enumerate() {
        if let Some(parent) = node.parent.as_ref() {
            children.entry(parent).or_default().push(index);
        } else {
            roots.push(index);
        }
    }

    let mut lines = Vec::with_capacity(nodes.len());
    let mut visited = HashSet::with_capacity(nodes.len());
    for seed in roots.into_iter().chain(0..nodes.len()) {
        if visited.contains(&seed) {
            continue;
        }
        let mut stack = vec![(seed, 0usize)];
        while let Some((index, depth)) = stack.pop() {
            if !visited.insert(index) {
                continue;
            }
            let node = &nodes[index];
            let timing = node.t_finished.map_or_else(
                || "in-flight".to_owned(),
                |finished| {
                    let started = node.t_received.unwrap_or(node.t_sent);
                    format!("+{}µs", finished.saturating_sub(started) / 1_000)
                },
            );
            lines.push(format!(
                "{}{sender} → {recipient}  {kind}  {timing}",
                "  ".repeat(depth),
                sender = node.sender,
                recipient = node.recipient,
                kind = node.kind,
            ));

            if let Some(node_children) = children.get(&node.mail_id) {
                for child in node_children.iter().rev() {
                    if !visited.contains(child) {
                        stack.push((*child, depth.saturating_add(1)));
                    }
                }
            }
        }
    }
    lines
}

/// The mailbox / kind / thread ids in one `MailNodeWire` that reverse
/// through the inventory (ADR-0088 §8): the two mailbox endpoints, the
/// kind, and both `MailId` senders. `correlation_id` is a `Uuid`, not a
/// tagged id, so it's excluded. Thread ids ride in `thread_name` already
/// resolved substrate-side, so they aren't re-resolved here.
pub(super) fn node_reversible_ids(node: &MailNodeWire) -> Vec<u64> {
    let mut ids = vec![node.sender.0, node.recipient.0, node.kind.0, node.mail_id.sender.0];
    if let Some(parent) = &node.parent {
        ids.push(parent.sender.0);
    }
    ids
}
