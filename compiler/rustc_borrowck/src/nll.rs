#![deny(rustc::untranslatable_diagnostic)]
#![deny(rustc::diagnostic_outside_of_impl)]
//! The entry point of the NLL borrow checker.

use polonius_engine::{Algorithm, Output};
use rustc_data_structures::fx::FxIndexMap;
use rustc_hir::def_id::LocalDefId;
use rustc_index::IndexSlice;
use rustc_middle::mir::{create_dump_file, dump_enabled, dump_mir, PassWhere};
use rustc_middle::mir::{Body, ClosureOutlivesSubject, ClosureRegionRequirements, Promoted};
use rustc_middle::ty::print::with_no_trimmed_paths;
use rustc_middle::ty::{self, OpaqueHiddenType, TyCtxt};
use rustc_mir_dataflow::impls::MaybeInitializedPlaces;
use rustc_mir_dataflow::move_paths::MoveData;
use rustc_mir_dataflow::ResultsCursor;
use rustc_span::symbol::sym;
use std::env;
use std::io;
use std::path::PathBuf;
use std::rc::Rc;
use std::str::FromStr;

use crate::{
    borrow_set::BorrowSet,
    constraint_generation,
    consumers::ConsumerOptions,
    diagnostics::RegionErrors,
    facts::{AllFacts, AllFactsExt, RustcFacts},
    location::LocationTable,
    polonius,
    region_infer::{values::RegionValueElements, RegionInferenceContext},
    renumber,
    type_check::{self, MirTypeckRegionConstraints, MirTypeckResults},
    universal_regions::UniversalRegions,
    BorrowckInferCtxt, Upvar,
};

pub type PoloniusOutput = Output<RustcFacts>;

/// The output of `nll::compute_regions`. This includes the computed `RegionInferenceContext`, any
/// closure requirements to propagate, and any generated errors.
pub(crate) struct NllOutput<'tcx> {
    pub regioncx: RegionInferenceContext<'tcx>,
    pub opaque_type_values: FxIndexMap<LocalDefId, OpaqueHiddenType<'tcx>>,
    pub polonius_input: Option<Box<AllFacts>>,
    pub polonius_output: Option<Rc<PoloniusOutput>>,
    pub opt_closure_req: Option<ClosureRegionRequirements<'tcx>>,
    pub nll_errors: RegionErrors<'tcx>,
}

/// Rewrites the regions in the MIR to use NLL variables, also scraping out the set of universal
/// regions (e.g., region parameters) declared on the function. That set will need to be given to
/// `compute_regions`.
#[instrument(skip(infcx, param_env, body, promoted), level = "debug")]
pub(crate) fn replace_regions_in_mir<'tcx>(
    infcx: &BorrowckInferCtxt<'_, 'tcx>,
    param_env: ty::ParamEnv<'tcx>,
    body: &mut Body<'tcx>,
    promoted: &mut IndexSlice<Promoted, Body<'tcx>>,
) -> UniversalRegions<'tcx> {
    let def = body.source.def_id().expect_local();

    debug!(?def);

    // Compute named region information. This also renumbers the inputs/outputs.
    let universal_regions = UniversalRegions::new(infcx, def, param_env);

    // Replace all remaining regions with fresh inference variables.
    renumber::renumber_mir(infcx, body, promoted);

    dump_mir(infcx.tcx, false, "renumber", &0, body, |_, _| Ok(()));

    universal_regions
}

/// Computes the (non-lexical) regions from the input MIR.
///
/// This may result in errors being reported.
pub(crate) fn compute_regions<'cx, 'tcx>(
    infcx: &BorrowckInferCtxt<'_, 'tcx>,
    universal_regions: UniversalRegions<'tcx>,
    body: &Body<'tcx>,
    promoted: &IndexSlice<Promoted, Body<'tcx>>,
    location_table: &LocationTable,
    param_env: ty::ParamEnv<'tcx>,
    flow_inits: &mut ResultsCursor<'cx, 'tcx, MaybeInitializedPlaces<'cx, 'tcx>>,
    move_data: &MoveData<'tcx>,
    borrow_set: &BorrowSet<'tcx>,
    upvars: &[Upvar<'tcx>],
    consumer_options: Option<ConsumerOptions>,
) -> NllOutput<'tcx> {
    let is_polonius_legacy_enabled = infcx.tcx.sess.opts.unstable_opts.polonius.is_legacy_enabled();
    let polonius_input = consumer_options.map(|c| c.polonius_input()).unwrap_or_default()
        || is_polonius_legacy_enabled;
    let polonius_output = consumer_options.map(|c| c.polonius_output()).unwrap_or_default()
        || is_polonius_legacy_enabled;
    let mut all_facts =
        (polonius_input || AllFacts::enabled(infcx.tcx)).then_some(AllFacts::default());

    let universal_regions = Rc::new(universal_regions);

    let elements = &Rc::new(RegionValueElements::new(body));

    // Run the MIR type-checker.
    let MirTypeckResults {
        constraints,
        universal_region_relations,
        opaque_type_values,
        live_loans,
    } = type_check::type_check(
        infcx,
        param_env,
        body,
        promoted,
        &universal_regions,
        location_table,
        borrow_set,
        &mut all_facts,
        flow_inits,
        move_data,
        elements,
        upvars,
        polonius_input,
    );

    if let Some(all_facts) = &mut all_facts {
        let _prof_timer = infcx.tcx.prof.generic_activity("polonius_fact_generation");
        polonius::emit_move_facts(all_facts, move_data, location_table, body);
        polonius::emit_universal_region_facts(
            all_facts,
            borrow_set,
            &universal_regions,
            &universal_region_relations,
        );
    }

    // Create the region inference context, taking ownership of the
    // region inference data that was contained in `infcx`, and the
    // base constraints generated by the type-check.
    let var_origins = infcx.get_region_var_origins();
    let MirTypeckRegionConstraints {
        placeholder_indices,
        placeholder_index_to_region: _,
        mut liveness_constraints,
        outlives_constraints,
        member_constraints,
        universe_causes,
        type_tests,
    } = constraints;
    let placeholder_indices = Rc::new(placeholder_indices);

    constraint_generation::generate_constraints(
        infcx,
        &mut liveness_constraints,
        &mut all_facts,
        location_table,
        body,
        borrow_set,
    );
    polonius::emit_cfg_and_loan_kills_facts(
        infcx,
        &mut all_facts,
        location_table,
        body,
        borrow_set,
    );

    let mut regioncx = RegionInferenceContext::new(
        infcx,
        var_origins,
        universal_regions,
        placeholder_indices,
        universal_region_relations,
        outlives_constraints,
        member_constraints,
        universe_causes,
        type_tests,
        liveness_constraints,
        elements,
        live_loans,
    );

    // Generate various additional constraints.
    polonius::emit_loan_invalidations_facts(
        infcx.tcx,
        &mut all_facts,
        location_table,
        body,
        borrow_set,
    );

    // If requested: dump NLL facts, and run legacy polonius analysis.
    let polonius_output = all_facts.as_ref().and_then(|all_facts| {
        if infcx.tcx.sess.opts.unstable_opts.nll_facts {
            let def_id = body.source.def_id();
            let def_path = infcx.tcx.def_path(def_id);
            let dir_path = PathBuf::from(&infcx.tcx.sess.opts.unstable_opts.nll_facts_dir)
                .join(def_path.to_filename_friendly_no_crate());
            all_facts.write_to_dir(dir_path, location_table).unwrap();
        }

        if polonius_output {
            let algorithm =
                env::var("POLONIUS_ALGORITHM").unwrap_or_else(|_| String::from("Hybrid"));
            let algorithm = Algorithm::from_str(&algorithm).unwrap();
            debug!("compute_regions: using polonius algorithm {:?}", algorithm);
            let _prof_timer = infcx.tcx.prof.generic_activity("polonius_analysis");
            Some(Rc::new(Output::compute(all_facts, algorithm, false)))
        } else {
            None
        }
    });

    // Solve the region constraints.
    let (closure_region_requirements, nll_errors) =
        regioncx.solve(infcx, param_env, body, polonius_output.clone());

    if !nll_errors.is_empty() {
        // Suppress unhelpful extra errors in `infer_opaque_types`.
        infcx.set_tainted_by_errors(infcx.tcx.sess.delay_span_bug(
            body.span,
            "`compute_regions` tainted `infcx` with errors but did not emit any errors",
        ));
    }

    let remapped_opaque_tys = regioncx.infer_opaque_types(infcx, opaque_type_values);

    NllOutput {
        regioncx,
        opaque_type_values: remapped_opaque_tys,
        polonius_input: all_facts.map(Box::new),
        polonius_output,
        opt_closure_req: closure_region_requirements,
        nll_errors,
    }
}

pub(super) fn dump_mir_results<'tcx>(
    infcx: &BorrowckInferCtxt<'_, 'tcx>,
    body: &Body<'tcx>,
    regioncx: &RegionInferenceContext<'tcx>,
    closure_region_requirements: &Option<ClosureRegionRequirements<'tcx>>,
) {
    if !dump_enabled(infcx.tcx, "nll", body.source.def_id()) {
        return;
    }

    dump_mir(infcx.tcx, false, "nll", &0, body, |pass_where, out| {
        match pass_where {
            // Before the CFG, dump out the values for each region variable.
            PassWhere::BeforeCFG => {
                regioncx.dump_mir(infcx.tcx, out)?;
                writeln!(out, "|")?;

                if let Some(closure_region_requirements) = closure_region_requirements {
                    writeln!(out, "| Free Region Constraints")?;
                    for_each_region_constraint(
                        infcx.tcx,
                        closure_region_requirements,
                        &mut |msg| writeln!(out, "| {msg}"),
                    )?;
                    writeln!(out, "|")?;
                }
            }

            PassWhere::BeforeLocation(_) => {}

            PassWhere::AfterTerminator(_) => {}

            PassWhere::BeforeBlock(_) | PassWhere::AfterLocation(_) | PassWhere::AfterCFG => {}
        }
        Ok(())
    });

    // Also dump the inference graph constraints as a graphviz file.
    let _: io::Result<()> = try {
        let mut file = create_dump_file(infcx.tcx, "regioncx.all.dot", false, "nll", &0, body)?;
        regioncx.dump_graphviz_raw_constraints(&mut file)?;
    };

    // Also dump the inference graph constraints as a graphviz file.
    let _: io::Result<()> = try {
        let mut file = create_dump_file(infcx.tcx, "regioncx.scc.dot", false, "nll", &0, body)?;
        regioncx.dump_graphviz_scc_constraints(&mut file)?;
    };
}

#[allow(rustc::diagnostic_outside_of_impl)]
#[allow(rustc::untranslatable_diagnostic)]
pub(super) fn dump_annotation<'tcx>(
    infcx: &BorrowckInferCtxt<'_, 'tcx>,
    body: &Body<'tcx>,
    regioncx: &RegionInferenceContext<'tcx>,
    closure_region_requirements: &Option<ClosureRegionRequirements<'tcx>>,
    opaque_type_values: &FxIndexMap<LocalDefId, OpaqueHiddenType<'tcx>>,
    errors: &mut crate::error::BorrowckErrors<'tcx>,
) {
    let tcx = infcx.tcx;
    let base_def_id = tcx.typeck_root_def_id(body.source.def_id());
    if !tcx.has_attr(base_def_id, sym::rustc_regions) {
        return;
    }

    // When the enclosing function is tagged with `#[rustc_regions]`,
    // we dump out various bits of state as warnings. This is useful
    // for verifying that the compiler is behaving as expected. These
    // warnings focus on the closure region requirements -- for
    // viewing the intraprocedural state, the -Zdump-mir output is
    // better.

    let def_span = tcx.def_span(body.source.def_id());
    let mut err = if let Some(closure_region_requirements) = closure_region_requirements {
        let mut err = tcx.sess.diagnostic().span_note_diag(def_span, "external requirements");

        regioncx.annotate(tcx, &mut err);

        err.note(format!(
            "number of external vids: {}",
            closure_region_requirements.num_external_vids
        ));

        // Dump the region constraints we are imposing *between* those
        // newly created variables.
        for_each_region_constraint(tcx, closure_region_requirements, &mut |msg| {
            err.note(msg);
            Ok(())
        })
        .unwrap();

        err
    } else {
        let mut err = tcx.sess.diagnostic().span_note_diag(def_span, "no external requirements");
        regioncx.annotate(tcx, &mut err);

        err
    };

    if !opaque_type_values.is_empty() {
        err.note(format!("Inferred opaque type values:\n{opaque_type_values:#?}"));
    }

    errors.buffer_non_error_diag(err);
}

fn for_each_region_constraint<'tcx>(
    tcx: TyCtxt<'tcx>,
    closure_region_requirements: &ClosureRegionRequirements<'tcx>,
    with_msg: &mut dyn FnMut(String) -> io::Result<()>,
) -> io::Result<()> {
    for req in &closure_region_requirements.outlives_requirements {
        let subject = match req.subject {
            ClosureOutlivesSubject::Region(subject) => format!("{subject:?}"),
            ClosureOutlivesSubject::Ty(ty) => {
                with_no_trimmed_paths!(format!(
                    "{}",
                    ty.instantiate(tcx, |vid| ty::Region::new_var(tcx, vid))
                ))
            }
        };
        with_msg(format!("where {}: {:?}", subject, req.outlived_free_region,))?;
    }
    Ok(())
}

pub(crate) trait ConstraintDescription {
    fn description(&self) -> &'static str;
}
