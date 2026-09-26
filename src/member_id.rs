//! **The durable member identity** (KD-MW-2, design-full-multi-writer
//! §5.1): a writer is `node_{16 hex}.m{8 hex}` — the writer-scope node
//! token plus its mount slot — everywhere a durable record or a wire word
//! names it: the claim set's member roster, the appender page, the death
//! ledger, the slot holder table, the custody and token planes.
//!
//! The node half is a node identity and never a mount uuid (a uuid is
//! minted per mount, so a rejoin would enroll a stranger); the slot half is
//! what keeps two co-located mounts' identities apart. The bare
//! `node_{16 hex}` form names a process with no mount slot (offline verbs,
//! tests) and is also what a SLOT-WILDCARD roster entry names — see
//! [`crate::membership::member_id_matches`].

use crate::error::Result;

/// This process's durable member id. Refuses loud exactly as
/// [`crate::writer_scope::resolve_node_identity`] does — an unstable
/// identity would enroll one host and admit another.
pub fn node_member_id() -> Result<String> {
    let node = crate::writer_scope::resolve_node_identity()?.token;
    Ok(node_member_id_of(node, crate::writer_scope::mount_slot()))
}

/// [`node_member_id`]'s form for a given `(node_token, mount_slot)`.
pub fn node_member_id_of(node: u64, slot: u32) -> String {
    match slot {
        0 => format!("node_{node:016x}"),
        slot => format!("node_{node:016x}.m{slot:08x}"),
    }
}

/// The inverse of [`node_member_id`]: `(node_token, mount_slot)` from a
/// writer member's id, `None` for any other member id (a reader's uuid).
pub fn parse_node_member_id(id: &str) -> Option<(u64, u32)> {
    let rest = id.strip_prefix("node_")?;
    let (node_hex, slot_hex) = match rest.split_once(".m") {
        Some((n, s)) => (n, Some(s)),
        None => (rest, None),
    };
    if node_hex.len() != 16 {
        return None;
    }
    let node = u64::from_str_radix(node_hex, 16).ok()?;
    let slot = match slot_hex {
        Some(s) if s.len() == 8 => u32::from_str_radix(s, 16).ok()?,
        Some(_) => return None,
        None => 0,
    };
    Some((node, slot))
}
