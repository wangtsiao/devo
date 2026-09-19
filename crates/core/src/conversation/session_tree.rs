//! In-session transcript tree: leaf pointer, parent edges, path-from-leaf.
//!
//! Builds Prime-shaped nested nodes for `session/tree/read` and selects the
//! effective model-context path as ancestry from the current leaf to root.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use devo_protocol::native::ids::ItemId;
use devo_protocol::native::item::{Item, ItemEnvelope, UserInput};
use devo_protocol::native::rpc_session::SessionTreeNode;
use serde_json::{Value as JsonValue, json};

use super::history::CanonicalHistory;

/// Whether an item appears in the InteractiveMode session tree.
pub fn is_tree_visible_item(item: &Item) -> bool {
    matches!(
        item,
        Item::UserMessage { .. }
            | Item::AssistantMessage { .. }
            | Item::Reasoning { .. }
            | Item::BranchSummary { .. }
            | Item::ToolCall { .. }
            | Item::ToolResult { .. }
            | Item::CommandExecution { .. }
            | Item::ContextCompaction { .. }
    )
}

/// Resolves parent pointers: TreeEdge and item `parent_id` win; otherwise
/// infer a linear chain by ascending `seq` among tree-visible items.
pub fn resolve_parent_map(history: &CanonicalHistory) -> HashMap<ItemId, Option<ItemId>> {
    let mut parents: HashMap<ItemId, Option<ItemId>> = history.tree_edges.clone();
    for envelope in &history.items {
        if let Some(parent_id) = envelope.parent_id {
            parents.insert(envelope.id, Some(parent_id));
        }
    }
    let mut visible: Vec<&ItemEnvelope> = history
        .items
        .iter()
        .filter(|envelope| is_tree_visible_item(&envelope.item))
        .collect();
    visible.sort_by_key(|envelope| envelope.seq);

    let mut prev: Option<ItemId> = None;
    for envelope in visible {
        // Only infer when no durable edge exists (including explicit root None).
        parents.entry(envelope.id).or_insert(prev);
        prev = Some(envelope.id);
    }
    parents
}

/// Effective leaf: durable SessionLeaf when set and present; else last
/// tree-visible item by seq.
pub fn resolve_leaf_id(
    history: &CanonicalHistory,
    parents: &HashMap<ItemId, Option<ItemId>>,
) -> Option<ItemId> {
    if let Some(leaf) = history.leaf_id
        && (parents.contains_key(&leaf) || history.items.iter().any(|item| item.id == leaf)) {
            return Some(leaf);
        }
    history
        .items
        .iter()
        .filter(|envelope| is_tree_visible_item(&envelope.item))
        .max_by_key(|envelope| envelope.seq)
        .map(|envelope| envelope.id)
}

/// Walk parent links from `leaf` to root (root first).
pub fn path_root_to_leaf(
    leaf: Option<&ItemId>,
    parents: &HashMap<ItemId, Option<ItemId>>,
) -> Vec<ItemId> {
    let Some(start) = leaf else {
        return Vec::new();
    };
    let mut chain = Vec::new();
    let mut current = Some(*start);
    let mut seen = HashSet::new();
    while let Some(id) = current {
        if !seen.insert(id) {
            break;
        }
        chain.push(id);
        current = parents.get(&id).cloned().flatten();
    }
    chain.reverse();
    chain
}

/// Item ids on the active root→leaf path (for model context filtering).
pub fn active_path_item_ids(history: &CanonicalHistory) -> HashSet<ItemId> {
    let parents = resolve_parent_map(history);
    let leaf = resolve_leaf_id(history, &parents);
    path_root_to_leaf(leaf.as_ref(), &parents)
        .into_iter()
        .collect()
}

/// Build nested Prime-shaped tree + resolved leaf id.
pub fn build_session_tree(history: &CanonicalHistory) -> (Vec<SessionTreeNode>, Option<ItemId>) {
    let parents = resolve_parent_map(history);
    let leaf_id = resolve_leaf_id(history, &parents);

    let mut by_id: HashMap<ItemId, SessionTreeNode> = HashMap::new();
    let mut order: Vec<ItemId> = Vec::new();
    for envelope in history
        .items
        .iter()
        .filter(|e| is_tree_visible_item(&e.item))
    {
        let parent_id = parents.get(&envelope.id).cloned().flatten();
        let node = SessionTreeNode {
            entry: project_tree_entry(envelope, parent_id.as_ref()),
            label: tree_label(envelope),
            label_timestamp: None,
            children: Vec::new(),
        };
        order.push(envelope.id);
        by_id.insert(envelope.id, node);
    }

    // Attach children (stable by original seq order).
    let mut roots: Vec<ItemId> = Vec::new();
    let mut child_ids: HashMap<ItemId, Vec<ItemId>> = HashMap::new();
    for id in &order {
        match parents.get(id).cloned().flatten() {
            Some(parent) if by_id.contains_key(&parent) => {
                child_ids.entry(parent).or_default().push(*id);
            }
            _ => roots.push(*id),
        }
    }

    fn assemble(
        id: &ItemId,
        by_id: &mut HashMap<ItemId, SessionTreeNode>,
        child_ids: &HashMap<ItemId, Vec<ItemId>>,
    ) -> Option<SessionTreeNode> {
        let mut node = by_id.remove(id)?;
        if let Some(children) = child_ids.get(id) {
            for child in children {
                if let Some(child_node) = assemble(child, by_id, child_ids) {
                    node.children.push(child_node);
                }
            }
        }
        Some(node)
    }

    let tree = roots
        .iter()
        .filter_map(|id| assemble(id, &mut by_id, &child_ids))
        .collect();
    (tree, leaf_id)
}

fn tree_label(envelope: &ItemEnvelope) -> Option<String> {
    match &envelope.item {
        Item::UserMessage { content, .. } => Some(user_text(content).chars().take(80).collect()),
        Item::AssistantMessage { text, .. } => Some(text.chars().take(80).collect()),
        Item::Reasoning { text, .. } => Some(format!(
            "thinking: {}",
            text.chars().take(60).collect::<String>()
        )),
        Item::BranchSummary { summary, .. } => Some(summary.chars().take(80).collect()),
        Item::ToolCall { tool_name, .. } => Some(format!("tool: {tool_name}")),
        Item::ToolResult { call_id, .. } => Some(format!("result: {call_id}")),
        Item::CommandExecution { command, .. } => Some(format!(
            "$ {}",
            command.chars().take(70).collect::<String>()
        )),
        Item::ContextCompaction { summary, .. } => {
            Some(summary.clone().unwrap_or_else(|| "compaction".to_string()))
        }
        _ => None,
    }
}

fn user_text(content: &[UserInput]) -> String {
    content
        .iter()
        .filter_map(|part| match part {
            UserInput::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn project_tree_entry(envelope: &ItemEnvelope, parent_id: Option<&ItemId>) -> JsonValue {
    let timestamp = format_timestamp(envelope.created_at);
    let parent = parent_id
        .map(|id| json!(id.as_str()))
        .unwrap_or(JsonValue::Null);
    match &envelope.item {
        Item::UserMessage { content, .. } => json!({
            "type": "message",
            "id": envelope.id.as_str(),
            "parentId": parent,
            "timestamp": timestamp,
            "message": {
                "role": "user",
                "content": content.iter().filter_map(|part| match part {
                    UserInput::Text { text } => Some(json!({ "type": "text", "text": text })),
                    _ => None,
                }).collect::<Vec<_>>(),
            }
        }),
        Item::AssistantMessage { text, .. } => json!({
            "type": "message",
            "id": envelope.id.as_str(),
            "parentId": parent,
            "timestamp": timestamp,
            "message": {
                "role": "assistant",
                "content": [{ "type": "text", "text": text }],
            }
        }),
        Item::Reasoning { text, .. } => json!({
            "type": "message",
            "id": envelope.id.as_str(),
            "parentId": parent,
            "timestamp": timestamp,
            "message": {
                "role": "assistant",
                "content": [{ "type": "thinking", "thinking": text }],
            }
        }),
        Item::BranchSummary { summary, details } => json!({
            "type": "branch_summary",
            "id": envelope.id.as_str(),
            "parentId": parent,
            "timestamp": timestamp,
            "summary": summary,
            "details": details,
        }),
        Item::ToolCall {
            call_id, tool_name, ..
        } => json!({
            "type": "message",
            "id": envelope.id.as_str(),
            "parentId": parent,
            "timestamp": timestamp,
            "message": {
                "role": "assistant",
                "content": [{
                    "type": "toolCall",
                    "id": call_id,
                    "name": tool_name,
                    "arguments": {},
                }],
            }
        }),
        Item::ToolResult { call_id, .. } => json!({
            "type": "message",
            "id": envelope.id.as_str(),
            "parentId": parent,
            "timestamp": timestamp,
            "message": {
                "role": "toolResult",
                "toolCallId": call_id,
                "toolName": "tool",
                "content": [{ "type": "text", "text": "" }],
            }
        }),
        Item::CommandExecution { command, .. } => json!({
            "type": "message",
            "id": envelope.id.as_str(),
            "parentId": parent,
            "timestamp": timestamp,
            "message": {
                "role": "assistant",
                "content": [{
                    "type": "bash",
                    "command": command,
                }],
            }
        }),
        Item::ContextCompaction { summary, .. } => json!({
            "type": "compaction",
            "id": envelope.id.as_str(),
            "parentId": parent,
            "timestamp": timestamp,
            "summary": summary,
        }),
        _ => json!({
            "type": "custom",
            "id": envelope.id.as_str(),
            "parentId": parent,
            "timestamp": timestamp,
            "customType": "item",
        }),
    }
}

fn format_timestamp(ts: DateTime<Utc>) -> String {
    ts.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}
