//! Value objects for the checkout domain: a validated [`BranchName`] and the
//! [`CheckoutTarget`] a caller asks for. Both are parsed once at the boundary
//! (CLI arg, Tilt button), so the aggregate's interior can trust them.

use thiserror::Error;

/// The sentinel target meaning "each repo's own default branch" — `checkout
/// default` puts every repo on its default rather than one shared branch. A repo
/// with a branch literally named `default` is shadowed by this alias.
pub const DEFAULT_ALIAS: &str = "default";

/// A rejected domain input.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum DomainError {
    #[error("branch name cannot be empty")]
    EmptyBranch,
    #[error("branch name must not contain whitespace: {0:?}")]
    WhitespaceInBranch(String),
}

/// A git branch name. Non-empty and whitespace-free; a leading `-` is allowed
/// (the git layer passes `--end-of-options`, so such names are refs, not flags).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BranchName(String);

impl BranchName {
    /// Parses user-supplied text into a branch name, trimming surrounding
    /// whitespace and rejecting the empty / internally-spaced cases.
    pub fn parse(s: &str) -> Result<BranchName, DomainError> {
        let s = s.trim();
        if s.is_empty() {
            return Err(DomainError::EmptyBranch);
        }
        if s.chars().any(char::is_whitespace) {
            return Err(DomainError::WhitespaceInBranch(s.to_string()));
        }
        Ok(BranchName(s.to_string()))
    }

    /// Wraps a name that git itself produced (e.g. the resolved default branch),
    /// which is already a valid ref — so it skips parsing.
    pub(crate) fn from_trusted(s: String) -> BranchName {
        BranchName(s)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for BranchName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// What a checkout should switch to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckoutTarget {
    /// Each repo's own default branch (the [`DEFAULT_ALIAS`] input).
    Default,
    Named(BranchName),
}

impl CheckoutTarget {
    /// Parses a user-supplied target: [`DEFAULT_ALIAS`] selects each repo's own
    /// default branch; anything else is a branch name.
    pub fn parse(s: &str) -> Result<CheckoutTarget, DomainError> {
        if s == DEFAULT_ALIAS {
            Ok(CheckoutTarget::Default)
        } else {
            Ok(CheckoutTarget::Named(BranchName::parse(s)?))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_trims_and_accepts_a_plain_name() {
        assert_eq!(
            BranchName::parse("  feature  ").unwrap().as_str(),
            "feature"
        );
    }

    #[test]
    fn parse_allows_a_leading_dash() {
        assert_eq!(BranchName::parse("-weird").unwrap().as_str(), "-weird");
    }

    #[test]
    fn parse_rejects_empty_and_whitespace() {
        assert_eq!(BranchName::parse("   "), Err(DomainError::EmptyBranch));
        assert!(matches!(
            BranchName::parse("two words"),
            Err(DomainError::WhitespaceInBranch(_))
        ));
    }

    #[test]
    fn checkout_target_maps_default_alias_and_names() {
        assert_eq!(
            CheckoutTarget::parse("default").unwrap(),
            CheckoutTarget::Default
        );
        assert_eq!(
            CheckoutTarget::parse("main").unwrap(),
            CheckoutTarget::Named(BranchName::parse("main").unwrap())
        );
    }
}
