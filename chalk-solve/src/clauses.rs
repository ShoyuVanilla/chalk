use self::clause_visitor::ClauseVisitor;
use self::program_clauses::ToProgramClauses;
use crate::RustIrDatabase;
use chalk_ir::cast::{Cast, Caster};
use chalk_ir::could_match::CouldMatch;
use chalk_ir::*;
use chalk_rust_ir::*;
use rustc_hash::FxHashSet;
use std::sync::Arc;

mod clause_visitor;
pub mod program_clauses;

/// For auto-traits, we generate a default rule for every struct,
/// unless there is a manual impl for that struct given explicitly.
///
/// So, if you have `impl Send for MyList<Foo>`, then we would
/// generate no rule for `MyList` at all -- similarly if you have
/// `impl !Send for MyList<Foo>`, or `impl<T> Send for MyList<T>`.
///
/// But if you have no rules at all for `Send` / `MyList`, then we
/// generate an impl based on the field types of `MyList`. For example
/// given the following program:
///
/// ```notrust
/// #[auto] trait Send { }
///
/// struct MyList<T> {
///     data: T,
///     next: Box<Option<MyList<T>>>,
/// }
///
/// ```
///
/// we generate:
///
/// ```notrust
/// forall<T> {
///     Implemented(MyList<T>: Send) :-
///         Implemented(T: Send),
///         Implemented(Box<Option<MyList<T>>>: Send).
/// }
/// ```
pub fn push_auto_trait_impls(
    auto_trait_id: TraitId,
    auto_trait: &TraitDatum,
    struct_id: StructId,
    struct_datum: &StructDatum,
    program: &dyn RustIrDatabase,
    vec: &mut Vec<ProgramClause>,
) {
    // Must be an auto trait.
    assert!(auto_trait.binders.value.flags.auto);

    // Auto traits never have generic parameters of their own (apart from `Self`).
    assert_eq!(auto_trait.binders.binders.len(), 1);

    // If there is a `impl AutoTrait for Foo<..>` or `impl !AutoTrait
    // for Foo<..>`, where `Foo` is the struct we're looking at, then
    // we don't generate our own rules.
    if program.impl_provided_for(auto_trait_id, struct_id) {
        return;
    }

    vec.push({
        // trait_ref = `MyStruct<...>: MyAutoTrait`
        let auto_trait_ref = TraitRef {
            trait_id: auto_trait.binders.value.trait_ref.trait_id,
            parameters: vec![Ty::Apply(struct_datum.binders.value.self_ty.clone()).cast()],
        };

        // forall<P0..Pn> { // generic parameters from struct
        //   MyStruct<...>: MyAutoTrait :-
        //      Field0: MyAutoTrait,
        //      ...
        //      FieldN: MyAutoTrait
        // }
        struct_datum
            .binders
            .map_ref(|struct_contents| ProgramClauseImplication {
                consequence: auto_trait_ref.clone().cast(),
                conditions: struct_contents
                    .fields
                    .iter()
                    .cloned()
                    .map(|field_ty| TraitRef {
                        trait_id: auto_trait_id,
                        parameters: vec![field_ty.cast()],
                    })
                    .casted()
                    .collect(),
            })
            .cast()
    });
}

/// Given some goal `goal` that must be proven, along with
/// its `environment`, figures out the program clauses that apply
/// to this goal from the Rust program. So for example if the goal
/// is `Implemented(T: Clone)`, then this function might return clauses
/// derived from the trait `Clone` and its impls.
pub fn program_clauses_for_goal<'db>(
    db: &'db dyn RustIrDatabase,
    environment: &Arc<Environment>,
    goal: &DomainGoal,
) -> Vec<ProgramClause> {
    let mut vec = vec![];
    program_clauses_that_could_match(db, goal, &mut vec);
    program_clauses_for_env(db, environment, &mut vec);
    vec.retain(|c| c.could_match(goal));
    vec
}

/// Returns a set of program clauses that could possibly match
/// `goal`. This can be any superset of the correct set, but the
/// more precise you can make it, the more efficient solving will
/// be.
fn program_clauses_that_could_match(
    db: &dyn RustIrDatabase,
    goal: &DomainGoal,
    vec: &mut Vec<ProgramClause>,
) {
    let mut clauses = vec![];
    match goal {
        DomainGoal::Holds(WhereClause::Implemented(trait_ref)) => {
            db.trait_datum(trait_ref.trait_id)
                .to_program_clauses(db, &mut clauses);

            // TODO sized, unsize_trait, builtin impls?
        }
        DomainGoal::Holds(WhereClause::ProjectionEq(projection_predicate)) => {
            db.associated_ty_data(projection_predicate.projection.associated_ty_id)
                .to_program_clauses(db, &mut clauses);

            // TODO filter out some clauses?
        }
        DomainGoal::WellFormed(WellFormed::Trait(trait_predicate)) => {
            db.trait_datum(trait_predicate.trait_id)
                .to_program_clauses(db, &mut clauses);
        }
        DomainGoal::WellFormed(WellFormed::Ty(ty))
        | DomainGoal::IsLocal(ty)
        | DomainGoal::IsUpstream(ty)
        | DomainGoal::IsFullyVisible(ty)
        | DomainGoal::DownstreamType(ty) => match_ty(db, goal, ty, &mut clauses),
        DomainGoal::FromEnv(_) => (), // Computed in the environment
        DomainGoal::Normalize(projection_predicate) => {
            db.associated_ty_data(projection_predicate.projection.associated_ty_id)
                .to_program_clauses(db, &mut clauses);
        }
        DomainGoal::UnselectedNormalize(normalize) => {
            match_ty(db, goal, &normalize.ty, &mut clauses)
        }
        DomainGoal::InScope(type_kind_id) => match_type_kind(db, *type_kind_id, &mut clauses),
        DomainGoal::LocalImplAllowed(trait_ref) => db
            .trait_datum(trait_ref.trait_id)
            .to_program_clauses(db, &mut clauses),
        DomainGoal::Compatible(()) => (),
    };

    vec.extend(
        clauses
            .into_iter()
            .filter(|clause| clause.could_match(goal))
            .collect::<Vec<_>>(),
    );
}

fn match_ty(db: &dyn RustIrDatabase, goal: &DomainGoal, ty: &Ty, clauses: &mut Vec<ProgramClause>) {
    match ty {
        Ty::Apply(application_ty) => match application_ty.name {
            TypeName::TypeKindId(type_kind_id) => match_type_kind(db, type_kind_id, clauses),
            TypeName::Placeholder(_) => {
                let implication = ProgramClauseImplication {
                    consequence: goal.clone(),
                    conditions: vec![],
                };
                clauses.push(ProgramClause::Implies(implication));
            }
            TypeName::AssociatedType(type_id) => db
                .associated_ty_data(type_id)
                .to_program_clauses(db, clauses),
        },
        Ty::Projection(projection_ty) => db
            .associated_ty_data(projection_ty.associated_ty_id)
            .to_program_clauses(db, clauses),
        Ty::UnselectedProjection(_) => (), //TODO what to do with the type_name?
        Ty::ForAll(quantified_ty) => match_ty(db, goal, &quantified_ty.ty, clauses), //TODO is recursion actually needed?
        Ty::BoundVar(_) | Ty::InferenceVar(_) => (),
    }
}

fn match_type_kind(
    db: &dyn RustIrDatabase,
    type_kind_id: TypeKindId,
    clauses: &mut Vec<ProgramClause>,
) {
    match type_kind_id {
        TypeKindId::TypeId(type_id) => db
            .associated_ty_data(type_id)
            .to_program_clauses(db, clauses),
        TypeKindId::TraitId(trait_id) => db.trait_datum(trait_id).to_program_clauses(db, clauses),
        TypeKindId::StructId(struct_id) => {
            db.struct_datum(struct_id).to_program_clauses(db, clauses)
        }
    }
}

fn program_clauses_for_env<'db>(
    db: &'db dyn RustIrDatabase,
    environment: &Arc<Environment>,
    clauses: &mut Vec<ProgramClause>,
) {
    let mut last_round = FxHashSet::default();
    {
        let mut visitor = ClauseVisitor::new(db, &mut last_round);
        for clause in &environment.clauses {
            visitor.visit_program_clause(&clause);
        }
    }

    let mut closure = last_round.clone();
    let mut next_round = FxHashSet::default();
    while !last_round.is_empty() {
        let mut visitor = ClauseVisitor::new(db, &mut next_round);
        for clause in last_round.drain() {
            visitor.visit_program_clause(&clause);
        }
        last_round.extend(
            next_round
                .drain()
                .filter(|clause| closure.insert(clause.clone())),
        );
    }

    clauses.extend(closure.drain())
}
