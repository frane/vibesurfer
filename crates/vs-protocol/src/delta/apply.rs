//! Apply a [`DeltaOp`] sequence to a [`Tree`].

use crate::error::{ParseError, Result};
use crate::tree::{Node, Ref, Tree};

use super::DeltaOp;

/// Apply `ops` to a clone of `prev` and return the resulting tree.
pub fn apply(prev: &Tree, ops: &[DeltaOp]) -> Result<Tree> {
    let mut tree = prev.clone();
    for op in ops {
        apply_one(&mut tree, op)?;
    }
    Ok(tree)
}

fn apply_one(tree: &mut Tree, op: &DeltaOp) -> Result<()> {
    match op {
        DeltaOp::Remove { r } => {
            let _ = take_subtree(tree, *r);
            Ok(())
        }
        DeltaOp::Add {
            r,
            parent,
            pos,
            role,
            label,
            ops,
            attrs,
        } => {
            if find(tree, *r).is_some() {
                return Err(ParseError::ApplyDuplicateRef(r.0));
            }
            let node = Node {
                r: *r,
                role: *role,
                label: label.clone(),
                ops: ops.clone(),
                attrs: attrs.clone(),
                children: Vec::new(),
            };
            insert_at(tree, *parent, *pos, node)?;
            Ok(())
        }
        DeltaOp::Update { r, attrs } => {
            let node = find_mut(tree, *r).ok_or(ParseError::ApplyMissingRef(r.0))?;
            node.attrs = attrs.clone();
            Ok(())
        }
        DeltaOp::Move { r, parent, pos } => {
            let node = take_subtree(tree, *r).ok_or(ParseError::ApplyMissingRef(r.0))?;
            insert_at(tree, *parent, *pos, node)
        }
        DeltaOp::Replace { r, subtree } => {
            if subtree.is_empty() || subtree[0].r != *r {
                return Err(ParseError::MalformedLine {
                    line: 0,
                    message: "Replace subtree root ref mismatch",
                });
            }
            let location = locate(tree, *r);
            take_subtree(tree, *r).ok_or(ParseError::ApplyMissingRef(r.0))?;
            let new_node = subtree[0].clone();
            match location {
                Some((parent, pos)) => insert_at(tree, parent, Some(pos), new_node),
                None => Ok(()),
            }
        }
    }
}

pub(super) fn find(tree: &Tree, r: Ref) -> Option<&Node> {
    fn walk(node: &Node, r: Ref) -> Option<&Node> {
        if node.r == r {
            return Some(node);
        }
        for child in &node.children {
            if let Some(found) = walk(child, r) {
                return Some(found);
            }
        }
        None
    }
    for root in &tree.roots {
        if let Some(found) = walk(root, r) {
            return Some(found);
        }
    }
    None
}

pub(super) fn find_mut(tree: &mut Tree, r: Ref) -> Option<&mut Node> {
    fn walk(node: &mut Node, r: Ref) -> Option<&mut Node> {
        if node.r == r {
            return Some(node);
        }
        for child in &mut node.children {
            if let Some(found) = walk(child, r) {
                return Some(found);
            }
        }
        None
    }
    for root in &mut tree.roots {
        if let Some(found) = walk(root, r) {
            return Some(found);
        }
    }
    None
}

/// Remove the subtree rooted at `r` from `tree` and return it. Returns
/// `None` if `r` is not present.
pub(super) fn take_subtree(tree: &mut Tree, r: Ref) -> Option<Node> {
    if let Some(idx) = tree.roots.iter().position(|n| n.r == r) {
        return Some(tree.roots.remove(idx));
    }
    for root in &mut tree.roots {
        if let Some(n) = take_from_subtree(root, r) {
            return Some(n);
        }
    }
    None
}

fn take_from_subtree(node: &mut Node, r: Ref) -> Option<Node> {
    if let Some(idx) = node.children.iter().position(|c| c.r == r) {
        return Some(node.children.remove(idx));
    }
    for child in &mut node.children {
        if let Some(n) = take_from_subtree(child, r) {
            return Some(n);
        }
    }
    None
}

/// Locate `r`'s parent and position. `parent` is `Ref::ROOT` for
/// top-level nodes.
fn locate(tree: &Tree, r: Ref) -> Option<(Ref, u32)> {
    if let Some(idx) = tree.roots.iter().position(|n| n.r == r) {
        return Some((Ref::ROOT, u32::try_from(idx).unwrap_or(u32::MAX)));
    }
    for root in &tree.roots {
        if let Some(loc) = locate_in_subtree(root, r) {
            return Some(loc);
        }
    }
    None
}

fn locate_in_subtree(node: &Node, r: Ref) -> Option<(Ref, u32)> {
    for (i, child) in node.children.iter().enumerate() {
        if child.r == r {
            return Some((node.r, u32::try_from(i).unwrap_or(u32::MAX)));
        }
        if let Some(loc) = locate_in_subtree(child, r) {
            return Some(loc);
        }
    }
    None
}

fn insert_at(tree: &mut Tree, parent: Ref, pos: Option<u32>, node: Node) -> Result<()> {
    if parent.is_root() {
        let p = pos.unwrap_or(u32::MAX) as usize;
        let p = p.min(tree.roots.len());
        tree.roots.insert(p, node);
        return Ok(());
    }
    let parent_node = find_mut(tree, parent).ok_or(ParseError::ApplyMissingParent(parent.0))?;
    let p = pos.unwrap_or(u32::MAX) as usize;
    let p = p.min(parent_node.children.len());
    parent_node.children.insert(p, node);
    Ok(())
}
