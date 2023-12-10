use std::fmt;
use std::iter::once;

use rustc_arena::TypedArena;
use rustc_data_structures::captures::Captures;
use rustc_hir::def_id::DefId;
use rustc_hir::{HirId, RangeEnd};
use rustc_index::Idx;
use rustc_index::IndexVec;
use rustc_middle::middle::stability::EvalResult;
use rustc_middle::mir;
use rustc_middle::mir::interpret::Scalar;
use rustc_middle::thir::{FieldPat, Pat, PatKind, PatRange, PatRangeBoundary};
use rustc_middle::ty::layout::IntegerExt;
use rustc_middle::ty::{self, Ty, TyCtxt, VariantDef};
use rustc_span::{Span, DUMMY_SP};
use rustc_target::abi::{FieldIdx, Integer, VariantIdx, FIRST_VARIANT};
use smallvec::SmallVec;

use crate::constructor::{
    Constructor, ConstructorSet, IntRange, MaybeInfiniteInt, OpaqueId, Slice, SliceKind,
    VariantVisibility,
};
use crate::pat::{DeconstructedPat, WitnessPat};

use Constructor::*;

pub struct MatchCheckCtxt<'p, 'tcx> {
    pub tcx: TyCtxt<'tcx>,
    /// The module in which the match occurs. This is necessary for
    /// checking inhabited-ness of types because whether a type is (visibly)
    /// inhabited can depend on whether it was defined in the current module or
    /// not. E.g., `struct Foo { _private: ! }` cannot be seen to be empty
    /// outside its module and should not be matchable with an empty match statement.
    pub module: DefId,
    pub param_env: ty::ParamEnv<'tcx>,
    pub pattern_arena: &'p TypedArena<DeconstructedPat<'p, 'tcx>>,
    /// Lint level at the match.
    pub match_lint_level: HirId,
    /// The span of the whole match, if applicable.
    pub whole_match_span: Option<Span>,
    /// Span of the scrutinee.
    pub scrut_span: Span,
    /// Only produce `NON_EXHAUSTIVE_OMITTED_PATTERNS` lint on refutable patterns.
    pub refutable: bool,
    /// Whether the data at the scrutinee is known to be valid. This is false if the scrutinee comes
    /// from a union field, a pointer deref, or a reference deref (pending opsem decisions).
    pub known_valid_scrutinee: bool,
}

impl<'p, 'tcx> MatchCheckCtxt<'p, 'tcx> {
    pub(super) fn is_uninhabited(&self, ty: Ty<'tcx>) -> bool {
        !ty.is_inhabited_from(self.tcx, self.module, self.param_env)
    }

    /// Returns whether the given type is an enum from another crate declared `#[non_exhaustive]`.
    pub fn is_foreign_non_exhaustive_enum(&self, ty: Ty<'tcx>) -> bool {
        match ty.kind() {
            ty::Adt(def, ..) => {
                def.is_enum() && def.is_variant_list_non_exhaustive() && !def.did().is_local()
            }
            _ => false,
        }
    }

    pub(crate) fn alloc_wildcard_slice(
        &self,
        tys: impl IntoIterator<Item = Ty<'tcx>>,
    ) -> &'p [DeconstructedPat<'p, 'tcx>] {
        self.pattern_arena
            .alloc_from_iter(tys.into_iter().map(|ty| DeconstructedPat::wildcard(ty, DUMMY_SP)))
    }

    // In the cases of either a `#[non_exhaustive]` field list or a non-public field, we hide
    // uninhabited fields in order not to reveal the uninhabitedness of the whole variant.
    // This lists the fields we keep along with their types.
    pub(crate) fn list_variant_nonhidden_fields<'a>(
        &'a self,
        ty: Ty<'tcx>,
        variant: &'a VariantDef,
    ) -> impl Iterator<Item = (FieldIdx, Ty<'tcx>)> + Captures<'p> + Captures<'a> {
        let cx = self;
        let ty::Adt(adt, args) = ty.kind() else { bug!() };
        // Whether we must not match the fields of this variant exhaustively.
        let is_non_exhaustive = variant.is_field_list_non_exhaustive() && !adt.did().is_local();

        variant.fields.iter().enumerate().filter_map(move |(i, field)| {
            let ty = field.ty(cx.tcx, args);
            // `field.ty()` doesn't normalize after substituting.
            let ty = cx.tcx.normalize_erasing_regions(cx.param_env, ty);
            let is_visible = adt.is_enum() || field.vis.is_accessible_from(cx.module, cx.tcx);
            let is_uninhabited = cx.tcx.features().exhaustive_patterns && cx.is_uninhabited(ty);

            if is_uninhabited && (!is_visible || is_non_exhaustive) {
                None
            } else {
                Some((FieldIdx::new(i), ty))
            }
        })
    }

    pub(crate) fn variant_index_for_adt(
        ctor: &Constructor<'tcx>,
        adt: ty::AdtDef<'tcx>,
    ) -> VariantIdx {
        match *ctor {
            Variant(idx) => idx,
            Single => {
                assert!(!adt.is_enum());
                FIRST_VARIANT
            }
            _ => bug!("bad constructor {:?} for adt {:?}", ctor, adt),
        }
    }

    /// Creates a new list of wildcard fields for a given constructor. The result must have a length
    /// of `ctor.arity()`.
    #[instrument(level = "trace", skip(self))]
    pub(crate) fn ctor_wildcard_fields(
        &self,
        ctor: &Constructor<'tcx>,
        ty: Ty<'tcx>,
    ) -> &'p [DeconstructedPat<'p, 'tcx>] {
        let cx = self;
        match ctor {
            Single | Variant(_) => match ty.kind() {
                ty::Tuple(fs) => cx.alloc_wildcard_slice(fs.iter()),
                ty::Ref(_, rty, _) => cx.alloc_wildcard_slice(once(*rty)),
                ty::Adt(adt, args) => {
                    if adt.is_box() {
                        // The only legal patterns of type `Box` (outside `std`) are `_` and box
                        // patterns. If we're here we can assume this is a box pattern.
                        cx.alloc_wildcard_slice(once(args.type_at(0)))
                    } else {
                        let variant =
                            &adt.variant(MatchCheckCtxt::variant_index_for_adt(&ctor, *adt));
                        let tys = cx.list_variant_nonhidden_fields(ty, variant).map(|(_, ty)| ty);
                        cx.alloc_wildcard_slice(tys)
                    }
                }
                _ => bug!("Unexpected type for `Single` constructor: {:?}", ty),
            },
            Slice(slice) => match *ty.kind() {
                ty::Slice(ty) | ty::Array(ty, _) => {
                    let arity = slice.arity();
                    cx.alloc_wildcard_slice((0..arity).map(|_| ty))
                }
                _ => bug!("bad slice pattern {:?} {:?}", ctor, ty),
            },
            Bool(..)
            | IntRange(..)
            | F32Range(..)
            | F64Range(..)
            | Str(..)
            | Opaque(..)
            | NonExhaustive
            | Hidden
            | Missing { .. }
            | Wildcard => &[],
            Or => {
                bug!("called `Fields::wildcards` on an `Or` ctor")
            }
        }
    }

    /// The number of fields for this constructor. This must be kept in sync with
    /// `Fields::wildcards`.
    pub(crate) fn ctor_arity(&self, ctor: &Constructor<'tcx>, ty: Ty<'tcx>) -> usize {
        match ctor {
            Single | Variant(_) => match ty.kind() {
                ty::Tuple(fs) => fs.len(),
                ty::Ref(..) => 1,
                ty::Adt(adt, ..) => {
                    if adt.is_box() {
                        // The only legal patterns of type `Box` (outside `std`) are `_` and box
                        // patterns. If we're here we can assume this is a box pattern.
                        1
                    } else {
                        let variant =
                            &adt.variant(MatchCheckCtxt::variant_index_for_adt(&ctor, *adt));
                        self.list_variant_nonhidden_fields(ty, variant).count()
                    }
                }
                _ => bug!("Unexpected type for `Single` constructor: {:?}", ty),
            },
            Slice(slice) => slice.arity(),
            Bool(..)
            | IntRange(..)
            | F32Range(..)
            | F64Range(..)
            | Str(..)
            | Opaque(..)
            | NonExhaustive
            | Hidden
            | Missing { .. }
            | Wildcard => 0,
            Or => bug!("The `Or` constructor doesn't have a fixed arity"),
        }
    }

    /// Creates a set that represents all the constructors of `ty`.
    ///
    /// See [`crate::constructor`] for considerations of emptiness.
    #[instrument(level = "debug", skip(self), ret)]
    pub fn ctors_for_ty(&self, ty: Ty<'tcx>) -> ConstructorSet {
        let cx = self;
        let make_range = |start, end| {
            IntRange::from_range(
                MaybeInfiniteInt::new_finite(cx.tcx, ty, start),
                MaybeInfiniteInt::new_finite(cx.tcx, ty, end),
                RangeEnd::Included,
            )
        };
        // This determines the set of all possible constructors for the type `ty`. For numbers,
        // arrays and slices we use ranges and variable-length slices when appropriate.
        match ty.kind() {
            ty::Bool => ConstructorSet::Bool,
            ty::Char => {
                // The valid Unicode Scalar Value ranges.
                ConstructorSet::Integers {
                    range_1: make_range('\u{0000}' as u128, '\u{D7FF}' as u128),
                    range_2: Some(make_range('\u{E000}' as u128, '\u{10FFFF}' as u128)),
                }
            }
            &ty::Int(ity) => {
                let range = if ty.is_ptr_sized_integral() {
                    // The min/max values of `isize` are not allowed to be observed.
                    IntRange {
                        lo: MaybeInfiniteInt::NegInfinity,
                        hi: MaybeInfiniteInt::PosInfinity,
                    }
                } else {
                    let bits = Integer::from_int_ty(&cx.tcx, ity).size().bits() as u128;
                    let min = 1u128 << (bits - 1);
                    let max = min - 1;
                    make_range(min, max)
                };
                ConstructorSet::Integers { range_1: range, range_2: None }
            }
            &ty::Uint(uty) => {
                let range = if ty.is_ptr_sized_integral() {
                    // The max value of `usize` is not allowed to be observed.
                    let lo = MaybeInfiniteInt::new_finite(cx.tcx, ty, 0);
                    IntRange { lo, hi: MaybeInfiniteInt::PosInfinity }
                } else {
                    let size = Integer::from_uint_ty(&cx.tcx, uty).size();
                    let max = size.truncate(u128::MAX);
                    make_range(0, max)
                };
                ConstructorSet::Integers { range_1: range, range_2: None }
            }
            ty::Slice(sub_ty) => ConstructorSet::Slice {
                array_len: None,
                subtype_is_empty: cx.is_uninhabited(*sub_ty),
            },
            ty::Array(sub_ty, len) => {
                // We treat arrays of a constant but unknown length like slices.
                ConstructorSet::Slice {
                    array_len: len.try_eval_target_usize(cx.tcx, cx.param_env).map(|l| l as usize),
                    subtype_is_empty: cx.is_uninhabited(*sub_ty),
                }
            }
            ty::Adt(def, args) if def.is_enum() => {
                let is_declared_nonexhaustive = cx.is_foreign_non_exhaustive_enum(ty);
                if def.variants().is_empty() && !is_declared_nonexhaustive {
                    ConstructorSet::NoConstructors
                } else {
                    let mut variants =
                        IndexVec::from_elem(VariantVisibility::Visible, def.variants());
                    for (idx, v) in def.variants().iter_enumerated() {
                        let variant_def_id = def.variant(idx).def_id;
                        // Visibly uninhabited variants.
                        let is_inhabited = v
                            .inhabited_predicate(cx.tcx, *def)
                            .instantiate(cx.tcx, args)
                            .apply(cx.tcx, cx.param_env, cx.module);
                        // Variants that depend on a disabled unstable feature.
                        let is_unstable = matches!(
                            cx.tcx.eval_stability(variant_def_id, None, DUMMY_SP, None),
                            EvalResult::Deny { .. }
                        );
                        // Foreign `#[doc(hidden)]` variants.
                        let is_doc_hidden =
                            cx.tcx.is_doc_hidden(variant_def_id) && !variant_def_id.is_local();
                        let visibility = if !is_inhabited {
                            // FIXME: handle empty+hidden
                            VariantVisibility::Empty
                        } else if is_unstable || is_doc_hidden {
                            VariantVisibility::Hidden
                        } else {
                            VariantVisibility::Visible
                        };
                        variants[idx] = visibility;
                    }

                    ConstructorSet::Variants { variants, non_exhaustive: is_declared_nonexhaustive }
                }
            }
            ty::Adt(..) | ty::Tuple(..) | ty::Ref(..) => {
                ConstructorSet::Single { empty: cx.is_uninhabited(ty) }
            }
            ty::Never => ConstructorSet::NoConstructors,
            // This type is one for which we cannot list constructors, like `str` or `f64`.
            // FIXME(Nadrieril): which of these are actually allowed?
            ty::Float(_)
            | ty::Str
            | ty::Foreign(_)
            | ty::RawPtr(_)
            | ty::FnDef(_, _)
            | ty::FnPtr(_)
            | ty::Dynamic(_, _, _)
            | ty::Closure(_, _)
            | ty::Coroutine(_, _, _)
            | ty::Alias(_, _)
            | ty::Param(_)
            | ty::Error(_) => ConstructorSet::Unlistable,
            ty::CoroutineWitness(_, _) | ty::Bound(_, _) | ty::Placeholder(_) | ty::Infer(_) => {
                bug!("Encountered unexpected type in `ConstructorSet::for_ty`: {ty:?}")
            }
        }
    }

    pub(crate) fn lower_pat_range_bdy(
        &self,
        bdy: PatRangeBoundary<'tcx>,
        ty: Ty<'tcx>,
    ) -> MaybeInfiniteInt {
        match bdy {
            PatRangeBoundary::NegInfinity => MaybeInfiniteInt::NegInfinity,
            PatRangeBoundary::Finite(value) => {
                let bits = value.eval_bits(self.tcx, self.param_env);
                MaybeInfiniteInt::new_finite(self.tcx, ty, bits)
            }
            PatRangeBoundary::PosInfinity => MaybeInfiniteInt::PosInfinity,
        }
    }

    /// Note: the input patterns must have been lowered through
    /// `rustc_mir_build::thir::pattern::check_match::MatchVisitor::lower_pattern`.
    pub fn lower_pat(&self, pat: &Pat<'tcx>) -> DeconstructedPat<'p, 'tcx> {
        let singleton = |pat| std::slice::from_ref(self.pattern_arena.alloc(pat));
        let cx = self;
        let ctor;
        let fields: &[_];
        match &pat.kind {
            PatKind::AscribeUserType { subpattern, .. }
            | PatKind::InlineConstant { subpattern, .. } => return self.lower_pat(subpattern),
            PatKind::Binding { subpattern: Some(subpat), .. } => return self.lower_pat(subpat),
            PatKind::Binding { subpattern: None, .. } | PatKind::Wild => {
                ctor = Wildcard;
                fields = &[];
            }
            PatKind::Deref { subpattern } => {
                ctor = Single;
                fields = singleton(self.lower_pat(subpattern));
            }
            PatKind::Leaf { subpatterns } | PatKind::Variant { subpatterns, .. } => {
                match pat.ty.kind() {
                    ty::Tuple(fs) => {
                        ctor = Single;
                        let mut wilds: SmallVec<[_; 2]> =
                            fs.iter().map(|ty| DeconstructedPat::wildcard(ty, pat.span)).collect();
                        for pat in subpatterns {
                            wilds[pat.field.index()] = self.lower_pat(&pat.pattern);
                        }
                        fields = cx.pattern_arena.alloc_from_iter(wilds);
                    }
                    ty::Adt(adt, args) if adt.is_box() => {
                        // The only legal patterns of type `Box` (outside `std`) are `_` and box
                        // patterns. If we're here we can assume this is a box pattern.
                        // FIXME(Nadrieril): A `Box` can in theory be matched either with `Box(_,
                        // _)` or a box pattern. As a hack to avoid an ICE with the former, we
                        // ignore other fields than the first one. This will trigger an error later
                        // anyway.
                        // See https://github.com/rust-lang/rust/issues/82772 ,
                        // explanation: https://github.com/rust-lang/rust/pull/82789#issuecomment-796921977
                        // The problem is that we can't know from the type whether we'll match
                        // normally or through box-patterns. We'll have to figure out a proper
                        // solution when we introduce generalized deref patterns. Also need to
                        // prevent mixing of those two options.
                        let pattern = subpatterns.into_iter().find(|pat| pat.field.index() == 0);
                        let pat = if let Some(pat) = pattern {
                            self.lower_pat(&pat.pattern)
                        } else {
                            DeconstructedPat::wildcard(args.type_at(0), pat.span)
                        };
                        ctor = Single;
                        fields = singleton(pat);
                    }
                    ty::Adt(adt, _) => {
                        ctor = match pat.kind {
                            PatKind::Leaf { .. } => Single,
                            PatKind::Variant { variant_index, .. } => Variant(variant_index),
                            _ => bug!(),
                        };
                        let variant =
                            &adt.variant(MatchCheckCtxt::variant_index_for_adt(&ctor, *adt));
                        // For each field in the variant, we store the relevant index into `self.fields` if any.
                        let mut field_id_to_id: Vec<Option<usize>> =
                            (0..variant.fields.len()).map(|_| None).collect();
                        let tys = cx
                            .list_variant_nonhidden_fields(pat.ty, variant)
                            .enumerate()
                            .map(|(i, (field, ty))| {
                                field_id_to_id[field.index()] = Some(i);
                                ty
                            });
                        let mut wilds: SmallVec<[_; 2]> =
                            tys.map(|ty| DeconstructedPat::wildcard(ty, pat.span)).collect();
                        for pat in subpatterns {
                            if let Some(i) = field_id_to_id[pat.field.index()] {
                                wilds[i] = self.lower_pat(&pat.pattern);
                            }
                        }
                        fields = cx.pattern_arena.alloc_from_iter(wilds);
                    }
                    _ => bug!("pattern has unexpected type: pat: {:?}, ty: {:?}", pat, pat.ty),
                }
            }
            PatKind::Constant { value } => {
                match pat.ty.kind() {
                    ty::Bool => {
                        ctor = match value.try_eval_bool(cx.tcx, cx.param_env) {
                            Some(b) => Bool(b),
                            None => Opaque(OpaqueId::new()),
                        };
                        fields = &[];
                    }
                    ty::Char | ty::Int(_) | ty::Uint(_) => {
                        ctor = match value.try_eval_bits(cx.tcx, cx.param_env) {
                            Some(bits) => IntRange(IntRange::from_bits(cx.tcx, pat.ty, bits)),
                            None => Opaque(OpaqueId::new()),
                        };
                        fields = &[];
                    }
                    ty::Float(ty::FloatTy::F32) => {
                        ctor = match value.try_eval_bits(cx.tcx, cx.param_env) {
                            Some(bits) => {
                                use rustc_apfloat::Float;
                                let value = rustc_apfloat::ieee::Single::from_bits(bits);
                                F32Range(value, value, RangeEnd::Included)
                            }
                            None => Opaque(OpaqueId::new()),
                        };
                        fields = &[];
                    }
                    ty::Float(ty::FloatTy::F64) => {
                        ctor = match value.try_eval_bits(cx.tcx, cx.param_env) {
                            Some(bits) => {
                                use rustc_apfloat::Float;
                                let value = rustc_apfloat::ieee::Double::from_bits(bits);
                                F64Range(value, value, RangeEnd::Included)
                            }
                            None => Opaque(OpaqueId::new()),
                        };
                        fields = &[];
                    }
                    ty::Ref(_, t, _) if t.is_str() => {
                        // We want a `&str` constant to behave like a `Deref` pattern, to be compatible
                        // with other `Deref` patterns. This could have been done in `const_to_pat`,
                        // but that causes issues with the rest of the matching code.
                        // So here, the constructor for a `"foo"` pattern is `&` (represented by
                        // `Single`), and has one field. That field has constructor `Str(value)` and no
                        // fields.
                        // Note: `t` is `str`, not `&str`.
                        let subpattern = DeconstructedPat::new(Str(*value), &[], *t, pat.span);
                        ctor = Single;
                        fields = singleton(subpattern)
                    }
                    // All constants that can be structurally matched have already been expanded
                    // into the corresponding `Pat`s by `const_to_pat`. Constants that remain are
                    // opaque.
                    _ => {
                        ctor = Opaque(OpaqueId::new());
                        fields = &[];
                    }
                }
            }
            PatKind::Range(patrange) => {
                let PatRange { lo, hi, end, .. } = patrange.as_ref();
                let ty = pat.ty;
                ctor = match ty.kind() {
                    ty::Char | ty::Int(_) | ty::Uint(_) => {
                        let lo = cx.lower_pat_range_bdy(*lo, ty);
                        let hi = cx.lower_pat_range_bdy(*hi, ty);
                        IntRange(IntRange::from_range(lo, hi, *end))
                    }
                    ty::Float(fty) => {
                        use rustc_apfloat::Float;
                        let lo = lo.as_finite().map(|c| c.eval_bits(cx.tcx, cx.param_env));
                        let hi = hi.as_finite().map(|c| c.eval_bits(cx.tcx, cx.param_env));
                        match fty {
                            ty::FloatTy::F32 => {
                                use rustc_apfloat::ieee::Single;
                                let lo = lo.map(Single::from_bits).unwrap_or(-Single::INFINITY);
                                let hi = hi.map(Single::from_bits).unwrap_or(Single::INFINITY);
                                F32Range(lo, hi, *end)
                            }
                            ty::FloatTy::F64 => {
                                use rustc_apfloat::ieee::Double;
                                let lo = lo.map(Double::from_bits).unwrap_or(-Double::INFINITY);
                                let hi = hi.map(Double::from_bits).unwrap_or(Double::INFINITY);
                                F64Range(lo, hi, *end)
                            }
                        }
                    }
                    _ => bug!("invalid type for range pattern: {}", ty),
                };
                fields = &[];
            }
            PatKind::Array { prefix, slice, suffix } | PatKind::Slice { prefix, slice, suffix } => {
                let array_len = match pat.ty.kind() {
                    ty::Array(_, length) => {
                        Some(length.eval_target_usize(cx.tcx, cx.param_env) as usize)
                    }
                    ty::Slice(_) => None,
                    _ => span_bug!(pat.span, "bad ty {:?} for slice pattern", pat.ty),
                };
                let kind = if slice.is_some() {
                    SliceKind::VarLen(prefix.len(), suffix.len())
                } else {
                    SliceKind::FixedLen(prefix.len() + suffix.len())
                };
                ctor = Slice(Slice::new(array_len, kind));
                fields = cx.pattern_arena.alloc_from_iter(
                    prefix.iter().chain(suffix.iter()).map(|p| self.lower_pat(&*p)),
                )
            }
            PatKind::Or { .. } => {
                ctor = Or;
                let pats = expand_or_pat(pat);
                fields =
                    cx.pattern_arena.alloc_from_iter(pats.into_iter().map(|p| self.lower_pat(p)))
            }
            PatKind::Never => {
                // FIXME(never_patterns): handle `!` in exhaustiveness. This is a sane default
                // in the meantime.
                ctor = Wildcard;
                fields = &[];
            }
            PatKind::Error(_) => {
                ctor = Opaque(OpaqueId::new());
                fields = &[];
            }
        }
        DeconstructedPat::new(ctor, fields, pat.ty, pat.span)
    }

    /// Convert back to a `thir::PatRangeBoundary` for diagnostic purposes.
    /// Note: it is possible to get `isize/usize::MAX+1` here, as explained in the doc for
    /// [`IntRange::split`]. This cannot be represented as a `Const`, so we represent it with
    /// `PosInfinity`.
    pub(crate) fn hoist_pat_range_bdy(
        &self,
        miint: MaybeInfiniteInt,
        ty: Ty<'tcx>,
    ) -> PatRangeBoundary<'tcx> {
        use MaybeInfiniteInt::*;
        let tcx = self.tcx;
        match miint {
            NegInfinity => PatRangeBoundary::NegInfinity,
            Finite(x) => {
                let bias = MaybeInfiniteInt::signed_bias(tcx, ty);
                let bits = x ^ bias;
                let size = ty.primitive_size(tcx);
                match Scalar::try_from_uint(bits, size) {
                    Some(scalar) => {
                        let value = mir::Const::from_scalar(tcx, scalar, ty);
                        PatRangeBoundary::Finite(value)
                    }
                    // The value doesn't fit. Since `x >= 0` and 0 always encodes the minimum value
                    // for a type, the problem isn't that the value is too small. So it must be too
                    // large.
                    None => PatRangeBoundary::PosInfinity,
                }
            }
            JustAfterMax | PosInfinity => PatRangeBoundary::PosInfinity,
        }
    }

    /// Whether the range denotes the fictitious values before `isize::MIN` or after
    /// `usize::MAX`/`isize::MAX` (see doc of [`IntRange::split`] for why these exist).
    pub fn is_range_beyond_boundaries(&self, range: &IntRange, ty: Ty<'tcx>) -> bool {
        ty.is_ptr_sized_integral() && {
            // The two invalid ranges are `NegInfinity..isize::MIN` (represented as
            // `NegInfinity..0`), and `{u,i}size::MAX+1..PosInfinity`. `hoist_pat_range_bdy`
            // converts `MAX+1` to `PosInfinity`, and we couldn't have `PosInfinity` in `range.lo`
            // otherwise.
            let lo = self.hoist_pat_range_bdy(range.lo, ty);
            matches!(lo, PatRangeBoundary::PosInfinity)
                || matches!(range.hi, MaybeInfiniteInt::Finite(0))
        }
    }

    /// Convert back to a `thir::Pat` for diagnostic purposes.
    pub(crate) fn hoist_pat_range(&self, range: &IntRange, ty: Ty<'tcx>) -> Pat<'tcx> {
        use MaybeInfiniteInt::*;
        let cx = self;
        let kind = if matches!((range.lo, range.hi), (NegInfinity, PosInfinity)) {
            PatKind::Wild
        } else if range.is_singleton() {
            let lo = cx.hoist_pat_range_bdy(range.lo, ty);
            let value = lo.as_finite().unwrap();
            PatKind::Constant { value }
        } else {
            // We convert to an inclusive range for diagnostics.
            let mut end = RangeEnd::Included;
            let mut lo = cx.hoist_pat_range_bdy(range.lo, ty);
            if matches!(lo, PatRangeBoundary::PosInfinity) {
                // The only reason to get `PosInfinity` here is the special case where
                // `hoist_pat_range_bdy` found `{u,i}size::MAX+1`. So the range denotes the
                // fictitious values after `{u,i}size::MAX` (see [`IntRange::split`] for why we do
                // this). We show this to the user as `usize::MAX..` which is slightly incorrect but
                // probably clear enough.
                let c = ty.numeric_max_val(cx.tcx).unwrap();
                let value = mir::Const::from_ty_const(c, cx.tcx);
                lo = PatRangeBoundary::Finite(value);
            }
            let hi = if matches!(range.hi, Finite(0)) {
                // The range encodes `..ty::MIN`, so we can't convert it to an inclusive range.
                end = RangeEnd::Excluded;
                range.hi
            } else {
                range.hi.minus_one()
            };
            let hi = cx.hoist_pat_range_bdy(hi, ty);
            PatKind::Range(Box::new(PatRange { lo, hi, end, ty }))
        };

        Pat { ty, span: DUMMY_SP, kind }
    }
    /// Convert back to a `thir::Pat` for diagnostic purposes. This panics for patterns that don't
    /// appear in diagnostics, like float ranges.
    pub fn hoist_witness_pat(&self, pat: &WitnessPat<'tcx>) -> Pat<'tcx> {
        let cx = self;
        let is_wildcard = |pat: &Pat<'_>| matches!(pat.kind, PatKind::Wild);
        let mut subpatterns = pat.iter_fields().map(|p| Box::new(cx.hoist_witness_pat(p)));
        let kind = match pat.ctor() {
            Bool(b) => PatKind::Constant { value: mir::Const::from_bool(cx.tcx, *b) },
            IntRange(range) => return self.hoist_pat_range(range, pat.ty()),
            Single | Variant(_) => match pat.ty().kind() {
                ty::Tuple(..) => PatKind::Leaf {
                    subpatterns: subpatterns
                        .enumerate()
                        .map(|(i, pattern)| FieldPat { field: FieldIdx::new(i), pattern })
                        .collect(),
                },
                ty::Adt(adt_def, _) if adt_def.is_box() => {
                    // Without `box_patterns`, the only legal pattern of type `Box` is `_` (outside
                    // of `std`). So this branch is only reachable when the feature is enabled and
                    // the pattern is a box pattern.
                    PatKind::Deref { subpattern: subpatterns.next().unwrap() }
                }
                ty::Adt(adt_def, args) => {
                    let variant_index =
                        MatchCheckCtxt::variant_index_for_adt(&pat.ctor(), *adt_def);
                    let variant = &adt_def.variant(variant_index);
                    let subpatterns = cx
                        .list_variant_nonhidden_fields(pat.ty(), variant)
                        .zip(subpatterns)
                        .map(|((field, _ty), pattern)| FieldPat { field, pattern })
                        .collect();

                    if adt_def.is_enum() {
                        PatKind::Variant { adt_def: *adt_def, args, variant_index, subpatterns }
                    } else {
                        PatKind::Leaf { subpatterns }
                    }
                }
                // Note: given the expansion of `&str` patterns done in `expand_pattern`, we should
                // be careful to reconstruct the correct constant pattern here. However a string
                // literal pattern will never be reported as a non-exhaustiveness witness, so we
                // ignore this issue.
                ty::Ref(..) => PatKind::Deref { subpattern: subpatterns.next().unwrap() },
                _ => bug!("unexpected ctor for type {:?} {:?}", pat.ctor(), pat.ty()),
            },
            Slice(slice) => {
                match slice.kind {
                    SliceKind::FixedLen(_) => PatKind::Slice {
                        prefix: subpatterns.collect(),
                        slice: None,
                        suffix: Box::new([]),
                    },
                    SliceKind::VarLen(prefix, _) => {
                        let mut subpatterns = subpatterns.peekable();
                        let mut prefix: Vec<_> = subpatterns.by_ref().take(prefix).collect();
                        if slice.array_len.is_some() {
                            // Improves diagnostics a bit: if the type is a known-size array, instead
                            // of reporting `[x, _, .., _, y]`, we prefer to report `[x, .., y]`.
                            // This is incorrect if the size is not known, since `[_, ..]` captures
                            // arrays of lengths `>= 1` whereas `[..]` captures any length.
                            while !prefix.is_empty() && is_wildcard(prefix.last().unwrap()) {
                                prefix.pop();
                            }
                            while subpatterns.peek().is_some()
                                && is_wildcard(subpatterns.peek().unwrap())
                            {
                                subpatterns.next();
                            }
                        }
                        let suffix: Box<[_]> = subpatterns.collect();
                        let wild = Pat::wildcard_from_ty(pat.ty());
                        PatKind::Slice {
                            prefix: prefix.into_boxed_slice(),
                            slice: Some(Box::new(wild)),
                            suffix,
                        }
                    }
                }
            }
            &Str(value) => PatKind::Constant { value },
            Wildcard | NonExhaustive | Hidden => PatKind::Wild,
            Missing { .. } => bug!(
                "trying to convert a `Missing` constructor into a `Pat`; this is probably a bug,
                `Missing` should have been processed in `apply_constructors`"
            ),
            F32Range(..) | F64Range(..) | Opaque(..) | Or => {
                bug!("can't convert to pattern: {:?}", pat)
            }
        };

        Pat { ty: pat.ty(), span: DUMMY_SP, kind }
    }

    /// Best-effort `Debug` implementation.
    pub(crate) fn debug_pat(
        f: &mut fmt::Formatter<'_>,
        pat: &DeconstructedPat<'p, 'tcx>,
    ) -> fmt::Result {
        let mut first = true;
        let mut start_or_continue = |s| {
            if first {
                first = false;
                ""
            } else {
                s
            }
        };
        let mut start_or_comma = || start_or_continue(", ");

        match pat.ctor() {
            Single | Variant(_) => match pat.ty().kind() {
                ty::Adt(def, _) if def.is_box() => {
                    // Without `box_patterns`, the only legal pattern of type `Box` is `_` (outside
                    // of `std`). So this branch is only reachable when the feature is enabled and
                    // the pattern is a box pattern.
                    let subpattern = pat.iter_fields().next().unwrap();
                    write!(f, "box {subpattern:?}")
                }
                ty::Adt(..) | ty::Tuple(..) => {
                    let variant = match pat.ty().kind() {
                        ty::Adt(adt, _) => Some(
                            adt.variant(MatchCheckCtxt::variant_index_for_adt(pat.ctor(), *adt)),
                        ),
                        ty::Tuple(_) => None,
                        _ => unreachable!(),
                    };

                    if let Some(variant) = variant {
                        write!(f, "{}", variant.name)?;
                    }

                    // Without `cx`, we can't know which field corresponds to which, so we can't
                    // get the names of the fields. Instead we just display everything as a tuple
                    // struct, which should be good enough.
                    write!(f, "(")?;
                    for p in pat.iter_fields() {
                        write!(f, "{}", start_or_comma())?;
                        write!(f, "{p:?}")?;
                    }
                    write!(f, ")")
                }
                // Note: given the expansion of `&str` patterns done in `expand_pattern`, we should
                // be careful to detect strings here. However a string literal pattern will never
                // be reported as a non-exhaustiveness witness, so we can ignore this issue.
                ty::Ref(_, _, mutbl) => {
                    let subpattern = pat.iter_fields().next().unwrap();
                    write!(f, "&{}{:?}", mutbl.prefix_str(), subpattern)
                }
                _ => write!(f, "_"),
            },
            Slice(slice) => {
                let mut subpatterns = pat.iter_fields();
                write!(f, "[")?;
                match slice.kind {
                    SliceKind::FixedLen(_) => {
                        for p in subpatterns {
                            write!(f, "{}{:?}", start_or_comma(), p)?;
                        }
                    }
                    SliceKind::VarLen(prefix_len, _) => {
                        for p in subpatterns.by_ref().take(prefix_len) {
                            write!(f, "{}{:?}", start_or_comma(), p)?;
                        }
                        write!(f, "{}", start_or_comma())?;
                        write!(f, "..")?;
                        for p in subpatterns {
                            write!(f, "{}{:?}", start_or_comma(), p)?;
                        }
                    }
                }
                write!(f, "]")
            }
            Bool(b) => write!(f, "{b}"),
            // Best-effort, will render signed ranges incorrectly
            IntRange(range) => write!(f, "{range:?}"),
            F32Range(lo, hi, end) => write!(f, "{lo}{end}{hi}"),
            F64Range(lo, hi, end) => write!(f, "{lo}{end}{hi}"),
            Str(value) => write!(f, "{value}"),
            Opaque(..) => write!(f, "<constant pattern>"),
            Or => {
                for pat in pat.iter_fields() {
                    write!(f, "{}{:?}", start_or_continue(" | "), pat)?;
                }
                Ok(())
            }
            Wildcard | Missing { .. } | NonExhaustive | Hidden => write!(f, "_ : {:?}", pat.ty()),
        }
    }
}

/// Recursively expand this pattern into its subpatterns. Only useful for or-patterns.
fn expand_or_pat<'p, 'tcx>(pat: &'p Pat<'tcx>) -> Vec<&'p Pat<'tcx>> {
    fn expand<'p, 'tcx>(pat: &'p Pat<'tcx>, vec: &mut Vec<&'p Pat<'tcx>>) {
        if let PatKind::Or { pats } = &pat.kind {
            for pat in pats.iter() {
                expand(pat, vec);
            }
        } else {
            vec.push(pat)
        }
    }

    let mut pats = Vec::new();
    expand(pat, &mut pats);
    pats
}
