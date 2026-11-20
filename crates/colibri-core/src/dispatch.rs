//! The interface implementations of the loaded program, and the user-written methods that take
//! over from the derived `Eq`, `Ord`, `Show`, `Clone` and `Drop` of one type (DESIGN §7.4,
//! §9.2; ARCHITECTURE §6).

use std::collections::HashMap;

use colibri_check::hir::{FnId, ImplDef, Program, TypeKey};
use colibri_check::types::{CLONE, DROP, EQ, INTO_ERROR, ORD, SHOW, TypeId, is_operator};

/// The implementations a program has, and the user-written methods that take over from the
/// derived `Eq`, `Ord`, `Show`, `Clone` and `Drop` of one type.
#[derive(Default)]
#[doc(hidden)]
pub struct Tables {
    pub impls: Vec<ImplDef>,
    #[doc(hidden)]
    pub index: HashMap<(TypeKey, TypeId), u32>,
    /// The impls of `Add`, `Sub`, `Mul`, `Div` and `Neg`, by left type, interface and the head
    /// of the right-hand type (`None` when it is a type parameter, or for `Neg`).
    operators: HashMap<(TypeKey, TypeId, Option<TypeKey>), u32>,
    #[doc(hidden)]
    pub eq: HashMap<TypeId, FnId>,
    #[doc(hidden)]
    pub ord: HashMap<TypeId, FnId>,
    #[doc(hidden)]
    pub show: HashMap<TypeId, FnId>,
    #[doc(hidden)]
    pub clone: HashMap<TypeId, FnId>,
    #[doc(hidden)]
    pub drop: HashMap<TypeId, FnId>,
    /// Some `IntoError` implementation has a `message` method that must run to show an error.
    #[doc(hidden)]
    pub error_messages: bool,
}

impl Tables {
    pub fn new(program: &Program) -> Self {
        let mut t = Tables {
            impls: program.impls.clone(),
            ..Tables::default()
        };
        for (i, imp) in program.impls.iter().enumerate() {
            t.index.insert((imp.key, imp.iface), i as u32);
            if is_operator(imp.iface) && !imp.derived {
                t.operators.insert((imp.key, imp.iface, imp.rhs), i as u32);
            }
            if imp.iface == INTO_ERROR
                && !imp.derived
                && matches!(imp.methods.first(), Some(Some(_)))
            {
                t.error_messages = true;
            }
            let (TypeKey::Adt(ty), Some(Some(f)), false) =
                (imp.key, imp.methods.first(), imp.derived)
            else {
                continue;
            };
            let table = match imp.iface {
                EQ => &mut t.eq,
                ORD => &mut t.ord,
                SHOW => &mut t.show,
                CLONE => &mut t.clone,
                DROP => &mut t.drop,
                _ => continue,
            };
            table.insert(ty, *f);
        }
        t
    }

    /// The index of the user impl of operator interface `iface` for the left type `left` and
    /// the right-hand type head `right` (`None` for a type parameter, or for `Neg`).
    #[inline]
    pub fn operator_impl(
        &self,
        left: TypeKey,
        iface: TypeId,
        right: Option<TypeKey>,
    ) -> Option<u32> {
        self.operators.get(&(left, iface, right)).copied()
    }

    /// The user-written `Drop` hook of a class.
    #[inline]
    pub fn drop_hook(&self, class: TypeId) -> Option<FnId> {
        self.drop.get(&class).copied()
    }
}
