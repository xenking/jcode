use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// A rendered user or assistant row. The digest keeps long prompts and answers
/// out of the mutation request, and includes the role to distinguish echoes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RewindMessageKey([u8; 32]);

impl RewindMessageKey {
    pub fn new(role: &str, text: &str) -> Option<Self> {
        let text = text.trim();
        if !matches!(role, "user" | "assistant") || text.is_empty() {
            return None;
        }
        let mut hash = Sha256::new();
        hash.update(role);
        hash.update([0]);
        hash.update(text);
        Some(Self(hash.finalize().into()))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RewindAnchor {
    pub selected: RewindMessageKey,
    pub before: Option<RewindMessageKey>,
    pub after: Option<RewindMessageKey>,
}

impl RewindAnchor {
    /// Distinct messages need not have identical neighbors in every client:
    /// paging and local notices can change the visible neighbors. For repeated
    /// text, use exact context and refuse to guess if it is still ambiguous.
    pub fn resolve(&self, keys: &[RewindMessageKey]) -> Option<usize> {
        let mut matches = keys
            .iter()
            .enumerate()
            .filter_map(|(index, key)| (*key == self.selected).then_some(index));
        let first = matches.next()?;
        let Some(second) = matches.next() else {
            return Some(first);
        };
        let mut matches = [first, second].into_iter().chain(matches).filter(|&index| {
            (self.before.is_some() || self.after.is_some())
                && self
                    .before
                    .is_none_or(|key| index.checked_sub(1).and_then(|i| keys.get(i)) == Some(&key))
                && self
                    .after
                    .is_none_or(|key| keys.get(index + 1) == Some(&key))
        });
        matches.next().filter(|_| matches.next().is_none())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repeated_prompt_requires_unambiguous_context() {
        let user = RewindMessageKey::new("user", "Repeat prompt").unwrap();
        let first = RewindMessageKey::new("assistant", "First answer").unwrap();
        let second = RewindMessageKey::new("assistant", "Second answer").unwrap();
        let history = [user, first, user, second];
        let anchor = RewindAnchor {
            selected: user,
            before: None,
            after: None,
        };
        assert_eq!(anchor.resolve(&history), None);
        let anchor = RewindAnchor {
            after: Some(second),
            ..anchor
        };
        assert_eq!(anchor.resolve(&history), Some(2));
        assert_eq!(anchor.resolve(&[user, second, user, second]), None);
        assert_eq!(anchor.resolve(&[first, second]), None);
    }
}
