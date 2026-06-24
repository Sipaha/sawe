//! Tools owned by the editor_mcp crate (cross-cutting, not domain-specific).
pub mod capabilities;
pub mod operations;
pub mod subscribe;

/// Helper for destructive-tier tools that want a per-call user confirmation
/// gate independent of the registry-level tier check. Tools opt in by
/// reading `confirmed: true` from their input payload (or any sibling
/// surface) and refusing to act without it.
///
/// The check is intentionally schema-agnostic — tools can inline this logic
/// or use the helper depending on whether their `Input` type already
/// surfaces a typed `confirmed` field.
pub fn is_confirmed(payload: &serde_json::Value) -> bool {
    payload
        .get("confirmed")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}
