// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright the Vortex contributors

use crate::ArrayRef;
use crate::array::ParentRef;

/// Trait for matching array types.
///
/// Matchers expose two parallel entry points:
///
/// - [`matches`](Self::matches) / [`try_match`](Self::try_match) take a [`ParentRef`].
///   This is the more general path because a `ParentRef` can borrow either a
///   heap-allocated [`ArrayRef`] or stack-allocated construction parts, so it works
///   uniformly for the optimizer's parent-reduce dispatch.
/// - [`matches_ref`](Self::matches_ref) / [`try_match_ref`](Self::try_match_ref) take
///   an [`ArrayRef`] directly. They exist as a fast path for callers that already
///   hold a heap-allocated array (e.g. `ArrayRef::is::<M>()`, `ArrayRef::as_opt::<M>()`)
///   so they don't pay for [`ParentRef`] construction.
///
/// The heap and parent paths have different associated match types. Heap matches may expose
/// APIs like `AsRef<ArrayRef>` because they borrow an existing allocation. Parent matches must
/// not hide stack materialization behind those APIs.
pub trait Matcher {
    type RefMatch<'a>;
    type ParentMatch<'a>;

    /// Check if the given parent matches this matcher type.
    ///
    /// The default implementation delegates through [`try_match`](Self::try_match).
    /// Override when a cheaper check (e.g. an encoding-id comparison) suffices.
    fn matches(parent: &ParentRef<'_>) -> bool {
        Self::try_match(parent).is_some()
    }

    /// Try to match a [`ParentRef`].
    ///
    /// The returned parent match borrows from `parent`, so matchers can return a
    /// [`ParentView`](crate::array::ParentView) without forcing the parent to materialize.
    /// Implementations typically delegate to [`ParentRef::as_opt`].
    fn try_match<'a>(parent: &'a ParentRef<'_>) -> Option<Self::ParentMatch<'a>>;

    /// Check if the given heap-allocated array matches this matcher type.
    ///
    /// The default implementation delegates through
    /// [`try_match_ref`](Self::try_match_ref), but matchers that can answer cheaply
    /// (encoding-id checks, no view construction) should override this directly so
    /// hot callers like `ArrayRef::is::<M>()` don't pay the `try_match_ref` cost.
    fn matches_ref(array: &ArrayRef) -> bool {
        Self::try_match_ref(array).is_some()
    }

    /// Try to match a heap-allocated [`ArrayRef`], returning the matched view type
    /// if successful.
    ///
    /// This is the heap-only fast path: callers that already hold an `ArrayRef`
    /// skip `ParentRef` construction. Implementations typically delegate to
    /// [`ArrayRef::as_typed`](crate::ArrayRef::as_typed).
    fn try_match_ref(array: &ArrayRef) -> Option<Self::RefMatch<'_>>;
}
