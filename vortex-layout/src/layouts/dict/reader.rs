// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright the Vortex contributors

use std::ops::BitAnd;
use std::ops::Range;
use std::sync::Arc;
use std::sync::OnceLock;

use futures::FutureExt;
use futures::TryFutureExt;
use futures::future::BoxFuture;
use futures::try_join;
use vortex_array::ArrayRef;
use vortex_array::IntoArray;
use vortex_array::MaskFuture;
use vortex_array::VortexSessionExecute;
use vortex_array::arrays::DictArray;
use vortex_array::arrays::SharedArray;
use vortex_array::dtype::DType;
use vortex_array::dtype::FieldMask;
use vortex_array::expr::Expression;
use vortex_array::expr::is_root;
use vortex_array::expr::label_is_fallible;
use vortex_array::expr::label_null_sensitive;
use vortex_array::expr::root;
use vortex_array::expr::traversal::NodeExt;
use vortex_array::expr::traversal::Transformed;
use vortex_array::expr::traversal::TraversalOrder;
use vortex_array::optimizer::ArrayOptimizer;
use vortex_array::scalar_fn::is_negative_cost;
use vortex_error::VortexError;
use vortex_error::VortexExpect;
use vortex_error::VortexResult;
use vortex_mask::Mask;
use vortex_session::VortexSession;
use vortex_utils::aliases::dash_map::DashMap;

use super::DictLayout;
use crate::LayoutReader;
use crate::LayoutReaderRef;
use crate::RowSplits;
use crate::SplitRange;
use crate::layouts::SharedArrayFuture;
use crate::segments::SegmentSource;

pub struct DictReader {
    layout: DictLayout,
    name: Arc<str>,
    session: VortexSession,

    /// Length of the values array
    values_len: usize,
    /// Cached dict values array
    values_array: OnceLock<SharedArrayFuture>,
    /// Cache of expression evaluation results on the values array by expression
    values_evals: DashMap<Expression, SharedArrayFuture>,

    values: LayoutReaderRef,
    codes: LayoutReaderRef,
}

impl DictReader {
    pub(super) fn try_new(
        layout: DictLayout,
        name: Arc<str>,
        segment_source: Arc<dyn SegmentSource>,
        session: VortexSession,
        ctx: crate::LayoutReaderContext,
    ) -> VortexResult<Self> {
        let values_len = usize::try_from(layout.values.row_count())?;
        let values = layout.values.new_reader(
            format!("{name}.values").into(),
            Arc::clone(&segment_source),
            &session,
            &ctx,
        )?;
        let codes = layout.codes.new_reader(
            format!("{name}.codes").into(),
            segment_source,
            &session,
            &ctx,
        )?;

        Ok(Self {
            layout,
            name,
            session,
            values_len,
            values_array: Default::default(),
            values_evals: Default::default(),
            values,
            codes,
        })
    }

    fn values_array(&self) -> SharedArrayFuture {
        // We capture the name, so it may be wrong if we re-use the same reader within multiple
        // different parent readers. But that's rare...
        let values_len = self.values_len;
        self.values_array
            .get_or_init(move || {
                self.values
                    .projection_evaluation(
                        &(0..values_len as u64),
                        &root(),
                        MaskFuture::new_true(values_len),
                    )
                    .vortex_expect("must construct dict values array evaluation")
                    .map_err(Arc::new)
                    .map(move |array| Ok(SharedArray::new(array?).into_array()))
                    .boxed()
                    .shared()
            })
            .clone()
    }

    // This is the dict values array without canonicalization, if not already canonical
    fn values_array_uncanonical(&self) -> SharedArrayFuture {
        // We capture the name, so it may be wrong if we re-use the same reader within multiple
        // different parent readers. But that's rare...
        let values_len = self.values_len;
        self.values_array.get().cloned().unwrap_or_else(|| {
            self.values
                .projection_evaluation(
                    &(0..values_len as u64),
                    &root(),
                    MaskFuture::new_true(values_len),
                )
                .vortex_expect("must construct dict values array evaluation")
                .map_err(Arc::new)
                .boxed()
                .shared()
        })
    }

    fn values_eval(&self, expr: Expression) -> SharedArrayFuture {
        // This is unsound since we cannot be sure that all the values are referenced in the query
        // after applying the filter, so if the expression is fallible this might fail when it
        // shouldn't.
        // TODO(joe): fixme

        // Check cache first with read-only lock
        if let Some(fut) = self.values_evals.get(&expr) {
            return fut.clone();
        }

        self.values_evals
            .entry(expr.clone())
            .or_insert_with(|| {
                self.values_array_uncanonical()
                    .map(move |array| {
                        let array = array?.apply(&expr)?;
                        Ok(SharedArray::new(array).into_array())
                    })
                    .boxed()
                    .shared()
            })
            .clone()
    }
}

fn references_root(expr: &Expression) -> bool {
    is_root(expr) || expr.children().iter().any(references_root)
}

/// Split expression into two parts:
///
/// left is the optional outer part that we want to apply to array after
/// canonicalizing.
/// right is the optional inner part that we want to apply to array before
/// canonicalizing.
///
/// We want to push to array only if expression has a negative cost, is
/// infallible and null-insensitive.
fn split_expression_for_pushdown(expr: Expression) -> (Option<Expression>, Option<Expression>) {
    let labelled_expr = expr.clone();
    let fallible = label_is_fallible(&labelled_expr);
    let null_sensitive = label_null_sensitive(&labelled_expr);
    let mut inner: Option<Expression> = None;

    let outer = expr
        .transform_down(|node| {
            if is_negative_cost(node.id())
                && references_root(&node)
                && !fallible.get(&node).copied().unwrap_or(true)
                && !null_sensitive.get(&node).copied().unwrap_or(true)
            {
                inner = Some(node);
                Ok(Transformed {
                    value: root(),
                    changed: true,
                    order: TraversalOrder::Skip,
                })
            } else {
                Ok(Transformed::no(node))
            }
        })
        .vortex_expect("infallible")
        .into_inner();

    let outer = (!is_root(&outer)).then_some(outer);
    (outer, inner)
}

impl LayoutReader for DictReader {
    fn name(&self) -> &Arc<str> {
        &self.name
    }

    fn dtype(&self) -> &DType {
        self.layout.dtype()
    }

    fn row_count(&self) -> u64 {
        self.layout.row_count()
    }

    fn register_splits(
        &self,
        field_mask: &[FieldMask],
        split_range: &SplitRange,
        splits: &mut RowSplits,
    ) -> VortexResult<()> {
        self.codes.register_splits(field_mask, split_range, splits)
    }

    fn pruning_evaluation(
        &self,
        _row_range: &Range<u64>,
        _expr: &Expression,
        mask: Mask,
    ) -> VortexResult<MaskFuture> {
        // NOTE: we can get the values here, convert expression to the codes domain, and push down
        // to the codes child. We don't do that here because:
        // - Reading values only for an approx filter is expensive
        // - In practice, all stats based pruning evaluation should be already done upstream of this dict reader
        Ok(MaskFuture::ready(mask))
    }

    fn filter_evaluation(
        &self,
        row_range: &Range<u64>,
        expr: &Expression,
        mask: MaskFuture,
    ) -> VortexResult<MaskFuture> {
        // TODO(joe): fix up expr partitioning with fallible & null sensitive annotations
        let values_eval = self.values_eval(expr.clone());

        // We register interest on the entire codes row_range for now, there
        // is no straightforward shift into the codes domain we can do to the expression
        // without reading values.
        let codes_eval = self.codes.projection_evaluation(
            row_range,
            &root(),
            MaskFuture::new_true(mask.len()),
        )?;

        let session = self.session.clone();

        Ok(MaskFuture::new(mask.len(), async move {
            // Join on the I/O futures first, before the mask.
            let (codes, values) = try_join!(codes_eval, values_eval.map_err(VortexError::from))?;
            let mask = mask.await?;

            let mut ctx = session.create_execution_ctx();
            let dict_mask = values.take(codes)?.execute::<Mask>(&mut ctx)?;

            Ok(mask.bitand(&dict_mask))
        }))
    }

    fn projection_evaluation(
        &self,
        row_range: &Range<u64>,
        expr: &Expression,
        mask: MaskFuture,
    ) -> VortexResult<BoxFuture<'static, VortexResult<ArrayRef>>> {
        // TODO: fix up expr partitioning with fallible & null sensitive annotations
        let codes_eval = self
            .codes
            .projection_evaluation(row_range, &root(), mask)
            .map_err(|err| err.with_context("While evaluating projection on codes"))?;

        let (expr_outer, expr_inner) = split_expression_for_pushdown(expr.clone());

        let values_eval = if let Some(inner) = expr_inner {
            self.values_eval(inner)
        } else {
            self.values_array()
        };
        let all_values_referenced = self.layout.has_all_values_referenced();
        Ok(async move {
            let (values, codes) = try_join!(values_eval.map_err(VortexError::from), codes_eval)?;

            // SAFETY: Layout was validated at write time.
            //  * The codes dtype is guaranteed to be an integer type from the layout
            //  * The codes child reader ensures the correct dtype.
            //  * The layout stores `all_values_referenced` and if this is malicious then it must
            //    only affect correctness not memory safety.
            let array = unsafe {
                DictArray::new_unchecked(codes, values)
                    .set_all_values_referenced(all_values_referenced)
            }
            .into_array()
            .optimize()?;

            if let Some(expr) = expr_outer {
                array.apply(&expr)
            } else {
                Ok(array)
            }
        }
        .boxed())
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use rstest::rstest;
    use vortex_array::ArrayContext;
    use vortex_array::Canonical;
    use vortex_array::IntoArray as _;
    use vortex_array::LEGACY_SESSION;
    use vortex_array::MaskFuture;
    use vortex_array::VortexSessionExecute;
    use vortex_array::arrays::BoolArray;
    use vortex_array::arrays::StructArray;
    use vortex_array::arrays::VarBinArray;
    use vortex_array::assert_arrays_eq;
    use vortex_array::dtype::DType;
    use vortex_array::dtype::FieldName;
    use vortex_array::dtype::FieldNames;
    use vortex_array::dtype::Nullability;
    use vortex_array::dtype::PType;
    use vortex_array::expr::Expression;
    use vortex_array::expr::byte_length;
    use vortex_array::expr::cast;
    use vortex_array::expr::eq;
    use vortex_array::expr::is_not_null;
    use vortex_array::expr::is_root;
    use vortex_array::expr::like;
    use vortex_array::expr::lit;
    use vortex_array::expr::pack;
    use vortex_array::expr::root;
    use vortex_array::expr::traversal::NodeExt;
    use vortex_array::expr::traversal::Transformed;
    use vortex_array::expr::traversal::TraversalOrder;
    use vortex_array::scalar_fn::session::ScalarFnSession;
    use vortex_array::session::ArraySession;
    use vortex_array::validity::Validity;
    use vortex_error::VortexExpect;
    use vortex_io::runtime::Handle;
    use vortex_io::runtime::single::block_on;
    use vortex_io::session::RuntimeSession;
    use vortex_io::session::RuntimeSessionExt;
    use vortex_session::VortexSession;

    use super::split_expression_for_pushdown;
    use crate::LayoutId;
    use crate::LayoutRef;
    use crate::LayoutStrategy;
    use crate::layouts::dict::writer::DictLayoutOptions;
    use crate::layouts::dict::writer::DictStrategy;
    use crate::layouts::flat::writer::FlatLayoutStrategy;
    use crate::segments::TestSegments;
    use crate::sequence::SequenceId;
    use crate::sequence::SequentialArrayStreamExt;
    use crate::sequence::SequentialStreamAdapter;
    use crate::sequence::SequentialStreamExt;
    use crate::session::LayoutSession;

    // FIXME(ngates): Deprecate the global `runtime::single::block_on` helper and require tests
    // to call `block_on` on an explicit runtime instance.
    fn session_with_handle(handle: Handle) -> VortexSession {
        VortexSession::empty()
            .with::<ArraySession>()
            .with::<LayoutSession>()
            .with::<ScalarFnSession>()
            .with::<RuntimeSession>()
            .with_handle(handle)
    }

    #[test]
    fn reading_nested_packs_works() {
        block_on(|handle| async move {
            let session = session_with_handle(handle);
            let strategy = DictStrategy::new(
                FlatLayoutStrategy::default(),
                FlatLayoutStrategy::default(),
                FlatLayoutStrategy::default(),
                DictLayoutOptions::default(),
            );

            let array = VarBinArray::from_iter(
                [
                    Some("abc"),
                    Some("def"),
                    None,
                    Some("abc"),
                    Some("def"),
                    None,
                    Some("abc"),
                    Some("def"),
                    None,
                ],
                DType::Utf8(Nullability::Nullable),
            )
            .into_array();
            let array_to_write = array.clone();
            let ctx = ArrayContext::empty();
            let segments = Arc::new(TestSegments::default());
            let (ptr, eof) = SequenceId::root().split();
            let layout: LayoutRef = strategy
                .write_stream(
                    ctx,
                    Arc::<TestSegments>::clone(&segments),
                    SequentialStreamAdapter::new(
                        DType::Utf8(Nullability::Nullable),
                        array_to_write.to_array_stream().sequenced(ptr),
                    )
                    .sendable(),
                    eof,
                    &session,
                )
                .await
                .unwrap();

            let expression = pack(
                [(
                    "top",
                    pack([("one", root()), ("two", root())], Nullability::NonNullable),
                )],
                Nullability::NonNullable,
            );
            assert!(layout.encoding_id() == LayoutId::new("vortex.dict"));
            let actual = layout
                .new_reader("".into(), segments, &session, &Default::default())
                .unwrap()
                .projection_evaluation(
                    &(0..layout.row_count()),
                    &expression,
                    MaskFuture::new_true(layout.row_count().try_into().unwrap()),
                )
                .unwrap()
                .await
                .unwrap();
            let expected = StructArray::try_new(
                FieldNames::from([FieldName::from("top")]),
                vec![
                    StructArray::try_new(
                        FieldNames::from([FieldName::from("one"), FieldName::from("two")]),
                        vec![array.clone(), array],
                        9,
                        Validity::NonNullable,
                    )
                    .unwrap()
                    .into_array(),
                ],
                9,
                Validity::NonNullable,
            )
            .unwrap()
            .into_array();
            assert_arrays_eq!(actual, expected);
        })
    }

    #[rstest]
    #[case::all_true_case(
        vec![Some(""), None, Some("")], // Dict values: [""]
        "", // Filter for empty string
        vec![true, false, true], // Expected: nulls excluded, all dict values match
    )]
    #[case::all_false_case(
        vec![Some("x"), None, Some("x")], // Dict values: ["x"]
        "", // Filter for empty string
        vec![false, false, false], // Expected: all false, no dict values match
    )]
    fn shortpathes_filtering(
        #[case] data: Vec<Option<&str>>,
        #[case] filter_value: &str,
        #[case] expected: Vec<bool>,
    ) {
        block_on(|handle| async move {
            let session = session_with_handle(handle);
            let strategy = DictStrategy::new(
                FlatLayoutStrategy::default(),
                FlatLayoutStrategy::default(),
                FlatLayoutStrategy::default(),
                DictLayoutOptions::default(),
            );

            let array =
                VarBinArray::from_iter(data, DType::Utf8(Nullability::Nullable)).into_array();
            let ctx = ArrayContext::empty();
            let segments = Arc::new(TestSegments::default());
            let (ptr, eof) = SequenceId::root().split();
            let layout: LayoutRef = strategy
                .write_stream(
                    ctx,
                    Arc::<TestSegments>::clone(&segments),
                    SequentialStreamAdapter::new(
                        DType::Utf8(Nullability::Nullable),
                        array.to_array_stream().sequenced(ptr),
                    )
                    .sendable(),
                    eof,
                    &session,
                )
                .await
                .unwrap();

            let filter = eq(
                root(),
                lit(vortex_array::scalar::Scalar::utf8(
                    filter_value,
                    Nullability::Nullable,
                )),
            );
            let mask = layout
                .new_reader("".into(), segments, &session, &Default::default())
                .unwrap()
                .filter_evaluation(&(0..3), &filter, MaskFuture::new_true(3))
                .unwrap()
                .await
                .unwrap();

            assert_arrays_eq!(mask.into_array(), BoolArray::from_iter(expected));
        })
    }

    #[test]
    fn reading_is_null_works() {
        block_on(|handle| async move {
            let mut ctx_exec = LEGACY_SESSION.create_execution_ctx();
            let session = session_with_handle(handle);
            let strategy = DictStrategy::new(
                FlatLayoutStrategy::default(),
                FlatLayoutStrategy::default(),
                FlatLayoutStrategy::default(),
                DictLayoutOptions::default(),
            );

            let array = VarBinArray::from_iter(
                [
                    Some("abc"),
                    Some("def"),
                    None,
                    Some("abc"),
                    Some("def"),
                    None,
                    Some("abc"),
                    Some("def"),
                    None,
                ],
                DType::Utf8(Nullability::Nullable),
            )
            .into_array();
            let array_to_write = array.clone();
            let ctx = ArrayContext::empty();

            let segments = Arc::new(TestSegments::default());
            let (ptr, eof) = SequenceId::root().split();
            let layout: LayoutRef = strategy
                .write_stream(
                    ctx,
                    Arc::<TestSegments>::clone(&segments),
                    SequentialStreamAdapter::new(
                        DType::Utf8(Nullability::Nullable),
                        array_to_write.to_array_stream().sequenced(ptr),
                    )
                    .sendable(),
                    eof,
                    &session,
                )
                .await
                .unwrap();

            let expression = is_not_null(root());
            assert_eq!(layout.encoding_id(), LayoutId::new("vortex.dict"));
            let actual = layout
                .new_reader("".into(), segments, &session, &Default::default())
                .unwrap()
                .projection_evaluation(
                    &(0..layout.row_count()),
                    &expression,
                    MaskFuture::new_true(layout.row_count().try_into().unwrap()),
                )
                .unwrap()
                .await
                .unwrap();
            let expected = array
                .validity()
                .unwrap()
                .execute_mask(array.len(), &mut ctx_exec)
                .unwrap()
                .into_array();
            let actual_canonical = actual
                .execute::<Canonical>(&mut ctx_exec)
                .vortex_expect("to_canonical failed")
                .into_array();
            assert_arrays_eq!(actual_canonical, expected);
        })
    }

    fn join_split_expr(initial: &Expression, outer: Option<Expression>, inner: Option<Expression>) {
        let outer_expr = outer.unwrap_or_else(root);
        let inner_expr = inner.unwrap_or_else(root);
        let expected = outer_expr
            .transform_down(|node| {
                if !is_root(&node) {
                    return Ok(Transformed::no(node));
                }
                Ok(Transformed {
                    value: inner_expr.clone(),
                    changed: true,
                    order: TraversalOrder::Skip,
                })
            })
            .vortex_expect("infallible");
        assert_eq!(&expected.into_inner(), initial);
    }

    #[test]
    fn split_expr_cast_root() {
        let (outer, inner) = split_expression_for_pushdown(root());
        assert_eq!(outer, None);
        assert_eq!(inner, None); // Applying root to array is useless work
    }

    #[test]
    fn split_expr_partial_pushdown() {
        let dtype = DType::Primitive(PType::U64, Nullability::NonNullable);
        let expr = cast(byte_length(root()), dtype.clone());
        let (outer, inner) = split_expression_for_pushdown(expr.clone());
        // [0] = cast([1], dtype)
        // [1] = byte_length(root)
        assert_eq!(outer, Some(cast(root(), dtype)));
        assert_eq!(inner, Some(byte_length(root())));
        join_split_expr(&expr, outer, inner);
    }

    #[test]
    fn split_expr_full_pushdown() {
        let expr = byte_length(root());
        let (outer, inner) = split_expression_for_pushdown(expr.clone());
        assert_eq!(outer, None);
        assert_eq!(inner, Some(byte_length(root())));
        join_split_expr(&expr, outer, inner);
    }

    #[test]
    fn split_expr_no_pushdown() {
        // We can push down lit(), but it we replace
        // lit() with root(), the semantics change.
        let expr = like(root(), lit(1u64));
        let (outer, inner) = split_expression_for_pushdown(expr.clone());
        assert_eq!(outer, Some(expr.clone()));
        assert_eq!(inner, None);
        join_split_expr(&expr, outer, inner);
    }
}
