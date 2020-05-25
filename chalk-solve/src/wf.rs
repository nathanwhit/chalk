use std::{fmt, iter};

use crate::ext::*;
use crate::goal_builder::GoalBuilder;
use crate::rust_ir::*;
use crate::solve::SolverChoice;
use crate::split::Split;
use crate::RustIrDatabase;
use chalk_ir::cast::*;
use chalk_ir::fold::shift::Shift;
use chalk_ir::interner::Interner;
use chalk_ir::visit::{Visit, Visitor};
use chalk_ir::*;

#[derive(Debug)]
pub enum WfError<I: Interner> {
    IllFormedTypeDecl(chalk_ir::AdtId<I>),
    IllFormedTraitImpl(chalk_ir::TraitId<I>),
}

impl<I: Interner> fmt::Display for WfError<I> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WfError::IllFormedTypeDecl(id) => write!(
                f,
                "type declaration `{:?}` does not meet well-formedness requirements",
                id
            ),
            WfError::IllFormedTraitImpl(id) => write!(
                f,
                "trait impl for `{:?}` does not meet well-formedness requirements",
                id
            ),
        }
    }
}

impl<I: Interner> std::error::Error for WfError<I> {}

pub struct WfSolver<'db, I: Interner> {
    db: &'db dyn RustIrDatabase<I>,
    solver_choice: SolverChoice,
}

struct InputTypeCollector<'i, I: Interner> {
    types: Vec<Ty<I>>,
    interner: &'i I,
}

impl<'i, I: Interner> InputTypeCollector<'i, I> {
    fn new(interner: &'i I) -> Self {
        Self {
            types: Vec::new(),
            interner,
        }
    }

    fn types_in(interner: &'i I, value: impl Visit<I>) -> Vec<Ty<I>> {
        let mut collector = Self::new(interner);
        value.visit_with(&mut collector, DebruijnIndex::INNERMOST);
        collector.types
    }
}

impl<'i, I: Interner> Visitor<'i, I> for InputTypeCollector<'i, I> {
    type Result = ();

    fn as_dyn(&mut self) -> &mut dyn Visitor<'i, I, Result = Self::Result> {
        self
    }

    fn interner(&self) -> &'i I {
        self.interner
    }

    fn visit_where_clause(&mut self, where_clause: &WhereClause<I>, outer_binder: DebruijnIndex) {
        match where_clause {
            WhereClause::AliasEq(alias_eq) => alias_eq
                .alias
                .clone()
                .intern(self.interner)
                .visit_with(self, outer_binder),
            WhereClause::Implemented(trait_ref) => {
                trait_ref.visit_with(self, outer_binder);
            }
        }
    }

    fn visit_ty(&mut self, ty: &Ty<I>, outer_binder: DebruijnIndex) {
        let interner = self.interner();

        let mut push_ty = || {
            self.types
                .push(ty.shifted_out_to(interner, outer_binder).unwrap())
        };
        match ty.data(interner) {
            TyData::Apply(apply) => {
                push_ty();
                apply.visit_with(self, outer_binder);
            }

            TyData::Dyn(clauses) => {
                push_ty();
                clauses.visit_with(self, outer_binder);
            }

            TyData::Alias(AliasTy::Projection(proj)) => {
                push_ty();
                proj.visit_with(self, outer_binder);
            }

            TyData::Alias(AliasTy::Opaque(opaque_ty)) => {
                push_ty();
                opaque_ty.visit_with(self, outer_binder);
            }

            TyData::Placeholder(_) => {
                push_ty();
            }

            // Type parameters do not carry any input types (so we can sort of assume they are
            // always WF).
            TyData::BoundVar(..) => (),

            // Higher-kinded types such as `for<'a> fn(&'a u32)` introduce their own implied
            // bounds, and these bounds will be enforced upon calling such a function. In some
            // sense, well-formedness requirements for the input types of an HKT will be enforced
            // lazily, so no need to include them here.
            TyData::Function(..) => (),

            TyData::InferenceVar(..) => {
                panic!("unexpected inference variable in wf rules: {:?}", ty)
            }
        }
    }
}

impl<'db, I> WfSolver<'db, I>
where
    I: Interner,
{
    /// Constructs a new `WfSolver`.
    pub fn new(db: &'db dyn RustIrDatabase<I>, solver_choice: SolverChoice) -> Self {
        Self { db, solver_choice }
    }

    /// TODO: Currently only handles structs, may need more work for enums & unions
    pub fn verify_adt_decl(&self, adt_id: AdtId<I>) -> Result<(), WfError<I>> {
        let interner = self.db.interner();

        // Given a struct like
        //
        // ```rust
        // struct Foo<T> where T: Eq {
        //     data: Vec<T>
        // }
        // ```
        let struct_datum = self.db.adt_datum(adt_id);

        let mut gb = GoalBuilder::new(self.db);
        let struct_data = struct_datum
            .binders
            .map_ref(|b| (&b.fields, &b.where_clauses));

        // We make a goal like...
        //
        // forall<T> { ... }
        let wg_goal = gb.forall(&struct_data, (), |gb, _, (fields, where_clauses), ()| {
            let interner = gb.interner();

            // struct is well-formed in terms of Sized
            let sized_constraint_goal = WfWellKnownGoals::struct_sized_constraint(gb.db(), fields);

            // (FromEnv(T: Eq) => ...)
            gb.implies(
                where_clauses
                    .iter()
                    .cloned()
                    .map(|wc| wc.into_from_env_goal(interner)),
                |gb| {
                    // WellFormed(Vec<T>), for each field type `Vec<T>` or type that appears in the where clauses
                    let types =
                        InputTypeCollector::types_in(gb.interner(), (&fields, &where_clauses));

                    gb.all(
                        types
                            .into_iter()
                            .map(|ty| ty.well_formed().cast(interner))
                            .chain(sized_constraint_goal.into_iter()),
                    )
                },
            )
        });

        let wg_goal = wg_goal.into_closed_goal(interner);

        let is_legal = match self.solver_choice.into_solver().solve(self.db, &wg_goal) {
            Some(sol) => sol.is_unique(),
            None => false,
        };

        if !is_legal {
            Err(WfError::IllFormedTypeDecl(adt_id))
        } else {
            Ok(())
        }
    }

    pub fn verify_trait_impl(&self, impl_id: ImplId<I>) -> Result<(), WfError<I>> {
        let interner = self.db.interner();

        let impl_datum = self.db.impl_datum(impl_id);
        let trait_id = impl_datum.trait_id();

        let impl_goal = Goal::all(
            interner,
            impl_header_wf_goal(self.db, impl_id).into_iter().chain(
                impl_datum
                    .associated_ty_value_ids
                    .iter()
                    .filter_map(|&id| compute_assoc_ty_goal(self.db, id)),
            ),
        );

        debug!("WF trait goal: {:?}", impl_goal);

        let is_legal = match self
            .solver_choice
            .into_solver()
            .solve(self.db, &impl_goal.into_closed_goal(interner))
        {
            Some(sol) => sol.is_unique(),
            None => false,
        };

        if is_legal {
            Ok(())
        } else {
            Err(WfError::IllFormedTraitImpl(trait_id))
        }
    }
}

fn impl_header_wf_goal<I: Interner>(
    db: &dyn RustIrDatabase<I>,
    impl_id: ImplId<I>,
) -> Option<Goal<I>> {
    let impl_datum = db.impl_datum(impl_id);

    if !impl_datum.is_positive() {
        return None;
    }

    let impl_fields = impl_datum
        .binders
        .map_ref(|v| (&v.trait_ref, &v.where_clauses));

    let mut gb = GoalBuilder::new(db);
    // forall<P0...Pn> {...}
    let well_formed_goal = gb.forall(&impl_fields, (), |gb, _, (trait_ref, where_clauses), ()| {
        let interner = gb.interner();

        let trait_constraint_goal = WfWellKnownGoals::inside_impl(gb.db(), &trait_ref);

        // if (WC && input types are well formed) { ... }
        gb.implies(
            impl_wf_environment(interner, &where_clauses, &trait_ref),
            |gb| {
                // We retrieve all the input types of the where clauses appearing on the trait impl,
                // e.g. in:
                // ```
                // impl<T, K> Foo for (T, K) where T: Iterator<Item = (HashSet<K>, Vec<Box<T>>)> { ... }
                // ```
                // we would retrieve `HashSet<K>`, `Box<T>`, `Vec<Box<T>>`, `(HashSet<K>, Vec<Box<T>>)`.
                // We will have to prove that these types are well-formed (e.g. an additional `K: Hash`
                // bound would be needed here).
                let types = InputTypeCollector::types_in(gb.interner(), &where_clauses);

                // Things to prove well-formed: input types of the where-clauses, projection types
                // appearing in the header, associated type values, and of course the trait ref.
                debug!("verify_trait_impl: input_types={:?}", types);
                let goals = types
                    .into_iter()
                    .map(|ty| ty.well_formed().cast(interner))
                    .chain(Some((*trait_ref).clone().well_formed().cast(interner)))
                    .chain(trait_constraint_goal.into_iter());

                gb.all::<_, Goal<I>>(goals)
            },
        )
    });

    Some(
        gb.all(
            iter::once(well_formed_goal)
                .chain(WfWellKnownGoals::outside_impl(db, &impl_datum).into_iter()),
        ),
    )
}

/// Creates the conditions that an impl (and its contents of an impl)
/// can assume to be true when proving that it is well-formed.
fn impl_wf_environment<'i, I: Interner>(
    interner: &'i I,
    where_clauses: &'i [QuantifiedWhereClause<I>],
    trait_ref: &'i TraitRef<I>,
) -> impl Iterator<Item = ProgramClause<I>> + 'i {
    // if (WC) { ... }
    let wc = where_clauses
        .iter()
        .cloned()
        .map(move |qwc| qwc.into_from_env_goal(interner).cast(interner));

    // We retrieve all the input types of the type on which we implement the trait: we will
    // *assume* that these types are well-formed, e.g. we will be able to derive that
    // `K: Hash` holds without writing any where clause.
    //
    // Example:
    // ```
    // struct HashSet<K> where K: Hash { ... }
    //
    // impl<K> Foo for HashSet<K> {
    //     // Inside here, we can rely on the fact that `K: Hash` holds
    // }
    // ```
    let types = InputTypeCollector::types_in(interner, trait_ref);

    let types_wf = types
        .into_iter()
        .map(move |ty| ty.into_from_env_goal(interner).cast(interner));

    wc.chain(types_wf)
}

/// Associated type values are special because they can be parametric (independently of
/// the impl), so we issue a special goal which is quantified using the binders of the
/// associated type value, for example in:
///
/// ```ignore
/// trait Foo {
///     type Item<'a>: Clone where Self: 'a
/// }
///
/// impl<T> Foo for Box<T> {
///     type Item<'a> = Box<&'a T>;
/// }
/// ```
///
/// we would issue the following subgoal: `forall<'a> { WellFormed(Box<&'a T>) }`.
///
/// Note that there is no binder for `T` in the above: the goal we
/// generate is expected to be exected in the context of the
/// larger WF goal for the impl, which already has such a
/// binder. So the entire goal for the impl might be:
///
/// ```ignore
/// forall<T> {
///     WellFormed(Box<T>) /* this comes from the impl, not this routine */,
///     forall<'a> { WellFormed(Box<&'a T>) },
/// }
/// ```
fn compute_assoc_ty_goal<I: Interner>(
    db: &dyn RustIrDatabase<I>,
    assoc_ty_id: AssociatedTyValueId<I>,
) -> Option<Goal<I>> {
    let mut gb = GoalBuilder::new(db);
    let assoc_ty = &db.associated_ty_value(assoc_ty_id);

    // Create `forall<T, 'a> { .. }`
    Some(gb.forall(
        &assoc_ty.value.map_ref(|v| &v.ty),
        assoc_ty_id,
        |gb, assoc_ty_substitution, value_ty, assoc_ty_id| {
            let interner = gb.interner();
            let db = gb.db();

            // Hmm, because `Arc<AssociatedTyValue>` does not implement `Fold`, we can't pass this value through,
            // just the id, so we have to fetch `assoc_ty` from the database again.
            // Implementing `Fold` for `AssociatedTyValue` doesn't *quite* seem right though, as that
            // would result in a deep clone, and the value is inert. We could do some more refatoring
            // (move the `Arc` behind a newtype, for example) to fix this, but for now doesn't
            // seem worth it.
            let assoc_ty = &db.associated_ty_value(assoc_ty_id);

            let (impl_parameters, projection) = db
                .impl_parameters_and_projection_from_associated_ty_value(
                    &assoc_ty_substitution.parameters(interner),
                    assoc_ty,
                );

            // If (/* impl WF environment */) { ... }
            let impl_id = assoc_ty.impl_id;
            let impl_datum = &db.impl_datum(impl_id);
            let ImplDatumBound {
                trait_ref: impl_trait_ref,
                where_clauses: impl_where_clauses,
            } = impl_datum.binders.substitute(interner, impl_parameters);
            let impl_wf_clauses =
                impl_wf_environment(interner, &impl_where_clauses, &impl_trait_ref);
            gb.implies(impl_wf_clauses, |gb| {
                // Get the bounds and where clauses from the trait
                // declaration, substituted appropriately.
                //
                // From our example:
                //
                // * bounds
                //     * original in trait, `Clone`
                //     * after substituting impl parameters, `Clone`
                //     * note that the self-type is not yet supplied for bounds,
                //       we will do that later
                // * where clauses
                //     * original in trait, `Self: 'a`
                //     * after substituting impl parameters, `Box<!T>: '!a`
                let assoc_ty_datum = db.associated_ty_data(projection.associated_ty_id);
                let AssociatedTyDatumBound {
                    bounds: defn_bounds,
                    where_clauses: defn_where_clauses,
                } = assoc_ty_datum
                    .binders
                    .substitute(interner, &projection.substitution);

                // Create `if (/* where clauses on associated type value */) { .. }`
                gb.implies(
                    defn_where_clauses
                        .iter()
                        .cloned()
                        .map(|qwc| qwc.into_from_env_goal(interner)),
                    |gb| {
                        let types = InputTypeCollector::types_in(gb.interner(), value_ty);

                        // We require that `WellFormed(T)` for each type that appears in the value
                        let wf_goals = types
                            .into_iter()
                            .map(|ty| ty.well_formed())
                            .casted(interner);

                        // Check that the `value_ty` meets the bounds from the trait.
                        // Here we take the substituted bounds (`defn_bounds`) and we
                        // supply the self-type `value_ty` to yield the final result.
                        //
                        // In our example, the bound was `Clone`, so the combined
                        // result is `Box<!T>: Clone`. This is then converted to a
                        // well-formed goal like `WellFormed(Box<!T>: Clone)`.
                        let bound_goals = defn_bounds
                            .iter()
                            .cloned()
                            .flat_map(|qb| qb.into_where_clauses(interner, (*value_ty).clone()))
                            .map(|qwc| qwc.into_well_formed_goal(interner))
                            .casted(interner);

                        // Concatenate the WF goals of inner types + the requirements from trait
                        gb.all::<_, Goal<I>>(wf_goals.chain(bound_goals))
                    },
                )
            })
        },
    ))
}

/// Defines methods to compute well-formedness goals for well-known
/// traits (e.g. a goal for all fields of struct in a Copy impl to be Copy)
struct WfWellKnownGoals {}

impl WfWellKnownGoals {
    /// A convenience method to compute the goal assuming `trait_ref`
    /// well-formedness requirements are in the environment.
    pub fn inside_impl<I: Interner>(
        db: &dyn RustIrDatabase<I>,
        trait_ref: &TraitRef<I>,
    ) -> Option<Goal<I>> {
        match db.trait_datum(trait_ref.trait_id).well_known? {
            WellKnownTrait::CopyTrait => Self::copy_impl_constraint(db, trait_ref),
            WellKnownTrait::DropTrait
            | WellKnownTrait::CloneTrait
            | WellKnownTrait::SizedTrait
            | WellKnownTrait::FnTrait
            | WellKnownTrait::FnMutTrait
            | WellKnownTrait::FnOnceTrait => None,
        }
    }

    /// Computes well-formedness goals without any assumptions about the environment.
    /// Note that `outside_impl` does not call `inside_impl`, one needs to call both
    /// in order to get the full set of goals to be proven.
    pub fn outside_impl<I: Interner>(
        db: &dyn RustIrDatabase<I>,
        impl_datum: &ImplDatum<I>,
    ) -> Option<Goal<I>> {
        let interner = db.interner();

        match db.trait_datum(impl_datum.trait_id()).well_known? {
            // You can't add a manual implementation of Sized
            WellKnownTrait::SizedTrait => Some(GoalData::CannotProve(()).intern(interner)),
            WellKnownTrait::DropTrait => Self::drop_impl_constraint(db, impl_datum),
            WellKnownTrait::CopyTrait
            | WellKnownTrait::CloneTrait
            | WellKnownTrait::FnTrait
            | WellKnownTrait::FnMutTrait
            | WellKnownTrait::FnOnceTrait => None,
        }
    }

    /// Computes a goal to prove Sized constraints on a struct definition.
    /// Struct is considered well-formed (in terms of Sized) when it either
    /// has no fields or all of it's fields except the last are proven to be Sized.
    pub fn struct_sized_constraint<I: Interner>(
        db: &dyn RustIrDatabase<I>,
        fields: &[Ty<I>],
    ) -> Option<Goal<I>> {
        if fields.len() <= 1 {
            return None;
        }

        let interner = db.interner();

        let sized_trait = db.well_known_trait_id(WellKnownTrait::SizedTrait)?;

        Some(Goal::all(
            interner,
            fields[..fields.len() - 1].iter().map(|ty| {
                TraitRef {
                    trait_id: sized_trait,
                    substitution: Substitution::from1(interner, ty.clone()),
                }
                .cast(interner)
            }),
        ))
    }

    /// Computes a goal to prove constraints on a Copy implementation.
    /// Copy impl is considered well-formed for
    ///    a) certain builtin types (scalar values, shared ref, etc..)
    ///    b) structs which
    ///        1) have all Copy fields
    ///        2) don't have a Drop impl
    fn copy_impl_constraint<I: Interner>(
        db: &dyn RustIrDatabase<I>,
        trait_ref: &TraitRef<I>,
    ) -> Option<Goal<I>> {
        let interner = db.interner();

        let ty = trait_ref.self_type_parameter(interner);
        let ty_data = ty.data(interner);

        let (adt_id, substitution) = match ty_data {
            TyData::Apply(ApplicationTy {
                name: TypeName::Adt(adt_id),
                substitution,
            }) => (*adt_id, substitution),
            // TODO(areredify)
            // when #368 lands, extend this to handle everything accordingly
            _ => return None,
        };

        // not { Implemented(ImplSelfTy: Drop) }
        let neg_drop_goal =
            db.well_known_trait_id(WellKnownTrait::DropTrait)
                .map(|drop_trait_id| {
                    TraitRef {
                        trait_id: drop_trait_id,
                        substitution: Substitution::from1(interner, ty.clone()),
                    }
                    .cast::<Goal<I>>(interner)
                    .negate(interner)
                });

        let adt_datum = db.adt_datum(adt_id);

        let goals = adt_datum
            .binders
            .map_ref(|b| &b.fields)
            .substitute(interner, substitution)
            .into_iter()
            .map(|f| {
                // Implemented(FieldTy: Copy)
                TraitRef {
                    trait_id: trait_ref.trait_id,
                    substitution: Substitution::from1(interner, f),
                }
                .cast(interner)
            })
            .chain(neg_drop_goal.into_iter());

        Some(Goal::all(interner, goals))
    }

    /// Computes goal to prove constraints on a Drop implementation
    /// Drop implementation is considered well-formed if:
    ///     a) it's implemented on an ADT
    ///     b) The generic parameters of the impl's type must all be parameters
    ///        of the Drop impl itself (i.e., no specialization like
    ///        `impl Drop for S<Foo> {...}` is allowed).
    ///     c) Any bounds on the genereic parameters of the impl must be
    ///        deductible from the bounds imposed by the struct definition
    ///        (i.e. the implementation must be exactly as generic as the ADT definition).
    ///
    /// ```rust,ignore
    /// struct S<T1, T2> { }
    /// struct Foo<T> { }
    ///
    /// impl<U1: Copy, U2: Sized> Drop for S<U2, Foo<U1>> { }
    /// ```
    ///
    /// generates the following:
    /// goal derived from c):
    ///
    /// ```notrust
    /// forall<U1, U2> {
    ///    Implemented(U1: Copy), Implemented(U2: Sized) :- FromEnv(S<U2, Foo<U1>>)
    /// }
    /// ```
    ///
    /// goal derived from b):
    /// ```notrust
    /// forall <T1, T2> {
    ///     exists<U1, U2> {
    ///        S<T1, T2> = S<U2, Foo<U1>>
    ///     }
    /// }
    /// ```
    fn drop_impl_constraint<I: Interner>(
        db: &dyn RustIrDatabase<I>,
        impl_datum: &ImplDatum<I>,
    ) -> Option<Goal<I>> {
        let interner = db.interner();

        let adt_id = match impl_datum.self_type_adt_id(interner) {
            Some(id) => id,
            // Drop can only be implemented on a nominal type
            None => return Some(GoalData::CannotProve(()).intern(interner)),
        };

        let mut gb = GoalBuilder::new(db);

        let adt_datum = db.adt_datum(adt_id);
        let adt_name = adt_datum.name(interner);

        let impl_fields = impl_datum
            .binders
            .map_ref(|v| (&v.trait_ref, &v.where_clauses));

        // forall<ImplP1...ImplPn> { .. }
        let implied_by_adt_def_goal =
            gb.forall(&impl_fields, (), |gb, _, (trait_ref, where_clauses), ()| {
                let interner = gb.interner();

                // FromEnv(ImplSelfType) => ...
                gb.implies(
                    iter::once(
                        FromEnv::Ty(trait_ref.self_type_parameter(interner))
                            .cast::<DomainGoal<I>>(interner),
                    ),
                    |gb| {
                        // All(ImplWhereClauses)
                        gb.all(
                            where_clauses
                                .iter()
                                .map(|wc| wc.clone().into_well_formed_goal(interner)),
                        )
                    },
                )
            });

        let impl_self_ty = impl_datum
            .binders
            .map_ref(|b| b.trait_ref.self_type_parameter(interner));

        // forall<StructP1..StructPN> {...}
        let eq_goal = gb.forall(
            &adt_datum.binders,
            (adt_name, impl_self_ty),
            |gb, substitution, _, (adt_name, impl_self_ty)| {
                let interner = gb.interner();

                let def_adt: Ty<I> = ApplicationTy {
                    name: adt_name,
                    substitution,
                }
                .cast(interner)
                .intern(interner);

                // exists<ImplP1...ImplPn> { .. }
                gb.exists(&impl_self_ty, def_adt, |gb, _, impl_adt, def_adt| {
                    let interner = gb.interner();

                    // StructName<StructP1..StructPn> = ImplSelfType
                    GoalData::EqGoal(EqGoal {
                        a: GenericArgData::Ty(def_adt).intern(interner),
                        b: GenericArgData::Ty(impl_adt.clone()).intern(interner),
                    })
                    .intern(interner)
                })
            },
        );

        Some(gb.all([implied_by_adt_def_goal, eq_goal].iter()))
    }
}
