//! Per-page in-memory state owned by the [`Daemon`](crate::Daemon).
//!
//! Each open page tracks its engine handle, URL, the most recent tree
//! we sent to the agent, and the most recent state token. The cache is
//! used by [`view`](crate::handlers) to compute deltas.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use vs_engine_webkit::PageHandle;
use vs_protocol::{DeltaOp, Ref, StateToken, Tree};

use crate::tokens;

/// In-memory page state.
#[derive(Debug)]
pub struct PageState {
    pub id: String,
    pub url: String,
    pub engine_handle: PageHandle,
    /// The last tree we emitted to the agent. `None` until the first
    /// `vs_view`.
    pub last_tree: Option<Tree>,
    /// Token corresponding to `last_tree`, or to the post-mutation
    /// state if a write just landed.
    pub last_token: Option<StateToken>,
    /// Whether the next `vs_view` must emit a fresh full tree (e.g.
    /// after `nav`, viewport change, or `auth_loaded`).
    pub force_full: bool,
    /// Refs ever seen on this page, for retire-tracking.
    pub seen_refs: HashSet<Ref>,
    /// Serializes `vs_act`'s token-check → engine-act → re-snapshot
    /// window. Without it, two concurrent acts carrying the same
    /// `before_token` would both pass the stale-token check before
    /// either mutation lands. Kept outside the session-map mutex so
    /// the (slow) engine call doesn't block unrelated pages.
    pub mutate_lock: Arc<Mutex<()>>,
}

impl PageState {
    #[must_use]
    pub fn new(id: String, url: String, engine_handle: PageHandle) -> Self {
        Self {
            id,
            url,
            engine_handle,
            last_tree: None,
            last_token: None,
            force_full: true,
            seen_refs: HashSet::new(),
            mutate_lock: Arc::new(Mutex::new(())),
        }
    }

    /// Mark the next view as a fresh-full re-baseline.
    pub fn invalidate_baseline(&mut self) {
        self.force_full = true;
    }

    /// Apply a fresh snapshot from the engine. Returns the new token
    /// and the [`ViewForm`] the agent should receive.
    pub fn apply_snapshot(&mut self, new_tree: Tree) -> (StateToken, ViewForm) {
        let token = tokens::compute(&new_tree, &self.url, &self.id);

        // Same token = no change. Send NoChange even on a force_full
        // marker — agents asking for a fresh full got the same one
        // when nothing happened.
        if !self.force_full && self.last_token.as_ref().is_some_and(|t| *t == token) {
            return (token, ViewForm::NoChange);
        }

        let form = match (&self.last_tree, self.force_full) {
            (None, _) | (Some(_), true) => ViewForm::Full(new_tree.clone()),
            (Some(prev), false) => {
                let ops = vs_protocol::diff(prev, &new_tree);
                ViewForm::Delta(ops)
            }
        };

        // Track refs seen on this page.
        for node in &new_tree {
            self.seen_refs.insert(node.r);
        }

        self.last_tree = Some(new_tree);
        self.last_token = Some(token);
        self.force_full = false;
        (token, form)
    }

    /// Find a node by ref in the most recent tree. Used by `vs_read`.
    #[must_use]
    pub fn find_node(&self, r: Ref) -> Option<&vs_protocol::Node> {
        self.last_tree
            .as_ref()
            .and_then(|t| t.iter().find(|n| n.r == r))
    }
}

/// What [`PageState::apply_snapshot`] produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ViewForm {
    /// First view on the page (or post-baseline-reset). Emit the whole
    /// tree.
    Full(Tree),
    /// Subsequent view. Emit the delta against the last-emitted tree.
    Delta(Vec<DeltaOp>),
    /// `state_token` unchanged from the last call. Body is empty.
    NoChange,
}

#[cfg(test)]
mod tests {
    use super::*;
    use vs_protocol::{Node, Role};

    fn sample_tree(label: &str) -> Tree {
        Tree::from_root(Node::leaf(Ref(1), Role::Doc, label))
    }

    fn make_state() -> PageState {
        PageState::new("page-1".into(), "https://x".into(), PageHandle(1))
    }

    #[test]
    fn first_snapshot_is_full() {
        let mut p = make_state();
        let (_token, form) = p.apply_snapshot(sample_tree("Hello"));
        match form {
            ViewForm::Full(tree) => assert_eq!(tree.roots[0].label, "Hello"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn second_snapshot_with_change_is_delta() {
        let mut p = make_state();
        p.apply_snapshot(sample_tree("Hello"));
        let (_token, form) = p.apply_snapshot(sample_tree("World"));
        assert!(matches!(form, ViewForm::Delta(_)));
    }

    #[test]
    fn unchanged_snapshot_is_no_change() {
        let mut p = make_state();
        p.apply_snapshot(sample_tree("Hello"));
        let (_token, form) = p.apply_snapshot(sample_tree("Hello"));
        assert!(matches!(form, ViewForm::NoChange));
    }

    #[test]
    fn baseline_reset_forces_full() {
        let mut p = make_state();
        p.apply_snapshot(sample_tree("Hello"));
        p.invalidate_baseline();
        let (_token, form) = p.apply_snapshot(sample_tree("Hello"));
        assert!(matches!(form, ViewForm::Full(_)));
    }

    #[test]
    fn token_changes_when_tree_changes() {
        let mut p = make_state();
        let (t1, _) = p.apply_snapshot(sample_tree("A"));
        let (t2, _) = p.apply_snapshot(sample_tree("B"));
        assert_ne!(t1, t2);
    }

    #[test]
    fn refs_accumulate_in_seen_set() {
        let mut p = make_state();
        let mut t = Tree::from_root(Node::leaf(Ref(1), Role::Doc, "X"));
        t.roots[0].children.push(Node::leaf(Ref(2), Role::P, "p1"));
        p.apply_snapshot(t);
        assert!(p.seen_refs.contains(&Ref(1)));
        assert!(p.seen_refs.contains(&Ref(2)));
    }

    #[test]
    fn find_node_returns_match() {
        let mut p = make_state();
        let mut t = Tree::from_root(Node::leaf(Ref(1), Role::Doc, "X"));
        t.roots[0].children.push(Node::leaf(Ref(2), Role::P, "kid"));
        p.apply_snapshot(t);
        let node = p.find_node(Ref(2)).unwrap();
        assert_eq!(node.label, "kid");
    }
}
