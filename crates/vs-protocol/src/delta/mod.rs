//! Delta operations: encoding, parsing, application, and diffing.
//!
//! See `docs/PROTOCOL.md` § "Tree deltas". Five operations:
//!
//! | Op      | Wire                                          |
//! | ------- | --------------------------------------------- |
//! | Add     | `+<ref>@<parent>[:<pos>] <role> <label> ...`  |
//! | Remove  | `-<ref>`                                      |
//! | Update  | `~<ref> <k>=<v> ...`                          |
//! | Move    | `><ref>@<parent>[:<pos>]`                     |
//! | Replace | `*<ref>` then indented subtree                |
//!
//! The wire encoding is canonical (attributes sorted, ops in source
//! order); the parser is tolerant.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use crate::codes::{Op, Role};
use crate::error::{ParseError, Result};
use crate::tokenize::{quote_label, quote_value, split_indent, strip_quotes, Tokenizer};
use crate::tree::{Node, Ref, Tree};

/// One delta operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeltaOp {
    /// Insert a new leaf node under `parent` at `pos` (or end if `None`).
    /// `parent == Ref::ROOT` means root level.
    Add {
        r: Ref,
        parent: Ref,
        pos: Option<u32>,
        role: Role,
        label: String,
        ops: BTreeSet<Op>,
        attrs: BTreeMap<String, String>,
    },
    /// Remove the subtree rooted at `r`.
    Remove { r: Ref },
    /// Replace the attribute map at `r` (per `docs/codes.md`, Update is
    /// attribute-only; for role/label/ops changes use [`DeltaOp::Replace`]).
    Update {
        r: Ref,
        attrs: BTreeMap<String, String>,
    },
    /// Reparent or reorder `r` to `parent` at `pos`.
    Move {
        r: Ref,
        parent: Ref,
        pos: Option<u32>,
    },
    /// Blow away the subtree at `r` and install `subtree` in its place.
    /// `subtree[0].r == r`; the wire-form indents these lines under
    /// the `*<ref>` header.
    Replace { r: Ref, subtree: Vec<Node> },
}

impl DeltaOp {
    /// The ref this op operates on.
    #[must_use]
    pub fn target(&self) -> Ref {
        match self {
            Self::Add { r, .. }
            | Self::Remove { r }
            | Self::Update { r, .. }
            | Self::Move { r, .. }
            | Self::Replace { r, .. } => *r,
        }
    }
}

/// Encode a sequence of delta ops as the wire body.
#[must_use]
pub fn encode(ops: &[DeltaOp]) -> String {
    let mut out = String::new();
    for op in ops {
        encode_one(op, &mut out);
    }
    out
}

fn encode_one(op: &DeltaOp, out: &mut String) {
    match op {
        DeltaOp::Add {
            r,
            parent,
            pos,
            role,
            label,
            ops,
            attrs,
        } => {
            write!(out, "+{r}@{}", parent.0).expect("write");
            if let Some(p) = pos {
                write!(out, ":{p}").expect("write");
            }
            write!(out, " {role} {}", quote_label(label)).expect("write");
            if !ops.is_empty() {
                out.push(' ');
                let mut first = true;
                for op in ops {
                    if !first {
                        out.push(',');
                    }
                    out.push_str(op.as_str());
                    first = false;
                }
            }
            for (k, v) in attrs {
                write!(out, " {k}={}", quote_value(v)).expect("write");
            }
            out.push('\n');
        }
        DeltaOp::Remove { r } => {
            writeln!(out, "-{r}").expect("write");
        }
        DeltaOp::Update { r, attrs } => {
            write!(out, "~{r}").expect("write");
            for (k, v) in attrs {
                write!(out, " {k}={}", quote_value(v)).expect("write");
            }
            out.push('\n');
        }
        DeltaOp::Move { r, parent, pos } => {
            write!(out, ">{r}@{}", parent.0).expect("write");
            if let Some(p) = pos {
                write!(out, ":{p}").expect("write");
            }
            out.push('\n');
        }
        DeltaOp::Replace { r, subtree } => {
            writeln!(out, "*{r}").expect("write");
            for node in subtree {
                encode_subtree_line(node, 1, out);
            }
        }
    }
}

fn encode_subtree_line(node: &Node, depth: usize, out: &mut String) {
    for _ in 0..depth {
        out.push_str("  ");
    }
    write!(out, "{} {} {}", node.r, node.role, quote_label(&node.label)).expect("write");
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
        write!(out, " {k}={}", quote_value(v)).expect("write");
    }
    out.push('\n');
    for child in &node.children {
        encode_subtree_line(child, depth + 1, out);
    }
}

/// Parse a delta-op stream into a vector of ops.
pub fn parse(input: &str) -> Result<Vec<DeltaOp>> {
    let lines: Vec<&str> = input.lines().collect();
    let mut idx = 0usize;
    let mut ops = Vec::new();
    while idx < lines.len() {
        let raw = lines[idx];
        if raw.trim().is_empty() {
            idx += 1;
            continue;
        }
        let (depth, body) = split_indent(raw);
        if depth != 0 {
            return Err(ParseError::IndentJump {
                from: 0,
                to: depth,
                line: idx + 1,
            });
        }
        let sigil = body.chars().next().ok_or(ParseError::MalformedLine {
            line: idx + 1,
            message: "empty delta line",
        })?;
        match sigil {
            '+' => {
                ops.push(parse_add(&body[1..], idx + 1)?);
                idx += 1;
            }
            '-' => {
                ops.push(parse_remove(&body[1..], idx + 1)?);
                idx += 1;
            }
            '~' => {
                ops.push(parse_update(&body[1..], idx + 1)?);
                idx += 1;
            }
            '>' => {
                ops.push(parse_move(&body[1..], idx + 1)?);
                idx += 1;
            }
            '*' => {
                let (op, consumed) = parse_replace(&lines[idx..], idx + 1)?;
                ops.push(op);
                idx += consumed;
            }
            _ => {
                return Err(ParseError::MalformedLine {
                    line: idx + 1,
                    message: "unknown delta sigil",
                });
            }
        }
    }
    Ok(ops)
}

fn parse_add(rest: &str, line_no: usize) -> Result<DeltaOp> {
    let mut tokens = Tokenizer::new(rest);
    let head = tokens.next().ok_or(ParseError::MalformedLine {
        line: line_no,
        message: "missing ref@parent",
    })?;
    let (r, parent, pos) = parse_ref_at_parent(head, line_no)?;
    let role_tok = tokens.next().ok_or(ParseError::MalformedLine {
        line: line_no,
        message: "missing role",
    })?;
    let label_tok = tokens.next().ok_or(ParseError::MalformedLine {
        line: line_no,
        message: "missing label",
    })?;
    let role: Role = role_tok.parse()?;
    let label = strip_quotes(label_tok).to_string();

    let mut ops = BTreeSet::new();
    let mut attrs = BTreeMap::new();
    consume_ops_and_attrs(tokens, line_no, &mut ops, &mut attrs)?;

    Ok(DeltaOp::Add {
        r,
        parent,
        pos,
        role,
        label,
        ops,
        attrs,
    })
}

fn parse_remove(rest: &str, line_no: usize) -> Result<DeltaOp> {
    let trimmed = rest.trim();
    if trimmed.is_empty() {
        return Err(ParseError::MalformedLine {
            line: line_no,
            message: "missing ref",
        });
    }
    let r: Ref = trimmed.parse()?;
    Ok(DeltaOp::Remove { r })
}

fn parse_update(rest: &str, line_no: usize) -> Result<DeltaOp> {
    let mut tokens = Tokenizer::new(rest);
    let r_tok = tokens.next().ok_or(ParseError::MalformedLine {
        line: line_no,
        message: "missing ref",
    })?;
    let r: Ref = r_tok.parse()?;
    let mut attrs = BTreeMap::new();
    for tok in tokens {
        let (k, v) = tok.split_once('=').ok_or(ParseError::InvalidAttribute {
            raw: tok.to_string(),
        })?;
        if k.is_empty() {
            return Err(ParseError::InvalidAttribute { raw: tok.into() });
        }
        attrs.insert(k.to_string(), strip_quotes(v).to_string());
    }
    Ok(DeltaOp::Update { r, attrs })
}

fn parse_move(rest: &str, line_no: usize) -> Result<DeltaOp> {
    let trimmed = rest.trim();
    if trimmed.is_empty() {
        return Err(ParseError::MalformedLine {
            line: line_no,
            message: "missing ref@parent",
        });
    }
    let (r, parent, pos) = parse_ref_at_parent(trimmed, line_no)?;
    Ok(DeltaOp::Move { r, parent, pos })
}

fn parse_replace(lines: &[&str], header_line_no: usize) -> Result<(DeltaOp, usize)> {
    let header = &lines[0][1..]; // strip `*`
    let r: Ref = header.trim().parse()?;
    let mut consumed = 1;
    let mut indented_lines: Vec<&str> = Vec::new();
    while consumed < lines.len() {
        let raw = lines[consumed];
        if raw.trim().is_empty() {
            consumed += 1;
            continue;
        }
        let (depth, _) = split_indent(raw);
        if depth == 0 {
            break;
        }
        indented_lines.push(raw);
        consumed += 1;
    }
    if indented_lines.is_empty() {
        return Err(ParseError::MalformedLine {
            line: header_line_no,
            message: "Replace missing subtree",
        });
    }
    // Dedent by one level and parse as a tree.
    let mut dedented = String::new();
    for l in &indented_lines {
        let trimmed = l.strip_prefix("  ").unwrap_or(l);
        dedented.push_str(trimmed);
        dedented.push('\n');
    }
    let subtree = Tree::parse(&dedented)?;
    if subtree.roots.is_empty() {
        return Err(ParseError::MalformedLine {
            line: header_line_no,
            message: "Replace subtree empty",
        });
    }
    Ok((
        DeltaOp::Replace {
            r,
            subtree: subtree.roots,
        },
        consumed,
    ))
}

fn parse_ref_at_parent(s: &str, line_no: usize) -> Result<(Ref, Ref, Option<u32>)> {
    let (r_part, parent_part) = s.split_once('@').ok_or(ParseError::MalformedLine {
        line: line_no,
        message: "expected ref@parent",
    })?;
    let r: Ref = r_part.parse()?;
    let (parent_part, pos_part) = match parent_part.split_once(':') {
        Some((p, q)) => (p, Some(q)),
        None => (parent_part, None),
    };
    let parent: Ref = parent_part.parse()?;
    let pos = match pos_part {
        Some(s) => Some(
            s.parse::<u32>()
                .map_err(|_| ParseError::InvalidPosition { raw: s.to_string() })?,
        ),
        None => None,
    };
    Ok((r, parent, pos))
}

fn consume_ops_and_attrs(
    tokens: Tokenizer<'_>,
    _line_no: usize,
    ops: &mut BTreeSet<Op>,
    attrs: &mut BTreeMap<String, String>,
) -> Result<()> {
    for tok in tokens {
        if let Some((k, v)) = tok.split_once('=') {
            if k.is_empty() {
                return Err(ParseError::InvalidAttribute { raw: tok.into() });
            }
            attrs.insert(k.to_string(), strip_quotes(v).to_string());
        } else {
            for piece in tok.split(',') {
                if piece.is_empty() {
                    return Err(ParseError::InvalidAttribute { raw: tok.into() });
                }
                ops.insert(piece.parse::<Op>()?);
            }
        }
    }
    Ok(())
}

mod apply;
mod diff;

#[cfg(test)]
mod tests;

pub use apply::apply;
pub use diff::diff;
