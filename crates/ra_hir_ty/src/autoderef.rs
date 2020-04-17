//! In certain situations, rust automatically inserts derefs as necessary: for
//! example, field accesses `foo.bar` still work when `foo` is actually a
//! reference to a type with the field `bar`. This is an approximation of the
//! logic in rustc (which lives in librustc_typeck/check/autoderef.rs).

use std::iter::successors;

use hir_def::lang_item::LangItemTarget;
use hir_expand::name::name;
use log::{info, warn};
use ra_db::CrateId;

use crate::{
    db::HirDatabase,
    traits::{InEnvironment, Solution},
    utils::generics,
    BoundVar, Canonical, DebruijnIndex, Obligation, Substs, TraitRef, Ty,
};

const AUTODEREF_RECURSION_LIMIT: usize = 10;

pub fn autoderef<'a>(
    db: &'a dyn HirDatabase,
    krate: Option<CrateId>,
    ty: InEnvironment<Canonical<Ty>>,
) -> impl Iterator<Item = Canonical<Ty>> + 'a {
    let InEnvironment { value: ty, environment } = ty;
    successors(Some(ty), move |ty| {
        deref(db, krate?, InEnvironment { value: ty, environment: environment.clone() })
    })
    .take(AUTODEREF_RECURSION_LIMIT)
}

pub(crate) fn deref(
    db: &dyn HirDatabase,
    krate: CrateId,
    ty: InEnvironment<&Canonical<Ty>>,
) -> Option<Canonical<Ty>> {
    if let Some(derefed) = ty.value.value.builtin_deref() {
        Some(Canonical { value: derefed, num_vars: ty.value.num_vars })
    } else {
        deref_by_trait(db, krate, ty)
    }
}

fn deref_by_trait(
    db: &dyn HirDatabase,
    krate: CrateId,
    ty: InEnvironment<&Canonical<Ty>>,
) -> Option<Canonical<Ty>> {
    let deref_trait = match db.lang_item(krate, "deref".into())? {
        LangItemTarget::TraitId(it) => it,
        _ => return None,
    };
    let target = db.trait_data(deref_trait).associated_type_by_name(&name![Target])?;

    let generic_params = generics(db.upcast(), target.into());
    if generic_params.len() != 1 {
        // the Target type + Deref trait should only have one generic parameter,
        // namely Deref's Self type
        return None;
    }

    // FIXME make the Canonical / bound var handling nicer

    let parameters =
        Substs::build_for_generics(&generic_params).push(ty.value.value.clone()).build();

    // Check that the type implements Deref at all
    let trait_ref = TraitRef { trait_: deref_trait, substs: parameters.clone() };
    let implements_goal = super::Canonical {
        num_vars: ty.value.num_vars,
        value: InEnvironment {
            value: Obligation::Trait(trait_ref),
            environment: ty.environment.clone(),
        },
    };
    if db.trait_solve(krate, implements_goal).is_none() {
        return None;
    }

    // Now do the assoc type projection
    let projection = super::traits::ProjectionPredicate {
        ty: Ty::Bound(BoundVar::new(DebruijnIndex::INNERMOST, ty.value.num_vars)),
        projection_ty: super::ProjectionTy { associated_ty: target, parameters },
    };

    let obligation = super::Obligation::Projection(projection);

    let in_env = InEnvironment { value: obligation, environment: ty.environment };

    let canonical = super::Canonical { num_vars: 1 + ty.value.num_vars, value: in_env };

    let solution = db.trait_solve(krate, canonical)?;

    match &solution {
        Solution::Unique(vars) => {
            // FIXME: vars may contain solutions for any inference variables
            // that happened to be inside ty. To correctly handle these, we
            // would have to pass the solution up to the inference context, but
            // that requires a larger refactoring (especially if the deref
            // happens during method resolution). So for the moment, we just
            // check that we're not in the situation we're we would actually
            // need to handle the values of the additional variables, i.e.
            // they're just being 'passed through'. In the 'standard' case where
            // we have `impl<T> Deref for Foo<T> { Target = T }`, that should be
            // the case.

            // FIXME: if the trait solver decides to truncate the type, these
            // assumptions will be broken. We would need to properly introduce
            // new variables in that case

            for i in 1..vars.0.num_vars {
                if vars.0.value[i - 1] != Ty::Bound(BoundVar::new(DebruijnIndex::INNERMOST, i - 1))
                {
                    warn!("complex solution for derefing {:?}: {:?}, ignoring", ty.value, solution);
                    return None;
                }
            }
            Some(Canonical {
                value: vars.0.value[vars.0.value.len() - 1].clone(),
                num_vars: vars.0.num_vars,
            })
        }
        Solution::Ambig(_) => {
            info!("Ambiguous solution for derefing {:?}: {:?}", ty.value, solution);
            None
        }
    }
}
