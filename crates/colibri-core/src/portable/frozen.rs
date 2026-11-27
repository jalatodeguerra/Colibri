//! The borrowing rebuild of a `Shared`'s frozen graph (DESIGN §14.1, ARCHITECTURE §36.6).
//!
//! [`from_portable`](super::from_portable) takes its input and frees it node by node as it builds,
//! which a graph that every thread keeps cannot allow. This walk is the same one over a shared
//! reference: it leaves the graph as it found it, and every list, map, set and class object it
//! builds is frozen when the copy is complete.

use super::*;

/// Rebuilds the graph of a `Shared` on this thread, frozen. The graph is borrowed, so it is
/// rebuilt as often as it is asked for (once per isolate: the isolate keeps the result).
pub fn from_portable_frozen(
    portable: &Portable,
    core: &mut Core,
    stack: StackGuard,
) -> Result<Value, RebuildError> {
    let mut b = Rebuild {
        core,
        stack,
        built: HashMap::new(),
        placeholders: HashSet::new(),
        pending: 0,
    };
    match b.value_ref(portable) {
        Ok(v) => {
            b.finish();
            b.freeze_built();
            Ok(v)
        }
        Err(e) => {
            b.abandon();
            Err(e)
        }
    }
}

impl Rebuild<'_> {
    /// Freezes what the walk built that can be changed. An object that came from a handle (a
    /// channel, another `Shared`) is frozen too, which changes nothing: its state is not in it.
    pub(super) fn freeze_built(&mut self) {
        for v in self.built.values() {
            match v {
                Value::List(l) => l.freeze(),
                Value::Map(m) => m.freeze(),
                Value::Set(s) => s.freeze(),
                Value::Obj(o) => o.freeze(),
                _ => {}
            }
        }
    }

    // The same shape as `value`, one function per kind that has children (so that a frame holds
    // only what its kind needs), and nothing is given back as it goes.
    fn value_ref(&mut self, p: &Portable) -> Result<Value, RebuildError> {
        if self.stack.exceeded() {
            return Err(RebuildError::TooDeep);
        }
        self.charge(BUILT_NODE_COST)?;
        Ok(match p {
            Portable::Unit => Value::Unit,
            Portable::Bool(b) => Value::Bool(*b),
            Portable::Int(i) => Value::Int(*i),
            Portable::U64(i) => Value::U64(*i),
            Portable::Ptr(a) => Value::Ptr(*a),
            Portable::Float(x) => Value::Float(*x),
            Portable::F32(x) => Value::F32(*x),
            Portable::Char(c) => Value::Char(*c),
            Portable::Str(s) => {
                self.charge(s.len())?;
                Value::Str(Rc::from(&**s))
            }
            Portable::Bytes(b) => {
                self.charge(b.len())?;
                Value::Bytes(Rc::from(&**b))
            }
            Portable::SharedStr(id, s) => {
                self.charge(s.len())?;
                let v = Value::Str(Rc::from(&**s));
                self.built.insert(*id, v.clone());
                v
            }
            Portable::SharedBytes(id, b) => {
                self.charge(b.len())?;
                let v = Value::Bytes(Rc::from(&**b));
                self.built.insert(*id, v.clone());
                v
            }
            Portable::Range(r) => Value::Range(Rc::new(*r)),
            Portable::Fn(f, name) => Value::Fn(*f, Rc::from(&**name)),
            Portable::ByteBuf { id, bytes } => {
                self.charge(bytes.len())?;
                let v = Value::ByteBuf(Rc::new(RefCell::new(bytes.to_vec())));
                self.built.insert(*id, v.clone());
                v
            }
            Portable::Struct { id, ty, fields } => self.struct_ref(*id, *ty, fields)?,
            Portable::Tuple { id, items } => self.tuple_ref(*id, items)?,
            Portable::Enum {
                id,
                ty,
                variant,
                payload,
            } => self.enum_ref(*id, *ty, *variant, payload)?,
            Portable::Dyn { id, impl_id, val } => self.dyn_ref(*id, *impl_id, val)?,
            Portable::Closure {
                id,
                f,
                generation,
                captures,
            } => self.closure_ref(*id, *f, *generation, captures)?,
            Portable::List { id, items } => self.list_ref(*id, items)?,
            Portable::Map { id, entries } => self.map_ref(*id, entries)?,
            Portable::Set { id, items } => self.set_ref(*id, items)?,
            Portable::Obj { id, ty, fields } => self.obj_ref(*id, *ty, fields)?,
            // A frozen graph holds no variable that a closure changes: `Shared(x)` refuses one.
            Portable::Cell { .. } => return Err(RebuildError::Malformed),
            Portable::Weak(None) => Value::Weak(Weak::new()),
            Portable::Weak(Some(w)) => self.weak(w),
            Portable::Ref(id) => match self.built.get(id) {
                Some(v) => v.clone(),
                None => return Err(RebuildError::Malformed),
            },
            Portable::Handle { id, handle } => {
                let v = handle.materialise(self.core);
                self.built.insert(*id, v.clone());
                v
            }
        })
    }

    #[inline(never)]
    fn struct_ref(
        &mut self,
        id: Option<u32>,
        ty: TypeId,
        fields: &[Portable],
    ) -> Result<Value, RebuildError> {
        let mut out = StructFields::with_capacity(fields.len());
        for f in fields {
            out.push(self.value_ref(f)?);
        }
        let v = Value::Struct(Rc::new(StructVal { ty, fields: out }));
        self.register(id, &v);
        Ok(v)
    }

    #[inline(never)]
    fn tuple_ref(&mut self, id: Option<u32>, items: &[Portable]) -> Result<Value, RebuildError> {
        let mut out = Vec::with_capacity(items.len());
        for p in items {
            out.push(self.value_ref(p)?);
        }
        let v = Value::Tuple(Rc::from(out));
        self.register(id, &v);
        Ok(v)
    }

    #[inline(never)]
    fn enum_ref(
        &mut self,
        id: Option<u32>,
        ty: TypeId,
        variant: u32,
        payload: &[Portable],
    ) -> Result<Value, RebuildError> {
        if id.is_none() && payload.is_empty() && ty == OPTION_TYPE && variant == VARIANT_NONE {
            return Ok(self.core.none());
        }
        let mut out = Payload::with_capacity(payload.len());
        for f in payload {
            out.push(self.value_ref(f)?);
        }
        let v = Value::Enum(Rc::new(EnumVal {
            ty,
            variant,
            payload: out,
        }));
        self.register(id, &v);
        Ok(v)
    }

    #[inline(never)]
    fn dyn_ref(
        &mut self,
        id: Option<u32>,
        impl_id: u32,
        val: &Portable,
    ) -> Result<Value, RebuildError> {
        let v = Value::Dyn(Rc::new(DynVal {
            impl_id,
            val: self.value_ref(val)?,
        }));
        self.register(id, &v);
        Ok(v)
    }

    #[inline(never)]
    fn closure_ref(
        &mut self,
        id: Option<u32>,
        f: FnId,
        generation: u32,
        captures: &[PortableCapture],
    ) -> Result<Value, RebuildError> {
        let mut out = Vec::with_capacity(captures.len());
        for c in captures {
            out.push(Captured {
                inner: c.inner,
                mode: c.mode,
                value: self.value_ref(&c.value)?,
            });
        }
        let closure = Rc::new(ClosureObj {
            f,
            generation,
            captures: out,
        });
        self.core.track_closure(&closure, None);
        let v = Value::Closure(closure);
        self.register(id, &v);
        Ok(v)
    }

    #[inline(never)]
    fn list_ref(&mut self, id: u32, items: &[Portable]) -> Result<Value, RebuildError> {
        let list = ListObj::new(Vec::with_capacity(items.len()));
        let v = Value::List(Rc::clone(&list));
        self.built.insert(id, v.clone());
        for p in items {
            let item = self.value_ref(p)?;
            let _ = list.push(item);
        }
        Ok(v)
    }

    #[inline(never)]
    fn map_ref(
        &mut self,
        id: u32,
        entries: &[(Portable, Portable)],
    ) -> Result<Value, RebuildError> {
        let map = MapObj::new(OrderedMap::new());
        let v = Value::Map(Rc::clone(&map));
        self.built.insert(id, v.clone());
        for (k, e) in entries {
            let (k, e) = (self.value_ref(k)?, self.value_ref(e)?);
            let _ = map.insert(k, e);
        }
        Ok(v)
    }

    #[inline(never)]
    fn set_ref(&mut self, id: u32, items: &[Portable]) -> Result<Value, RebuildError> {
        let set = SetObj::new(OrderedMap::new());
        let v = Value::Set(Rc::clone(&set));
        self.built.insert(id, v.clone());
        for p in items {
            let item = self.value_ref(p)?;
            let _ = set.add(item);
        }
        Ok(v)
    }

    #[inline(never)]
    fn obj_ref(&mut self, id: u32, ty: TypeId, fields: &[Portable]) -> Result<Value, RebuildError> {
        let obj = match self.built.get(&id) {
            // Made for a `weak` reference that came first.
            Some(Value::Obj(o)) if self.placeholders.remove(&id) => Rc::clone(o),
            _ => {
                let o = ClassObj::new_exact(ty, ObjFields::from_elem(Value::Unit, fields.len()));
                self.built.insert(id, Value::Obj(Rc::clone(&o)));
                o
            }
        };
        self.core.track_object(&obj, None);
        for (i, p) in fields.iter().enumerate() {
            let field = self.value_ref(p)?;
            let _ = obj.set_field(i, field);
        }
        Ok(Value::Obj(obj))
    }
}
