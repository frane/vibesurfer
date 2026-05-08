//! State token computation.
//!
//! `token = blake3(canonical(tree) || 0 || url || 0 || page_id)[..8].hex()`
//!
//! The canonicalization comes "for free" from `vs_protocol::Tree::encode`:
//! attributes are stored in `BTreeMap` and emitted alphabetically; ops are
//! stored in `BTreeSet` and emitted in source order. Two structurally
//! identical trees therefore produce the same token.

use blake3::Hasher;
use vs_protocol::{StateToken, Tree};

/// Hash the canonical encoding of `tree` together with `url` and
/// `page_id` and return the leading 8 bytes as a [`StateToken`].
#[must_use]
pub fn compute(tree: &Tree, url: &str, page_id: &str) -> StateToken {
    let canonical = tree.encode();
    let mut h = Hasher::new();
    h.update(canonical.as_bytes());
    h.update(b"\0");
    h.update(url.as_bytes());
    h.update(b"\0");
    h.update(page_id.as_bytes());
    let hash = h.finalize();
    let bytes = hash.as_bytes();
    let mut out = [0u8; 8];
    out.copy_from_slice(&bytes[..8]);
    StateToken::from_bytes(out)
}

/// Hash request args for the idempotency cache. The daemon stores this
/// as a hex string in `actions.args_hash`.
#[must_use]
pub fn args_hash(primitive: &str, args: &[String]) -> String {
    let mut h = Hasher::new();
    h.update(primitive.as_bytes());
    h.update(b"\0");
    for arg in args {
        h.update(arg.as_bytes());
        h.update(b"\0");
    }
    h.finalize().to_hex().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use vs_protocol::{Node, Ref, Role};

    fn sample_tree() -> Tree {
        Tree::from_root(Node::leaf(Ref(1), Role::Doc, "Hello"))
    }

    #[test]
    fn token_is_deterministic() {
        let t = sample_tree();
        let a = compute(&t, "https://example.com", "page-1");
        let b = compute(&t, "https://example.com", "page-1");
        assert_eq!(a, b);
    }

    #[test]
    fn token_changes_with_url() {
        let t = sample_tree();
        let a = compute(&t, "https://example.com", "page-1");
        let b = compute(&t, "https://example.org", "page-1");
        assert_ne!(a, b);
    }

    #[test]
    fn token_changes_with_page_id() {
        let t = sample_tree();
        let a = compute(&t, "https://example.com", "page-1");
        let b = compute(&t, "https://example.com", "page-2");
        assert_ne!(a, b);
    }

    #[test]
    fn token_changes_with_tree_structure() {
        let mut t1 = sample_tree();
        t1.roots[0].label = "Hello".into();
        let mut t2 = sample_tree();
        t2.roots[0].label = "Goodbye".into();
        let a = compute(&t1, "u", "p");
        let b = compute(&t2, "u", "p");
        assert_ne!(a, b);
    }

    #[test]
    fn args_hash_is_deterministic() {
        let h1 = args_hash("vs_act", &["7".into(), "click".into()]);
        let h2 = args_hash("vs_act", &["7".into(), "click".into()]);
        assert_eq!(h1, h2);
    }

    #[test]
    fn args_hash_distinguishes_argument_boundaries() {
        // ("ab", "c") != ("a", "bc")
        let h1 = args_hash("vs_act", &["ab".into(), "c".into()]);
        let h2 = args_hash("vs_act", &["a".into(), "bc".into()]);
        assert_ne!(h1, h2);
    }
}
