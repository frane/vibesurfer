//! Delta encode/parse round-trips, apply/diff correctness, and
//! property tests over random trees.

use std::collections::{BTreeMap, BTreeSet};

use proptest::prelude::*;

use super::apply::{find_mut, take_subtree};
use super::{apply, diff, encode, parse, DeltaOp};
use crate::codes::{Op, Role};
use crate::tree::{Node, Ref, Tree};

#[test]
fn add_round_trip() {
    let mut attrs = BTreeMap::new();
    attrs.insert("href".into(), "https://example.com".into());
    let op = DeltaOp::Add {
        r: Ref(7),
        parent: Ref(1),
        pos: Some(2),
        role: Role::Lnk,
        label: "Click here".into(),
        ops: BTreeSet::from([Op::Click]),
        attrs,
    };
    let s = encode(std::slice::from_ref(&op));
    assert_eq!(
        s,
        "+7@1:2 lnk \"Click here\" click href=https://example.com\n"
    );
    let parsed = parse(&s).unwrap();
    assert_eq!(parsed, vec![op]);
}

#[test]
fn remove_round_trip() {
    let s = encode(&[DeltaOp::Remove { r: Ref(42) }]);
    assert_eq!(s, "-42\n");
    let parsed = parse(&s).unwrap();
    assert_eq!(parsed, vec![DeltaOp::Remove { r: Ref(42) }]);
}

#[test]
fn update_round_trip() {
    let mut attrs = BTreeMap::new();
    attrs.insert("aria-pressed".into(), "true".into());
    let op = DeltaOp::Update { r: Ref(3), attrs };
    let s = encode(std::slice::from_ref(&op));
    assert_eq!(s, "~3 aria-pressed=true\n");
    assert_eq!(parse(&s).unwrap(), vec![op]);
}

#[test]
fn move_round_trip() {
    let op = DeltaOp::Move {
        r: Ref(5),
        parent: Ref(2),
        pos: Some(0),
    };
    let s = encode(std::slice::from_ref(&op));
    assert_eq!(s, ">5@2:0\n");
    assert_eq!(parse(&s).unwrap(), vec![op]);
}

#[test]
fn replace_round_trip() {
    let new_node = Node::leaf(Ref(1), Role::Doc, "Replaced");
    let op = DeltaOp::Replace {
        r: Ref(1),
        subtree: vec![new_node],
    };
    let s = encode(std::slice::from_ref(&op));
    let parsed = parse(&s).unwrap();
    assert_eq!(parsed, vec![op]);
}

#[test]
fn apply_add_to_root() {
    let a = Tree::new();
    let ops = vec![DeltaOp::Add {
        r: Ref(1),
        parent: Ref::ROOT,
        pos: None,
        role: Role::Doc,
        label: "Hello".into(),
        ops: BTreeSet::new(),
        attrs: BTreeMap::new(),
    }];
    let tree = apply(&a, &ops).unwrap();
    assert_eq!(tree.roots.len(), 1);
    assert_eq!(tree.roots[0].label, "Hello");
}

#[test]
fn apply_diff_round_trip_simple() {
    let a = Tree::from_root(Node::leaf(Ref(1), Role::Doc, "A"));
    let b = Tree::from_root(Node::leaf(Ref(1), Role::Doc, "B"));
    let ops = diff(&a, &b);
    let result = apply(&a, &ops).unwrap();
    assert_eq!(result, b);
}

#[test]
fn apply_diff_attr_change() {
    let mut a_root = Node::leaf(Ref(1), Role::Doc, "X");
    a_root.attrs.insert("k".into(), "1".into());
    let a = Tree::from_root(a_root);
    let mut b_root = Node::leaf(Ref(1), Role::Doc, "X");
    b_root.attrs.insert("k".into(), "2".into());
    let b = Tree::from_root(b_root);
    let ops = diff(&a, &b);
    assert!(matches!(ops.as_slice(), [DeltaOp::Update { .. }]));
    assert_eq!(apply(&a, &ops).unwrap(), b);
}

#[test]
fn apply_diff_add_child() {
    let a = Tree::from_root(Node::leaf(Ref(1), Role::Doc, "X"));
    let mut b_root = Node::leaf(Ref(1), Role::Doc, "X");
    b_root.children.push(Node::leaf(Ref(2), Role::P, "kid"));
    let b = Tree::from_root(b_root);
    let ops = diff(&a, &b);
    assert!(matches!(ops.as_slice(), [DeltaOp::Add { .. }]));
    assert_eq!(apply(&a, &ops).unwrap(), b);
}

#[test]
fn apply_diff_remove_child() {
    let mut a_root = Node::leaf(Ref(1), Role::Doc, "X");
    a_root.children.push(Node::leaf(Ref(2), Role::P, "kid"));
    let a = Tree::from_root(a_root);
    let b = Tree::from_root(Node::leaf(Ref(1), Role::Doc, "X"));
    let ops = diff(&a, &b);
    assert!(matches!(ops.as_slice(), [DeltaOp::Remove { r: Ref(2) }]));
    assert_eq!(apply(&a, &ops).unwrap(), b);
}

#[test]
fn apply_diff_reorder_children() {
    let mut a_root = Node::leaf(Ref(1), Role::Doc, "X");
    a_root.children.push(Node::leaf(Ref(2), Role::P, "a"));
    a_root.children.push(Node::leaf(Ref(3), Role::P, "b"));
    let a = Tree::from_root(a_root);
    let mut b_root = Node::leaf(Ref(1), Role::Doc, "X");
    b_root.children.push(Node::leaf(Ref(3), Role::P, "b"));
    b_root.children.push(Node::leaf(Ref(2), Role::P, "a"));
    let b = Tree::from_root(b_root);
    let ops = diff(&a, &b);
    let result = apply(&a, &ops).unwrap();
    assert_eq!(result, b);
}

#[test]
fn diff_identity() {
    let mut root = Node::leaf(Ref(1), Role::Doc, "X");
    root.children.push(Node::leaf(Ref(2), Role::P, "kid"));
    let t = Tree::from_root(root);
    assert!(diff(&t, &t).is_empty());
}

// ---- Property tests ----

fn arb_label() -> impl Strategy<Value = String> {
    prop::collection::vec(prop::char::range('a', 'z'), 0..8).prop_map(|cs| cs.into_iter().collect())
}

fn arb_attrs() -> impl Strategy<Value = BTreeMap<String, String>> {
    prop::collection::btree_map(
        prop::collection::vec(prop::char::range('a', 'z'), 1..6)
            .prop_map(|cs| cs.into_iter().collect::<String>()),
        prop::collection::vec(prop::char::range('a', 'z'), 0..8)
            .prop_map(|cs| cs.into_iter().collect::<String>()),
        0..4,
    )
}

fn arb_ops() -> impl Strategy<Value = BTreeSet<Op>> {
    prop::collection::vec(0u8..7, 0..4)
        .prop_map(|v| v.into_iter().map(|i| Op::all()[i as usize]).collect())
}

fn arb_role() -> impl Strategy<Value = Role> {
    (0u8..25).prop_map(|i| Role::all()[i as usize])
}

fn arb_simple_tree() -> impl Strategy<Value = Tree> {
    (
        arb_role(),
        arb_label(),
        arb_ops(),
        arb_attrs(),
        prop::collection::vec(
            (
                arb_role(),
                arb_label(),
                arb_ops(),
                arb_attrs(),
                prop::collection::vec((arb_role(), arb_label(), arb_ops(), arb_attrs()), 0..3),
            ),
            0..3,
        ),
    )
        .prop_map(|(rr, rl, ro, ra, kids)| {
            let mut next = 2u32;
            let children = kids
                .into_iter()
                .map(|(cr, cl, co, ca, gks)| {
                    let cref = next;
                    next += 1;
                    let grands = gks
                        .into_iter()
                        .map(|(gr, gl, go, ga)| {
                            let g = next;
                            next += 1;
                            Node {
                                r: Ref(g),
                                role: gr,
                                label: gl,
                                ops: go,
                                attrs: ga,
                                children: Vec::new(),
                            }
                        })
                        .collect();
                    Node {
                        r: Ref(cref),
                        role: cr,
                        label: cl,
                        ops: co,
                        attrs: ca,
                        children: grands,
                    }
                })
                .collect();
            Tree::from_root(Node {
                r: Ref(1),
                role: rr,
                label: rl,
                ops: ro,
                attrs: ra,
                children,
            })
        })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn proptest_tree_round_trip(t in arb_simple_tree()) {
        let s = t.encode();
        let parsed = Tree::parse(&s).expect("parse encoded tree");
        prop_assert_eq!(parsed, t);
    }

    #[test]
    fn proptest_delta_round_trip_via_edit(
        base in arb_simple_tree(),
        seed in any::<u64>(),
    ) {
        let edited = mutate_tree(&base, seed);
        let ops = diff(&base, &edited);
        let result = apply(&base, &ops).expect("apply");
        prop_assert_eq!(result, edited);
    }

    #[test]
    fn proptest_delta_encode_round_trip(
        base in arb_simple_tree(),
        seed in any::<u64>(),
    ) {
        let edited = mutate_tree(&base, seed);
        let ops = diff(&base, &edited);
        let s = encode(&ops);
        let parsed = parse(&s).expect("parse encoded delta");
        prop_assert_eq!(parsed, ops);
    }
}

/// Apply a deterministic mutation to `base`, derived from `seed`.
#[allow(clippy::cast_possible_truncation)]
fn mutate_tree(base: &Tree, seed: u64) -> Tree {
    use std::collections::HashSet;
    let mut t = base.clone();
    let nodes_in_order: Vec<Ref> = t.iter().map(|n| n.r).collect();
    if nodes_in_order.is_empty() {
        return t;
    }
    let kind = (seed >> 1) % 4;
    let pick = nodes_in_order[(seed as usize) % nodes_in_order.len()];

    match kind {
        0 => {
            if let Some(n) = find_mut(&mut t, pick) {
                n.attrs.insert("k".into(), format!("v{}", seed % 100));
            }
        }
        1 => {
            if t.iter().count() > 1 {
                let _ = take_subtree(&mut t, pick);
            }
        }
        2 => {
            if let Some(parent) = parent_of(&t, pick) {
                if let Some(parent_node) = find_mut(&mut t, parent) {
                    let n = parent_node.children.len();
                    if n >= 2 {
                        let i = (seed as usize) % n;
                        let j = (i + 1) % n;
                        parent_node.children.swap(i, j);
                    }
                }
            }
        }
        _ => {
            let used: HashSet<u32> = t.iter().map(|n| n.r.0).collect();
            let mut new_ref = (seed.wrapping_mul(2_654_435_761) as u32) | 1;
            while used.contains(&new_ref) {
                new_ref = new_ref.wrapping_add(1);
            }
            if let Some(parent) = find_mut(&mut t, pick) {
                parent.children.push(Node::leaf(Ref(new_ref), Role::P, "x"));
            }
        }
    }
    t
}

fn parent_of(tree: &Tree, child: Ref) -> Option<Ref> {
    for root in &tree.roots {
        if root.children.iter().any(|c| c.r == child) {
            return Some(root.r);
        }
        if let Some(p) = parent_of_in(root, child) {
            return Some(p);
        }
    }
    None
}

fn parent_of_in(node: &Node, child: Ref) -> Option<Ref> {
    for c in &node.children {
        if c.r == child {
            return Some(node.r);
        }
        if let Some(p) = parent_of_in(c, child) {
            return Some(p);
        }
    }
    None
}
