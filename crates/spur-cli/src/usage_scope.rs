// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! What a usage reply may be presented as, given `[auth] private_data = ["usage"]`.

/// The user the controller pinned a usage or fairshare reply to, if any.
///
/// A pinned reply holds only that user's rows. Summing it would print the
/// caller's own figures as an account or cluster total, and dividing by it
/// would make the caller's share look like 100%.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct UsageScope {
    pinned: Option<String>,
    /// A coordinator's accounts: every returned row is real, and sums are not
    /// the cluster.
    partial: bool,
}

impl UsageScope {
    /// Pinned if any reply says so: a caller pinned on one RPC is pinned on all.
    pub(crate) fn from_replies<'a>(scoped: impl IntoIterator<Item = Option<&'a str>>) -> Self {
        Self {
            pinned: scoped.into_iter().flatten().next().map(str::to_string),
            partial: false,
        }
    }

    pub(crate) fn with_partial(mut self, partial: impl IntoIterator<Item = bool>) -> Self {
        self.partial = partial.into_iter().any(|flag| flag);
        self
    }

    /// Sums and shares of the cluster are real.
    pub(crate) fn totals_visible(&self) -> bool {
        self.pinned.is_none() && !self.partial
    }

    /// Account rows cover every user of that account, so their raw usage is real.
    /// A single-user pin does not.
    pub(crate) fn account_rows_visible(&self) -> bool {
        self.pinned.is_none()
    }

    /// The reply was pinned to one user.
    pub(crate) fn is_single_user(&self) -> bool {
        self.pinned.is_some()
    }

    /// `user`'s own figures are in the reply, so a missing row is a real zero.
    pub(crate) fn user_visible(&self, user: &str) -> bool {
        self.pinned.as_deref().is_none_or(|me| me == user)
    }
}

/// `value` when `shown`, else a blank cell of the same width.
pub(crate) fn cell(shown: bool, width: usize, value: impl FnOnce() -> String) -> String {
    if shown {
        value()
    } else {
        format!("{:>width$}", "")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unpinned_reply_shows_everything() {
        let scope = UsageScope::from_replies([None, None]);
        assert!(scope.totals_visible());
        assert!(scope.user_visible("alice"));
        assert!(scope.user_visible("bob"));
    }

    #[test]
    fn a_pinned_reply_shows_only_the_caller_and_no_totals() {
        let scope = UsageScope::from_replies([None, Some("alice")]);
        assert!(!scope.totals_visible());
        assert!(scope.user_visible("alice"));
        assert!(!scope.user_visible("bob"));
    }

    #[test]
    fn a_nameless_pin_shows_nobody() {
        let scope = UsageScope::from_replies([Some("")]);
        assert!(!scope.totals_visible());
        assert!(!scope.user_visible("alice"));
    }

    #[test]
    fn a_partial_reply_shows_every_returned_row_and_no_cluster_total() {
        let scope = UsageScope::from_replies([None]).with_partial([true]);
        assert!(!scope.totals_visible());
        assert!(scope.account_rows_visible());
        assert!(!scope.is_single_user());
        assert!(scope.user_visible("bob"));
    }

    #[test]
    fn a_hidden_cell_keeps_its_width() {
        assert_eq!(cell(true, 12, || format!("{:>12.1}", 3.0)), "         3.0");
        assert_eq!(cell(false, 12, || unreachable!()), " ".repeat(12));
    }
}
