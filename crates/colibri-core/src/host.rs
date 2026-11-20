//! What the embedding program attaches to an engine: host state, the types of the host
//! interface, and the root slab for values the host keeps (EMBEDDING §1, §6).

use std::any::{Any, TypeId as AnyTypeId};
use std::cell::Cell;
use std::collections::HashMap;
use std::rc::Rc;

use colibri_check::hir::{Body, Program, TypeDef};
use colibri_check::types::{Ty, TypeId};
use colibri_syntax::SourceMap;

use crate::error::{new_error, new_typed_error};
use crate::heap::MapObj;
use crate::hostheap::HostHeap;
use crate::ordered::OrderedMap;
use crate::state::Core;
use crate::value::Value;

/// Why a host function did not return a value.
#[derive(Debug)]
pub enum HostFault {
    /// The host function panicked (or reported a Colibri panic): the message is the panic's.
    Panic(String),
    /// A panic with exactly this message, which the host layer worded itself (a destroyed or
    /// busy host object).
    Message(String),
    /// The coroutine running the host function was cancelled while the function waited for a
    /// background operation (`Ctx::run_async`): unwind without a panic.
    Cancel,
}

/// A handle to a root: slot, generation of the slot, epoch of the slab.
pub type RootHandle = (u32, u32, u32);

/// The roots whose last holder let go, to be released at the next entry from the host.
pub type RootDropQueue = Rc<Cell<Vec<RootHandle>>>;

/// A slot of the root slab; the generation tells a stale handle from a live one.
#[derive(Debug, Default)]
struct RootSlot {
    generation: u32,
    value: Option<Value>,
}

/// Values the host keeps alive between calls (`Callback`s).
#[derive(Debug, Default)]
#[doc(hidden)]
pub struct Roots {
    slots: Vec<RootSlot>,
    free: Vec<u32>,
    /// Bumped when every root dies at once (shutdown, reload).
    pub epoch: u32,
}

/// Everything the host attached.
#[derive(Default)]
#[doc(hidden)]
pub struct HostSide {
    /// The empty bodies of the host functions, which name them in stack traces.
    pub bodies: Vec<Rc<Body>>,
    pub state: HashMap<AnyTypeId, Box<dyn Any>>,
    pub roots: Roots,
    pub sources: Rc<SourceMap>,
    /// Which `Vm` and which program in it (its epoch) this engine belongs to, and whether
    /// a program is loaded.
    pub owner: (u64, u32),
    pub loaded: bool,
    /// The types of the default `host` module, by name: what a bare name means. A named
    /// module's types are never here, two modules may each declare a `Child`.
    pub type_ids: HashMap<String, TypeId>,
    /// Every host type by `module` and name.
    pub module_type_ids: HashMap<(String, String), TypeId>,
    /// The type a bare name means inside a module's host functions when the module does not
    /// declare it itself: one of a module it uses (`render` naming `physics.Pos`).
    pub type_aliases: HashMap<(String, String), TypeId>,
    /// The named modules (not the standard library's) that declare a type of each bare
    /// name, in module order, for the host calling into the script.
    pub named_owners: HashMap<String, Vec<(String, TypeId)>>,
    /// The host classes by the Rust type that implements them.
    pub class_types: HashMap<AnyTypeId, TypeId>,
    /// Whether some class has a `show` hook, an `eq` hook (the hooks themselves are the
    /// engine's: they are host functions).
    pub any_show_hook: bool,
    pub any_eq_hook: bool,
    /// Which `Vm` this engine belongs to.
    pub vm_id: u64,
    pub drop_queue: RootDropQueue,
    /// The type arguments of the host function call that is running.
    pub type_args: Vec<Ty>,
    /// The objects scripts hold for the host (EMBEDDING §3).
    pub heap: HostHeap,
}

impl Core {
    /// Records the host functions of a freshly loaded program (their empty bodies) and the
    /// host types it declares.
    pub fn load_host(&mut self, program: &Program) {
        self.host.bodies = program
            .host_fns
            .iter()
            .map(|h| Rc::clone(&program.fns[h.f.0 as usize].body))
            .collect();
        self.host.class_types.clear();
        self.host.any_show_hook = false;
        self.host.any_eq_hook = false;
        let first = colibri_check::types::FIRST_USER_TYPE as usize;
        // By bare name, for the host types of the default module only. A named module's types
        // (`process.Child`, `fs.Error`) are never here: two modules each declare a `Child` or
        // an `Error`, and the name alone would silently keep the last one. They are found with
        // `host_type_in` and the module.
        self.host.type_ids = program
            .types
            .iter()
            .enumerate()
            .skip(first)
            .take(program.host_types as usize)
            .zip(&program.host_type_modules)
            .filter(|(_, m)| *m == colibri_check::host::DEFAULT_MODULE)
            .map(|((i, t), _)| (t.name.clone(), TypeId(i as u32)))
            .collect();
        self.host.module_type_ids = program
            .types
            .iter()
            .enumerate()
            .skip(first)
            .take(program.host_types as usize)
            .zip(&program.host_type_modules)
            .map(|((i, t), m)| ((m.clone(), t.name.clone()), TypeId(i as u32)))
            .collect();
        let mut owners: HashMap<String, Vec<(String, TypeId)>> = HashMap::new();
        for ((module, name), id) in &self.host.module_type_ids {
            if module != colibri_check::host::DEFAULT_MODULE
                && !colibri_check::host::is_std_module_name(module)
            {
                owners
                    .entry(name.clone())
                    .or_default()
                    .push((module.clone(), *id));
            }
        }
        for list in owners.values_mut() {
            list.sort_by(|a, b| a.0.cmp(&b.0));
        }
        self.host.named_owners = owners;
    }

    /// The type id of a struct, enum or class declared by the default `host` module of the
    /// host interface. Another module's types are found with [`Core::host_type_in`].
    pub fn host_type(&self, name: &str) -> Option<TypeId> {
        self.host.type_ids.get(name).copied()
    }

    /// The type id of a struct, enum or class that the host module `module` declares (`host`
    /// for the default module).
    pub fn host_type_in(&self, module: &str, name: &str) -> Option<TypeId> {
        let key = (module.to_string(), name.to_string());
        self.host
            .module_type_ids
            .get(&key)
            .or_else(|| self.host.type_aliases.get(&key))
            .copied()
    }

    /// Records the types a module's bare names mean that another module declares (see
    /// [`Core::host_type_here`]).
    pub fn set_host_type_aliases(&mut self, aliases: HashMap<(String, String), TypeId>) {
        self.host.type_aliases = aliases;
    }

    /// The type a host function of `module` means by the bare `name`: the type of that name in
    /// its own module, else one of a module it uses, else in the default module. With no
    /// module (the host calling into the script) it is the default module's, else the one
    /// named module that declares it. `Err` says why there is none: no such type, or several
    /// modules declare one (named, so the host can say which it means).
    pub fn host_type_here(&self, module: Option<&str>, name: &str) -> Result<TypeId, String> {
        if let Some(module) = module {
            if let Some(id) = self.host_type_in(module, name) {
                return Ok(id);
            }
            if let Some(id) = self
                .host
                .type_aliases
                .get(&(module.to_string(), name.to_string()))
            {
                return Ok(*id);
            }
        }
        if let Some(id) = self.host_type(name) {
            return Ok(id);
        }
        match self.host.named_owners.get(name).map(Vec::as_slice) {
            Some([(_, id)]) => Ok(*id),
            Some(owners) if owners.len() > 1 => {
                let modules: Vec<&str> = owners.iter().map(|(m, _)| m.as_str()).collect();
                Err(format!(
                    "the type `{name}` is declared by several modules ({}) and the host cannot tell which one it means; call from a host function of that module, or name the module (`Ctx::new_struct_in`)",
                    modules.join(", ")
                ))
            }
            _ => Err(format!(
                "the type `{name}` is not part of the loaded host interface (register it with `Vm::register_type` before loading)"
            )),
        }
    }

    /// Records which Rust type implements which host class of the loaded program.
    pub fn set_host_classes(&mut self, classes: HashMap<AnyTypeId, TypeId>) {
        self.host.class_types = classes;
    }

    /// The type id of the host class that the Rust type `any` implements.
    pub fn host_class(&self, any: AnyTypeId) -> Option<TypeId> {
        self.host.class_types.get(&any).copied()
    }

    /// The type id and the index of the variant `variant` of the enum `enum_name` that the host
    /// module `module` (`std.fs`) declared as its own with `register_module_enum`. `Err` is the
    /// reason there is none, written for the host's bug report.
    pub fn module_enum_variant(
        &self,
        module: &str,
        enum_name: &str,
        variant: &str,
    ) -> Result<(TypeId, u32), String> {
        let Some(id) = self.host_type_in(module, enum_name) else {
            return Err(format!(
                "the type `{enum_name}` of the module `{module}` is not part of the loaded host interface (declare it with `HostModule::register_module_enum` before loading)"
            ));
        };
        let def = &self.types[id.0 as usize];
        let shown = format!(
            "{}.{enum_name}",
            module.rsplit('.').next().unwrap_or(module)
        );
        if !def.is_open_enum() {
            return Err(format!(
                "`{shown}` is not a module enum (declare it with `HostModule::register_module_enum`)"
            ));
        }
        match def.variants().iter().position(|v| v.name == variant) {
            Some(i) => Ok((id, i as u32)),
            None => {
                let names: Vec<&str> = def.variants().iter().map(|v| v.name.as_str()).collect();
                Err(format!(
                    "`{shown}` has no variant `{variant}`; its variants are {}",
                    names.join(", ")
                ))
            }
        }
    }

    /// The name of a type of the loaded program.
    pub fn type_name(&self, id: TypeId) -> Option<&str> {
        self.types.get(id.0 as usize).map(|t| t.name.as_str())
    }

    /// Says which program of which `Vm` this is, whether it is loaded, and what shows the
    /// locations of its panics.
    pub fn set_owner(&mut self, owner: (u64, u32), loaded: bool, sources: Rc<SourceMap>) {
        self.host.owner = owner;
        self.host.loaded = loaded;
        self.host.sources = sources;
    }

    pub fn is_loaded(&self) -> bool {
        self.host.loaded
    }

    /// Tells the engine which `Vm` it belongs to (for handles).
    pub fn set_vm_id(&mut self, id: u64) {
        self.host.vm_id = id;
    }

    pub fn vm_id(&self) -> u64 {
        self.host.vm_id
    }

    /// Where dropped callbacks leave the roots they held.
    pub fn root_drop_queue(&self) -> RootDropQueue {
        Rc::clone(&self.host.drop_queue)
    }

    pub fn owner(&self) -> (u64, u32) {
        self.host.owner
    }

    pub fn sources(&self) -> &Rc<SourceMap> {
        &self.host.sources
    }

    /// The type arguments written (or inferred) at the host function call that is running, in
    /// the order of the function's type parameters. Empty outside a host function.
    pub fn host_type_args(&self) -> &[Ty] {
        &self.host.type_args
    }

    /// Every struct, enum and class of the loaded program, indexed by type id.
    pub fn type_defs(&self) -> &[TypeDef] {
        &self.types
    }

    /// The same descriptors, shared: a host function can keep them while it calls back into
    /// the program.
    pub fn type_defs_shared(&self) -> Rc<[TypeDef]> {
        Rc::clone(&self.types)
    }

    // ---- host state ----

    /// Stores a value of type `T` for the host functions (replacing any earlier one).
    pub fn insert_state<T: 'static>(&mut self, value: T) -> Option<T> {
        self.host
            .state
            .insert(AnyTypeId::of::<T>(), Box::new(value))
            .and_then(|old| old.downcast().ok())
            .map(|b| *b)
    }

    pub fn state_mut<T: 'static>(&mut self) -> Option<&mut T> {
        self.host
            .state
            .get_mut(&AnyTypeId::of::<T>())
            .and_then(|b| b.downcast_mut())
    }

    pub fn remove_state<T: 'static>(&mut self) -> Option<T> {
        self.host
            .state
            .remove(&AnyTypeId::of::<T>())
            .and_then(|b| b.downcast().ok())
            .map(|b| *b)
    }

    // ---- roots ----

    /// Keeps `value` alive; the handle is `(index, generation, epoch)`.
    pub fn root(&mut self, value: Value) -> RootHandle {
        let roots = &mut self.host.roots;
        let epoch = roots.epoch;
        match roots.free.pop() {
            Some(i) => {
                let slot = &mut roots.slots[i as usize];
                slot.value = Some(value);
                (i, slot.generation, epoch)
            }
            None => {
                roots.slots.push(RootSlot {
                    generation: 0,
                    value: Some(value),
                });
                (roots.slots.len() as u32 - 1, 0, epoch)
            }
        }
    }

    /// The rooted value, if the handle is still current.
    pub fn rooted(&self, handle: RootHandle) -> Option<&Value> {
        let roots = &self.host.roots;
        if roots.epoch != handle.2 {
            return None;
        }
        let slot = roots.slots.get(handle.0 as usize)?;
        if slot.generation != handle.1 {
            return None;
        }
        slot.value.as_ref()
    }

    /// Lets go of a root and hands back its value, for the engine to release (a `Drop` hook
    /// may run). `None` for a stale handle.
    #[must_use = "the value must be released by the engine"]
    pub fn take_root(&mut self, handle: RootHandle) -> Option<Value> {
        let roots = &mut self.host.roots;
        if roots.epoch != handle.2 {
            return None;
        }
        let slot = roots.slots.get_mut(handle.0 as usize)?;
        if slot.generation != handle.1 {
            return None;
        }
        let value = slot.value.take();
        slot.generation = slot.generation.wrapping_add(1);
        roots.free.push(handle.0);
        value
    }

    /// Every root dies at once; the handles made so far become stale. The values come back
    /// in slot order, for the engine to release.
    #[must_use = "the values must be released by the engine"]
    pub fn take_all_roots(&mut self) -> Vec<Value> {
        let slots = std::mem::take(&mut self.host.roots.slots);
        self.host.roots.free.clear();
        self.host.roots.epoch = self.host.roots.epoch.wrapping_add(1);
        slots.into_iter().filter_map(|slot| slot.value).collect()
    }

    /// The current root epoch (for the handles of a program not yet running).
    pub fn root_epoch(&self) -> u32 {
        self.host.roots.epoch
    }
}

/// An `Error` made from a message, as `ok`/`err` payloads need it.
pub fn error_value(message: &str) -> Value {
    new_error(Value::str(message), -1)
}

/// An `Error` that keeps the typed `value` (an enum of a host module, say) and shows as
/// `message`: `e.is[T]()` and `e.as[T]()` find the value, `{e}` prints the message.
pub fn typed_error_value(value: Value, message: &str) -> Value {
    new_typed_error(value, message)
}

/// `err(e)` of the prelude `Result`.
pub fn err_variant(error: Value) -> Value {
    use colibri_check::types::{RESULT_TYPE, VARIANT_ERR};
    Value::variant(RESULT_TYPE, VARIANT_ERR, vec![error])
}

/// `ok(v)` of the prelude `Result`.
pub fn ok_variant(v: Value) -> Value {
    use colibri_check::types::{RESULT_TYPE, VARIANT_OK};
    Value::variant(RESULT_TYPE, VARIANT_OK, vec![v])
}

/// A `Map` value with these entries in order (a repeated key keeps the last value).
pub fn map_value(entries: Vec<(Value, Value)>) -> Value {
    let mut map = OrderedMap::new();
    for (k, v) in entries {
        // Keys and values replaced here are plain data the host just made.
        let _ = map.insert(k, v);
    }
    Value::Map(MapObj::new(map))
}
