//! In-memory tree representation and full-tree wire format.
//!
//! See `docs/PROTOCOL.md` § "Tree representation" for the wire spec.
//! Each node line is:
//!
//! ```text
//! <ref> <role> <label> [op[,op]...] [k=v]...
//! ```
//!
//! …prefixed by two spaces of indent per nesting level. The encoder
//! emits a canonical form (attributes sorted alphabetically, ops in
//! source order via [`std::collections::BTreeSet`]); the parser is
//! tolerant of either ordering.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::{self, Write as _};
use std::str::FromStr;

use crate::codes::{Op, Role};
use crate::error::{ParseError, Result};
use crate::tokenize::{quote_label, quote_value, split_indent, strip_quotes, Tokenizer};

/// A stable session-scoped element identifier. Refs are positive
/// integers; `0` is reserved as the wire sentinel for "root level"
/// in delta operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Ref(pub u32);

impl Ref {
    /// The wire sentinel for "root level" parents in delta `Add` / `Move`.
    pub const ROOT: Ref = Ref(0);

    /// True if this ref is the root sentinel.
    #[must_use]
    pub const fn is_root(self) -> bool {
        self.0 == 0
    }
}

impl fmt::Display for Ref {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl FromStr for Ref {
    type Err = ParseError;
    fn from_str(s: &str) -> Result<Self> {
        s.parse::<u32>()
            .map(Ref)
            .map_err(|_| ParseError::InvalidRef { raw: s.to_string() })
    }
}

/// One node in the a11y tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    pub r: Ref,
    pub role: Role,
    pub label: String,
    pub ops: BTreeSet<Op>,
    pub attrs: BTreeMap<String, String>,
    pub children: Vec<Node>,
}

impl Node {
    /// Build a leaf node with no ops, attrs, or children.
    #[must_use]
    pub fn leaf(r: Ref, role: Role, label: impl Into<String>) -> Self {
        Self {
            r,
            role,
            label: label.into(),
            ops: BTreeSet::new(),
            attrs: BTreeMap::new(),
            children: Vec::new(),
        }
    }
}

/// A full a11y tree as exchanged on the wire.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Tree {
    pub roots: Vec<Node>,
}

impl Tree {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn from_root(root: Node) -> Self {
        Self { roots: vec![root] }
    }

    /// Iterate all nodes in the tree in pre-order.
    #[must_use]
    pub fn iter(&self) -> TreeIter<'_> {
        TreeIter::new(self)
    }

    /// Encode the tree as a sequence of indented lines (no trailing
    /// blank line, no envelope). The result is the body that follows
    /// a success envelope on the wire.
    #[must_use]
    pub fn encode(&self) -> String {
        let mut out = String::new();
        for root in &self.roots {
            encode_node(root, 0, &mut out);
        }
        out
    }

    /// Parse the body of a `vs_view` response. The input must contain
    /// only tree lines — no envelope, no warnings.
    pub fn parse(input: &str) -> Result<Self> {
        let mut entries: Vec<(usize, NodeShell)> = Vec::new();
        for (idx, raw_line) in input.lines().enumerate() {
            if raw_line.trim().is_empty() {
                continue;
            }
            let (depth, body) = split_indent(raw_line);
            let shell = parse_node_line(body, idx + 1)?;
            entries.push((depth, shell));
        }
        Ok(assemble(&entries))
    }
}

#[derive(Debug)]
struct NodeShell {
    r: Ref,
    role: Role,
    label: String,
    ops: BTreeSet<Op>,
    attrs: BTreeMap<String, String>,
}

fn assemble(entries: &[(usize, NodeShell)]) -> Tree {
    let mut roots: Vec<Node> = Vec::new();
    let mut stack: Vec<(usize, Node)> = Vec::new();

    for (depth, shell) in entries {
        // Pop any node at depth >= current; attach to its parent.
        while let Some(&(top_depth, _)) = stack.last() {
            if top_depth >= *depth {
                let (_, popped) = stack.pop().expect("non-empty");
                if let Some(parent) = stack.last_mut() {
                    parent.1.children.push(popped);
                } else {
                    roots.push(popped);
                }
            } else {
                break;
            }
        }
        let node = Node {
            r: shell.r,
            role: shell.role,
            label: shell.label.clone(),
            ops: shell.ops.clone(),
            attrs: shell.attrs.clone(),
            children: Vec::new(),
        };
        stack.push((*depth, node));
    }

    while let Some((_, popped)) = stack.pop() {
        if let Some(parent) = stack.last_mut() {
            parent.1.children.push(popped);
        } else {
            roots.push(popped);
        }
    }

    Tree { roots }
}

fn encode_node(node: &Node, depth: usize, out: &mut String) {
    for _ in 0..depth {
        out.push_str("  ");
    }
    write!(out, "{} {} {}", node.r, node.role, quote_label(&node.label))
        .expect("write to String never fails");
    if !node.ops.is_empty() {
        out.push(' ');
        let mut first = true;
        for op in &node.ops {
            if !first {
                out.push(',');
            }
            out.push_str(op.as_str());
            first = false;
        }
    }
    for (k, v) in &node.attrs {
        out.push(' ');
        out.push_str(k);
        out.push('=');
        out.push_str(&quote_value(v));
    }
    out.push('\n');
    for child in &node.children {
        encode_node(child, depth + 1, out);
    }
}

fn parse_node_line(line: &str, line_no: usize) -> Result<NodeShell> {
    let mut tokens = Tokenizer::new(line);
    let r_tok = tokens.next().ok_or(ParseError::MalformedLine {
        line: line_no,
        message: "missing ref",
    })?;
    let role_tok = tokens.next().ok_or(ParseError::MalformedLine {
        line: line_no,
        message: "missing role",
    })?;
    let label_tok = tokens.next().ok_or(ParseError::MalformedLine {
        line: line_no,
        message: "missing label",
    })?;

    let r: Ref = r_tok.parse()?;
    let role: Role = role_tok.parse()?;
    let label = strip_quotes(label_tok).to_string();

    let mut ops = BTreeSet::new();
    let mut attrs = BTreeMap::new();
    for tok in tokens {
        if let Some((k, v)) = tok.split_once('=') {
            if k.is_empty() {
                return Err(ParseError::InvalidAttribute { raw: tok.into() });
            }
            attrs.insert(k.to_string(), strip_quotes(v).to_string());
        } else {
            for piece in tok.split(',') {
                if piece.is_empty() {
                    return Err(ParseError::MalformedLine {
                        line: line_no,
                        message: "empty op in cluster",
                    });
                }
                ops.insert(piece.parse::<Op>()?);
            }
        }
    }

    Ok(NodeShell {
        r,
        role,
        label,
        ops,
        attrs,
    })
}

impl<'a> IntoIterator for &'a Tree {
    type Item = &'a Node;
    type IntoIter = TreeIter<'a>;
    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

/// Pre-order iterator over a tree.
pub struct TreeIter<'a> {
    stack: Vec<&'a Node>,
}

impl<'a> TreeIter<'a> {
    fn new(tree: &'a Tree) -> Self {
        let mut stack: Vec<&'a Node> = Vec::new();
        // Push roots in reverse so the first root pops first.
        for root in tree.roots.iter().rev() {
            stack.push(root);
        }
        Self { stack }
    }
}

impl<'a> Iterator for TreeIter<'a> {
    type Item = &'a Node;
    fn next(&mut self) -> Option<&'a Node> {
        let node = self.stack.pop()?;
        for child in node.children.iter().rev() {
            self.stack.push(child);
        }
        Some(node)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codes::Op;

    fn sample_tree() -> Tree {
        let mut p = Node::leaf(Ref(3), Role::P, "This domain is for examples.");
        p.attrs.insert("lang".into(), "en".into());

        let mut lnk = Node::leaf(Ref(4), Role::Lnk, "More information");
        lnk.ops.insert(Op::Click);
        lnk.attrs
            .insert("href".into(), "https://www.iana.org/".into());

        let mut hd = Node::leaf(Ref(2), Role::Hd, "Example Domain");
        hd.attrs.insert("level".into(), "1".into());

        let doc = Node {
            r: Ref(1),
            role: Role::Doc,
            label: "Example Domain".into(),
            ops: BTreeSet::new(),
            attrs: BTreeMap::new(),
            children: vec![hd, p, lnk],
        };
        Tree::from_root(doc)
    }

    #[test]
    fn encode_sample_matches_spec_shape() {
        let encoded = sample_tree().encode();
        let expected = "\
1 doc \"Example Domain\"
  2 hd \"Example Domain\" level=1
  3 p \"This domain is for examples.\" lang=en
  4 lnk \"More information\" click href=https://www.iana.org/
";
        assert_eq!(encoded, expected);
    }

    #[test]
    fn round_trip_sample() {
        let t = sample_tree();
        let s = t.encode();
        let parsed = Tree::parse(&s).expect("parses");
        assert_eq!(parsed, t);
    }

    #[test]
    fn empty_tree_round_trips() {
        let empty = Tree::new();
        assert_eq!(empty.encode(), "");
        assert_eq!(Tree::parse("").unwrap(), empty);
    }

    #[test]
    fn parse_handles_blank_lines() {
        let s = "\n1 doc empty\n\n";
        let parsed = Tree::parse(s).unwrap();
        assert_eq!(parsed.roots.len(), 1);
        assert_eq!(parsed.roots[0].label, "empty");
    }

    #[test]
    fn parse_rejects_unknown_role() {
        let err = Tree::parse("1 zzz foo").unwrap_err();
        matches!(err, ParseError::UnknownCode(_));
    }

    #[test]
    fn parse_rejects_bad_ref() {
        let err = Tree::parse("xx doc foo").unwrap_err();
        matches!(err, ParseError::InvalidRef { .. });
    }

    #[test]
    fn ops_cluster_round_trips() {
        let mut node = Node::leaf(Ref(1), Role::Btn, "Submit");
        node.ops.insert(Op::Click);
        node.ops.insert(Op::Focus);
        node.ops.insert(Op::Hover);
        let tree = Tree::from_root(node);
        let s = tree.encode();
        // BTreeSet iter order is `Click < Fill < ... < Focus < Hover` per source order.
        assert!(s.contains("click,hover,focus"));
        let back = Tree::parse(&s).unwrap();
        assert_eq!(back, tree);
    }

    #[test]
    fn attribute_with_whitespace_quoted() {
        let mut node = Node::leaf(Ref(1), Role::Tf, "");
        node.attrs
            .insert("placeholder".into(), "Your name here".into());
        let s = Tree::from_root(node.clone()).encode();
        assert!(s.contains("placeholder=\"Your name here\""));
        let back = Tree::parse(&s).unwrap();
        assert_eq!(back, Tree::from_root(node));
    }

    #[test]
    fn iter_pre_order() {
        let t = sample_tree();
        let refs: Vec<u32> = t.iter().map(|n| n.r.0).collect();
        assert_eq!(refs, vec![1, 2, 3, 4]);
    }
}
