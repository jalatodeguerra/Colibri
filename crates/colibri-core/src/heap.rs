//! The reference objects of the heap: lists, maps, sets and class objects, and the mutation
//! guard (ARCHITECTURE §3).
//!
//! This is the only module that touches a `RefCell`. Every function here borrows, does its
//! work and releases the borrow before returning, and none of them evaluates Colibri code or
//! calls back into the engine, so a borrow can never be held across evaluation and a
//! borrow panic is impossible. Values that leave a collection (removed, replaced, cleared) are
//! handed back to the caller, which releases them after the borrow has ended, because a
//! release can run `Drop` hooks.
//!
//! Operations that change the structure of a collection check the *mutation guard* first
//! (DESIGN §6.3, §11.6): while a `for` loop iterates a collection, or a place into it is live
//! (a `mut self` call on an element, a `for mut` loop), a structural change is rejected with a
//! [`Violation`] and the arguments are returned to the caller untouched.

use std::cell::{Cell, RefCell};
use std::fmt;
use std::rc::Rc;

use colibri_check::types::TypeId;

use crate::ordered::OrderedMap;
use crate::value::{ObjFields, Value};

// ---- the guard ----

/// Counts the live iterations and places of one collection.
#[derive(Debug)]
pub struct Guard {
    iterations: Cell<u32>,
    places: Cell<u32>,
    /// The display text of the most recent place, for the message.
    place_name: RefCell<Rc<str>>,
    /// `list`, `map` or `set`.
    what: &'static str,
}

/// A structural change (or an element write) that the guard refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Violation(pub String);

/// Arguments handed back when the guard refused an operation.
pub struct Rejected<A> {
    pub violation: Violation,
    pub args: A,
}

impl Guard {
    fn new(what: &'static str) -> Rc<Guard> {
        Rc::new(Guard {
            iterations: Cell::new(0),
            places: Cell::new(0),
            place_name: RefCell::new(Rc::from("the collection")),
            what,
        })
    }

    /// Fails when the collection is being iterated or has a live place.
    #[inline]
    pub(crate) fn check(&self) -> Result<(), Violation> {
        if self.places.get() == 0 && self.iterations.get() == 0 {
            return Ok(());
        }
        self.violation()
    }

    #[cold]
    #[inline(never)]
    fn violation(&self) -> Result<(), Violation> {
        if self.places.get() > 0 {
            let name = self.place_name.borrow().clone();
            return Err(Violation(format!(
                "`{name}` modified while an element is being updated; collect the changes in a separate {} and apply them afterwards",
                self.what
            )));
        }
        if self.iterations.get() > 0 {
            return Err(Violation(format!(
                "{} modified during iteration",
                self.what
            )));
        }
        Ok(())
    }
}

/// How a token holds a collection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Hold {
    Iteration,
    Place,
}

/// RAII proof that a guard is held; dropping it releases the hold, on every exit path.
pub struct GuardToken {
    guard: Rc<Guard>,
    hold: Hold,
}

impl GuardToken {
    fn new(guard: &Rc<Guard>, hold: Hold, name: Option<&str>) -> GuardToken {
        match hold {
            Hold::Iteration => guard.iterations.set(guard.iterations.get() + 1),
            Hold::Place => {
                guard.places.set(guard.places.get() + 1);
                if let Some(name) = name {
                    *guard.place_name.borrow_mut() = Rc::from(name);
                }
            }
        }
        GuardToken {
            guard: Rc::clone(guard),
            hold,
        }
    }
}

impl Drop for GuardToken {
    fn drop(&mut self) {
        let cell = match self.hold {
            Hold::Iteration => &self.guard.iterations,
            Hold::Place => &self.guard.places,
        };
        cell.set(cell.get().saturating_sub(1));
    }
}

impl fmt::Debug for GuardToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "GuardToken({:?})", self.hold)
    }
}

/// A list index that is not inside the list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OutOfBounds {
    pub index: i64,
    pub len: usize,
}

/// Why a list operation failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ListError {
    Guard(Violation),
    Bounds(OutOfBounds),
}

impl From<Violation> for ListError {
    fn from(v: Violation) -> Self {
        ListError::Guard(v)
    }
}

impl From<OutOfBounds> for ListError {
    fn from(e: OutOfBounds) -> Self {
        ListError::Bounds(e)
    }
}

#[inline]
fn in_bounds(index: i64, len: usize) -> Result<usize, OutOfBounds> {
    usize::try_from(index)
        .ok()
        .filter(|&i| i < len)
        .ok_or(OutOfBounds { index, len })
}

// ---- lists ----

/// A `List`: a growable vector with a guard.
pub struct ListObj {
    items: RefCell<Vec<Value>>,
    guard: Rc<Guard>,
}

impl ListObj {
    #[inline]
    pub fn new(items: Vec<Value>) -> Rc<ListObj> {
        Rc::new(ListObj {
            items: RefCell::new(items),
            guard: Guard::new("list"),
        })
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.items.borrow().len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// A copy of the element list (each element is a reference bump).
    #[inline]
    pub fn snapshot(&self) -> Vec<Value> {
        self.items.borrow().clone()
    }

    /// The element at `index`, or why there is none.
    #[inline]
    pub fn get(&self, index: i64) -> Result<Value, OutOfBounds> {
        let items = self.items.borrow();
        let i = in_bounds(index, items.len())?;
        Ok(items[i].clone())
    }

    /// The element at a position, `None` past the end (for iteration).
    #[inline]
    pub fn at(&self, i: usize) -> Option<Value> {
        self.items.borrow().get(i).map(Value::dup)
    }

    #[inline]
    pub fn first(&self) -> Option<Value> {
        self.items.borrow().first().cloned()
    }

    #[inline]
    pub fn last(&self) -> Option<Value> {
        self.items.borrow().last().cloned()
    }

    /// Runs `f` on the elements. `f` must not run Colibri code.
    pub fn with<R>(&self, f: impl FnOnce(&[Value]) -> R) -> R {
        f(&self.items.borrow())
    }

    /// Lends an element: moves it out and leaves `()` in its place, for a `for mut` loop that
    /// holds the list and is the only one to see it. `None` past the end.
    ///
    /// Engine use only: the slot must be refilled with `unlend_at` before anything else can see
    /// the list, since a `()` hole breaks the list's element invariant.
    #[doc(hidden)]
    #[inline]
    pub fn lend_at(&self, i: usize) -> Option<Value> {
        self.items
            .borrow_mut()
            .get_mut(i)
            .map(|slot| std::mem::replace(slot, Value::Unit))
    }

    /// Puts a lent element back into its slot, which holds `()` (so nothing is released).
    /// Gives the value back when the position does not exist.
    ///
    /// Engine use only (see `lend_at`).
    #[doc(hidden)]
    #[inline]
    pub fn unlend_at(&self, i: usize, v: Value) -> Result<(), Value> {
        match self.items.borrow_mut().get_mut(i) {
            Some(slot) => {
                debug_assert!(matches!(slot, Value::Unit), "a slot that was not lent");
                *slot = v;
                Ok(())
            }
            None => Err(v),
        }
    }

    #[inline]
    pub fn iter_hold(&self) -> GuardToken {
        GuardToken::new(&self.guard, Hold::Iteration, None)
    }

    #[inline]
    pub fn place_hold(&self, name: &str) -> GuardToken {
        GuardToken::new(&self.guard, Hold::Place, Some(name))
    }

    #[inline]
    pub(crate) fn check_guard(&self) -> Result<(), Violation> {
        self.guard.check()
    }

    #[inline]
    pub fn guard(&self) -> &Guard {
        &self.guard
    }

    #[inline]
    pub fn push(&self, v: Value) -> Result<(), Rejected<Value>> {
        match self.guard.check() {
            Ok(()) => {
                self.items.borrow_mut().push(v);
                Ok(())
            }
            Err(violation) => Err(Rejected { violation, args: v }),
        }
    }

    #[inline]
    pub fn pop(&self) -> Result<Option<Value>, Violation> {
        self.guard.check()?;
        Ok(self.items.borrow_mut().pop())
    }

    /// Inserts before `index` (`index == len` appends). On failure the value comes back.
    #[inline]
    pub fn insert(&self, index: i64, v: Value) -> Result<(), (ListError, Value)> {
        if let Err(violation) = self.guard.check() {
            return Err((violation.into(), v));
        }
        let mut items = self.items.borrow_mut();
        let len = items.len();
        match usize::try_from(index).ok().filter(|&i| i <= len) {
            Some(i) => {
                items.insert(i, v);
                Ok(())
            }
            None => Err((OutOfBounds { index, len }.into(), v)),
        }
    }

    #[inline]
    pub fn remove(&self, index: i64) -> Result<Value, ListError> {
        self.guard.check()?;
        let mut items = self.items.borrow_mut();
        let i = in_bounds(index, items.len())?;
        Ok(items.remove(i))
    }

    /// Removes every element and returns them (to be released).
    #[inline]
    pub fn clear(&self) -> Result<Vec<Value>, Violation> {
        self.guard.check()?;
        Ok(std::mem::take(&mut *self.items.borrow_mut()))
    }

    #[inline]
    pub fn reverse(&self) -> Result<(), Violation> {
        self.guard.check()?;
        self.items.borrow_mut().reverse();
        Ok(())
    }

    #[inline]
    pub fn extend(&self, more: Vec<Value>) -> Result<(), Rejected<Vec<Value>>> {
        match self.guard.check() {
            Ok(()) => {
                self.items.borrow_mut().extend(more);
                Ok(())
            }
            Err(violation) => Err(Rejected {
                violation,
                args: more,
            }),
        }
    }

    /// `xs[i] = v` from user code: refused while the list is held. Returns the old element,
    /// or the value back on failure.
    #[inline]
    pub fn assign(&self, index: i64, v: Value) -> Result<Value, (ListError, Value)> {
        if let Err(violation) = self.guard.check() {
            return Err((violation.into(), v));
        }
        self.replace_at(index, v)
    }

    /// Stores an element and returns the old one. Unguarded: the write-back of a place that
    /// holds the guard itself.
    #[inline]
    pub fn write_back(&self, index: i64, v: Value) -> Result<Value, (ListError, Value)> {
        self.replace_at(index, v)
    }

    #[inline]
    fn replace_at(&self, index: i64, v: Value) -> Result<Value, (ListError, Value)> {
        let mut items = self.items.borrow_mut();
        match in_bounds(index, items.len()) {
            Ok(i) => Ok(std::mem::replace(&mut items[i], v)),
            Err(e) => Err((e.into(), v)),
        }
    }

    /// Changes an element in place with `f` (which must not run Colibri code); `guarded`
    /// says whether the guard applies (a write from user code) or not (a place write-back).
    pub fn update<R>(
        &self,
        index: i64,
        guarded: bool,
        f: impl FnOnce(&mut Value) -> R,
    ) -> Result<R, ListError> {
        if guarded {
            self.guard.check()?;
        }
        let mut items = self.items.borrow_mut();
        let i = in_bounds(index, items.len())?;
        Ok(f(&mut items[i]))
    }

    /// Takes every element out, for destruction. Unguarded.
    #[inline]
    pub(crate) fn take_all(&self) -> Vec<Value> {
        std::mem::take(&mut *self.items.borrow_mut())
    }

    /// Replaces all elements and returns the old ones (to be released). Unguarded: the caller
    /// has checked the guard, since it held the guard itself while it worked.
    #[inline]
    pub fn replace_all(&self, items: Vec<Value>) -> Vec<Value> {
        std::mem::replace(&mut *self.items.borrow_mut(), items)
    }
}

impl fmt::Debug for ListObj {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "List(len {})", self.len())
    }
}

// ---- maps and sets ----

/// A `Map`: insertion-ordered, with a guard.
pub struct MapObj {
    map: RefCell<OrderedMap>,
    guard: Rc<Guard>,
}

/// What `Map.insert` replaced.
pub struct Replaced {
    /// The previous value of an existing key.
    pub old: Option<Value>,
    /// The new key, when the key already existed (the old key object stays).
    pub spare_key: Option<Value>,
}

impl MapObj {
    #[inline]
    pub fn new(entries: OrderedMap) -> Rc<MapObj> {
        Rc::new(MapObj {
            map: RefCell::new(entries),
            guard: Guard::new("map"),
        })
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.map.borrow().len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    #[inline]
    pub fn get(&self, key: &Value) -> Option<Value> {
        self.map.borrow().get(key).cloned()
    }

    /// `get` for an `int` or `str` key (outer `None`: another kind of key).
    #[inline]
    pub fn get_plain(&self, key: &Value) -> Option<Option<Value>> {
        self.map.borrow().get_plain(key).map(|v| v.cloned())
    }

    /// `insert` for an `int` or `str` key that the guard allows: the previous value of the key.
    /// Gives `value` back for any other kind of key and when the guard refuses (the caller
    /// then takes the general path, which says why).
    #[inline]
    pub fn insert_plain(&self, key: &Value, value: Value) -> Result<Option<Value>, Value> {
        if self.guard.check().is_err() {
            return Err(value);
        }
        self.map.borrow_mut().insert_plain(key, value)
    }

    #[inline]
    pub fn contains_key(&self, key: &Value) -> bool {
        self.map.borrow().contains(key)
    }

    /// Runs `f` on the entries. `f` must not run Colibri code.
    pub fn with<R>(&self, f: impl FnOnce(&OrderedMap) -> R) -> R {
        f(&self.map.borrow())
    }

    /// Entries in insertion order (each a reference bump).
    #[inline]
    pub fn snapshot(&self) -> Vec<(Value, Value)> {
        self.map.borrow().iter().cloned().collect()
    }

    #[inline]
    pub fn keys(&self) -> Vec<Value> {
        self.map.borrow().iter().map(|(k, _)| k.clone()).collect()
    }

    #[inline]
    pub fn values(&self) -> Vec<Value> {
        self.map.borrow().iter().map(|(_, v)| v.clone()).collect()
    }

    /// The first live entry at or after a position, with the position after it (for
    /// iteration, which holds the guard so positions stay valid).
    #[inline]
    pub fn next_entry(&self, from: usize) -> Option<(Value, Value, usize)> {
        let map = self.map.borrow();
        (from..map.slots())
            .find_map(|p| map.entry_at(p).map(|(k, v)| (k.clone(), v.clone(), p + 1)))
    }

    #[inline]
    pub fn iter_hold(&self) -> GuardToken {
        GuardToken::new(&self.guard, Hold::Iteration, None)
    }

    #[inline]
    pub fn place_hold(&self, name: &str) -> GuardToken {
        GuardToken::new(&self.guard, Hold::Place, Some(name))
    }

    #[inline]
    pub fn guard(&self) -> &Guard {
        &self.guard
    }

    #[inline]
    pub fn insert(&self, key: Value, value: Value) -> Result<Replaced, Rejected<(Value, Value)>> {
        if let Err(violation) = self.guard.check() {
            return Err(Rejected {
                violation,
                args: (key, value),
            });
        }
        let (old, spare_key) = self.map.borrow_mut().insert(key, value);
        Ok(Replaced { old, spare_key })
    }

    #[inline]
    pub fn remove(&self, key: &Value) -> Result<Option<(Value, Value)>, Violation> {
        self.guard.check()?;
        Ok(self.map.borrow_mut().remove(key))
    }

    #[inline]
    pub fn clear(&self) -> Result<Vec<(Value, Value)>, Violation> {
        self.guard.check()?;
        Ok(self.map.borrow_mut().take_all())
    }

    /// Changes the value of an existing key with `f` (which must not run Colibri code).
    /// `None` when the key is missing.
    pub fn update<R>(
        &self,
        key: &Value,
        guarded: bool,
        f: impl FnOnce(&mut Value) -> R,
    ) -> Result<Option<R>, Violation> {
        if guarded {
            self.guard.check()?;
        }
        Ok(self.map.borrow_mut().get_mut(key).map(f))
    }

    /// `m[k] = v` from user code: refused while the map is held; adds the key if missing.
    #[inline]
    pub fn assign(&self, key: Value, value: Value) -> Result<Replaced, Rejected<(Value, Value)>> {
        self.insert(key, value)
    }

    /// Replaces the value of a key that exists. Unguarded write-back of a place.
    #[inline]
    pub fn write_back(&self, key: &Value, value: Value) -> Option<Value> {
        let mut map = self.map.borrow_mut();
        map.get_mut(key).map(|slot| std::mem::replace(slot, value))
    }

    #[inline]
    pub(crate) fn take_all(&self) -> Vec<(Value, Value)> {
        self.map.borrow_mut().take_all()
    }
}

impl fmt::Debug for MapObj {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Map(len {})", self.len())
    }
}

/// A `Set`: the keys of an insertion-ordered map, with a guard.
pub struct SetObj {
    map: RefCell<OrderedMap>,
    guard: Rc<Guard>,
}

impl SetObj {
    #[inline]
    pub fn new(items: OrderedMap) -> Rc<SetObj> {
        Rc::new(SetObj {
            map: RefCell::new(items),
            guard: Guard::new("set"),
        })
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.map.borrow().len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    #[inline]
    pub fn contains(&self, item: &Value) -> bool {
        self.map.borrow().contains(item)
    }

    /// Runs `f` on the elements (the keys of the map). `f` must not run Colibri code.
    pub fn with<R>(&self, f: impl FnOnce(&OrderedMap) -> R) -> R {
        f(&self.map.borrow())
    }

    #[inline]
    pub fn snapshot(&self) -> Vec<Value> {
        self.map.borrow().iter().map(|(k, _)| k.clone()).collect()
    }

    /// The first live element at or after a position, with the position after it.
    #[inline]
    pub fn next_item(&self, from: usize) -> Option<(Value, usize)> {
        let map = self.map.borrow();
        (from..map.slots()).find_map(|p| map.entry_at(p).map(|(k, _)| (k.clone(), p + 1)))
    }

    #[inline]
    pub fn iter_hold(&self) -> GuardToken {
        GuardToken::new(&self.guard, Hold::Iteration, None)
    }

    #[inline]
    pub fn guard(&self) -> &Guard {
        &self.guard
    }

    /// Adds an element; returns whether it was new, and the argument back if it was not
    /// (the caller releases it).
    #[inline]
    pub fn add(&self, item: Value) -> Result<(bool, Option<Value>), Rejected<Value>> {
        if let Err(violation) = self.guard.check() {
            return Err(Rejected {
                violation,
                args: item,
            });
        }
        let (old, spare) = self.map.borrow_mut().insert(item, Value::Unit);
        Ok((old.is_none(), spare))
    }

    #[inline]
    pub fn remove(&self, item: &Value) -> Result<Option<Value>, Violation> {
        self.guard.check()?;
        Ok(self.map.borrow_mut().remove(item).map(|(k, _)| k))
    }

    #[inline]
    pub fn clear(&self) -> Result<Vec<Value>, Violation> {
        self.guard.check()?;
        Ok(self
            .map
            .borrow_mut()
            .take_all()
            .into_iter()
            .map(|(k, _)| k)
            .collect())
    }

    #[inline]
    pub(crate) fn take_all(&self) -> Vec<Value> {
        self.map
            .borrow_mut()
            .take_all()
            .into_iter()
            .map(|(k, _)| k)
            .collect()
    }
}

impl fmt::Debug for SetObj {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Set(len {})", self.len())
    }
}

// ---- class objects ----

/// A class object: its type and its fields in declaration order.
///
/// Objects die only through the engine's `release` (ARCHITECTURE §3), which takes the
/// fields out first. A debug build asserts that an object is never dropped any other way.
pub struct ClassObj {
    ty: TypeId,
    fields: RefCell<ObjFields>,
    released: Cell<bool>,
    /// The object's `Drop` hook has run (or is running); it runs at most once.
    hooked: Cell<bool>,
}

impl ClassObj {
    #[inline]
    pub fn new(ty: TypeId, fields: impl Into<ObjFields>) -> Rc<ClassObj> {
        let mut fields = fields.into();
        fields.shrink_to_fit();
        Rc::new(ClassObj {
            ty,
            fields: RefCell::new(fields),
            released: Cell::new(false),
            hooked: Cell::new(false),
        })
    }

    /// Marks the `Drop` hook as run; `true` if it had run before.
    #[inline]
    pub(crate) fn mark_hooked(&self) -> bool {
        self.hooked.replace(true)
    }

    #[inline]
    pub fn ty(&self) -> TypeId {
        self.ty
    }

    #[inline]
    pub fn field(&self, idx: usize) -> Option<Value> {
        self.fields.borrow().get(idx).cloned()
    }

    /// Reads the fields in place, without touching their reference counts.
    pub(crate) fn with_fields<R>(&self, f: impl FnOnce(&[Value]) -> R) -> R {
        f(&self.fields.borrow())
    }

    /// A copy of every field (each a reference bump).
    #[inline]
    pub fn snapshot(&self) -> Vec<Value> {
        self.fields.borrow().to_vec()
    }

    /// Stores a field and returns the old value, or hands the value back when there is no
    /// such field.
    #[inline]
    pub fn set_field(&self, idx: usize, v: Value) -> Result<Value, Value> {
        let mut fields = self.fields.borrow_mut();
        match fields.get_mut(idx) {
            Some(slot) => Ok(std::mem::replace(slot, v)),
            None => Err(v),
        }
    }

    /// Changes a field in place with `f` (which must not run Colibri code).
    pub fn update_field<R>(&self, idx: usize, f: impl FnOnce(&mut Value) -> R) -> Option<R> {
        self.fields.borrow_mut().get_mut(idx).map(f)
    }

    /// Takes the fields out and marks the object destroyed. Only `release` calls this.
    #[inline]
    pub(crate) fn take_fields(&self) -> ObjFields {
        self.released.set(true);
        std::mem::take(&mut *self.fields.borrow_mut())
    }
}

impl Drop for ClassObj {
    fn drop(&mut self) {
        // Every object must go through `release`; a Rust-side drop would skip its `Drop` hook
        // and its trace entry. (Not checked while unwinding from some other failure.)
        debug_assert!(
            self.released.get() || std::thread::panicking(),
            "a class object was dropped without being released"
        );
    }
}

impl fmt::Debug for ClassObj {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Object(type#{})", self.ty.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ints(xs: &[i64]) -> Vec<Value> {
        xs.iter().map(|&i| Value::Int(i)).collect()
    }

    #[test]
    fn an_iteration_blocks_changes_until_its_token_drops() {
        let list = ListObj::new(ints(&[1, 2]));
        let token = list.iter_hold();
        let Err(rejected) = list.push(Value::Int(3)) else {
            panic!("a held list refused the push")
        };
        assert_eq!(rejected.violation.0, "list modified during iteration");
        assert_eq!(rejected.args, Value::Int(3));
        assert!(list.assign(0, Value::Int(9)).is_err());
        drop(token);
        assert!(list.push(Value::Int(3)).is_ok());
        assert_eq!(list.len(), 3);
    }

    #[test]
    fn a_place_blocks_changes_with_its_own_message_and_wins_over_iteration() {
        let list = ListObj::new(ints(&[1]));
        let _iter = list.iter_hold();
        let _place = list.place_hold("enemies");
        let err = list.pop().unwrap_err();
        assert!(
            err.0
                .starts_with("`enemies` modified while an element is being updated")
        );
        assert!(err.0.contains("separate list"));
    }

    #[test]
    fn write_back_ignores_the_guard() {
        let list = ListObj::new(ints(&[1]));
        let _place = list.place_hold("xs");
        assert!(list.assign(0, Value::Int(2)).is_err());
        assert_eq!(list.write_back(0, Value::Int(3)).ok(), Some(Value::Int(1)));
        assert_eq!(list.get(0).ok(), Some(Value::Int(3)));
    }

    #[test]
    fn bounds_are_checked_for_every_index() {
        let list = ListObj::new(ints(&[1, 2]));
        assert_eq!(list.get(2), Err(OutOfBounds { index: 2, len: 2 }));
        assert_eq!(list.get(-1), Err(OutOfBounds { index: -1, len: 2 }));
        assert!(list.remove(5).is_err());
        let (err, back) = list.insert(3, Value::Int(0)).unwrap_err();
        assert!(matches!(err, ListError::Bounds(_)));
        assert_eq!(back, Value::Int(0));
        assert!(list.insert(2, Value::Int(0)).is_ok());
    }

    #[test]
    fn maps_and_sets_have_guards_too() {
        let map = MapObj::new(OrderedMap::new());
        assert!(map.insert(Value::Int(1), Value::Int(1)).is_ok());
        let token = map.iter_hold();
        let rejected = map
            .insert(Value::Int(2), Value::Int(2))
            .err()
            .expect("guarded");
        assert_eq!(rejected.violation.0, "map modified during iteration");
        drop(token);
        assert!(map.insert(Value::Int(2), Value::Int(2)).is_ok());
        let set = SetObj::new(OrderedMap::new());
        let token = set.iter_hold();
        assert!(set.add(Value::Int(1)).is_err());
        drop(token);
        assert!(set.add(Value::Int(1)).is_ok());
    }

    #[test]
    fn taking_the_fields_marks_an_object_destroyed() {
        let obj = ClassObj::new(TypeId(1), ints(&[1, 2]));
        assert_eq!(obj.take_fields().into_vec(), ints(&[1, 2]));
        assert!(obj.field(0).is_none());
    }
}
