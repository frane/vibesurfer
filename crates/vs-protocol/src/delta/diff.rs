//! Compute a [`DeltaOp`] sequence that transforms one [`Tree`] into
//! another.

use crate::tree::{Node, Ref, Tree};

use super::DeltaOp;

/// Compute a delta that transforms `a` into `b`. `apply(a, diff(a, b))`
/// equals `b` (modulo position resolution; see the property tests).
///
/// The diff is structurally faithful but not optimized: it never emits
/// [`DeltaOp::Replace`] for whole-subtree collapse. The 40%-edit-
/// distance threshold lives at the daemon level (M4) where ref churn
/// is observable across many snapshots.
#[must_use]
pub fn diff(a: &Tree, b: &Tree) -> Vec<DeltaOp> {
    let mut ops = Vec::new();
    diff_children(&a.roots, &b.roots, Ref::ROOT, &mut ops);
    ops
}

fn diff_children(a: &[Node], b: &[Node], parent: Ref, ops: &mut Vec<DeltaOp>) {
    use std::collections::HashSet;
    let a_refs: HashSet<Ref> = a.iter().map(|n| n.r).collect();
    let b_refs: HashSet<Ref> = b.iter().map(|n| n.r).collect();

    for node in a {
        if !b_refs.contains(&node.r) {
            ops.push(DeltaOp::Remove { r: node.r });
        }
    }

    for (i, node) in b.iter().enumerate() {
        if !a_refs.contains(&node.r) {
            ops.push(DeltaOp::Add {
                r: node.r,
                parent,
                pos: Some(u32::try_from(i).unwrap_or(u32::MAX)),
                role: node.role,
                label: node.label.clone(),
                ops: node.ops.clone(),
                attrs: node.attrs.clone(),
            });
            diff_children(&[], &node.children, node.r, ops);
        }
    }

    for b_node in b {
        if let Some(a_node) = a.iter().find(|n| n.r == b_node.r) {
            diff_node(a_node, b_node, ops);
        }
    }

    for (i, b_node) in b.iter().enumerate() {
        if a_refs.contains(&b_node.r) {
            let a_kept_pos = a
                .iter()
                .filter(|n| b_refs.contains(&n.r))
                .position(|n| n.r == b_node.r)
                .expect("kept ref present in a");
            let b_kept_pos = b
                .iter()
                .filter(|n| a_refs.contains(&n.r))
                .position(|n| n.r == b_node.r)
                .expect("kept ref present in b");
            if a_kept_pos != b_kept_pos {
                ops.push(DeltaOp::Move {
                    r: b_node.r,
                    parent,
                    pos: Some(u32::try_from(i).unwrap_or(u32::MAX)),
                });
            }
        }
    }
}

fn diff_node(a: &Node, b: &Node, ops: &mut Vec<DeltaOp>) {
    if a.role != b.role || a.label != b.label || a.ops != b.ops {
        ops.push(DeltaOp::Replace {
            r: a.r,
            subtree: vec![b.clone()],
        });
        return;
    }
    if a.attrs != b.attrs {
        ops.push(DeltaOp::Update {
            r: a.r,
            attrs: b.attrs.clone(),
        });
    }
    diff_children(&a.children, &b.children, a.r, ops);
}
