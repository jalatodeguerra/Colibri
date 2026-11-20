//! Growing a loaded program entry by entry, for `colibri repl` (ARCHITECTURE §21).

use super::*;

/// A session source that was checked and can be [`Vm::session_commit`]ted.
pub struct SessionCheck {
    checked: Checked,
}

impl SessionCheck {
    /// The type of module-level value `name`, as written in source.
    pub fn global_type(&self, name: &str) -> Option<String> {
        let p = &self.checked.program;
        let g = p.globals.iter().find(|g| g.name == name)?;
        Some(g.ty.render(&p.types))
    }

    /// Whether a top-level statement of the entry makes a closure.
    pub fn script_has_closures(&self) -> bool {
        self.checked
            .program
            .fns
            .iter()
            .any(|f| f.name == "<closure in <script>>")
    }

    /// Whether the checked program has a function called `name`.
    pub fn has_fn(&self, name: &str) -> bool {
        self.checked.program.fn_by_name(name).is_some()
    }

    /// The warnings of the checked program (lints such as a use of a deprecated function).
    pub fn warnings(&self) -> &[Diagnostic] {
        &self.checked.warnings
    }

    /// The sources the diagnostics of this check point into (for a failed check, see
    /// [`Error::Compile`]).
    pub fn sources(&self) -> &SourceMap {
        &self.checked.sources
    }
}

/// Why [`Vm::session_commit`] did not take an entry.
#[derive(Debug)]
pub enum SessionError {
    /// The entry would renumber functions, types or module-level values the running program
    /// holds; nothing changed.
    Conflict(String),
    /// The entry's initialisers or statements failed; nothing changed (but for what they did
    /// to values that already existed).
    Failed(Error),
}

impl Vm {
    /// Starts (or restarts) a session: an empty program is loaded.
    pub fn session_start(&mut self) -> Result<(), Error> {
        self.reload = None;
        self.teardown();
        // Later entries may import anything: have every module registered, so the host types
        // keep their places as the session grows.
        self.materialize_all()?;
        self.load_source("<repl>", "")
    }

    /// Checks `source`, the whole session program, against the host without loading it.
    pub fn session_check(&self, source: &str) -> Result<SessionCheck, Error> {
        let mut fs = MemoryFs::new();
        fs.insert("<repl>", source);
        // A session has every module registered (`session_start`), so nothing is left to do
        // between reading the program and checking it.
        let (sources, loaded) =
            self.read_program(&fs, Entry::File(Path::new("<repl>")), LoadMode::Session)?;
        self.check_imports_not_failed(&loaded)?;
        let checked = self.check_loaded(sources, loaded, LoadMode::Session)?;
        Ok(SessionCheck { checked })
    }

    /// Puts a checked session program to work: it must extend the loaded one (same functions,
    /// types and module-level values in the same order, new ones after). The initialisers of
    /// the new module-level values and of `reinit` run, then the top-level statements.
    /// `replaced` names the functions whose definition was replaced in place; closures made
    /// inside them, inside a `reinit` value's initialiser and by earlier top-level statements
    /// stop working.
    pub fn session_commit(
        &mut self,
        check: SessionCheck,
        reinit: &[&str],
        replaced: &[&str],
    ) -> Result<(), SessionError> {
        let Some(old) = self.program.clone() else {
            return Err(SessionError::Conflict("no session is running".to_string()));
        };
        let Checked {
            sources, program, ..
        } = check.checked;
        let mut gone: Vec<String> = replaced.iter().map(|s| s.to_string()).collect();
        gone.extend(reinit.iter().map(|g| format!("<init {g}>")));
        gone.push("<script>".to_string());
        let map = closure_map(&old, &program, &gone, reinit).map_err(SessionError::Conflict)?;
        let reinit_ids: Vec<colibri_check::hir::GlobalId> = program
            .globals
            .iter()
            .enumerate()
            .filter(|(_, g)| reinit.contains(&g.name.as_str()))
            .map(|(i, _)| colibri_check::hir::GlobalId(i as u32))
            .collect();
        let new_from = old.globals.len();
        let old_sources = Rc::clone(&self.sources);
        let undo = self.interp.session_extend(&program, map, &reinit_ids);
        self.sources = Rc::new(sources);
        self.interp
            .set_owner((self.id, self.epoch), true, Rc::clone(&self.sources));
        let outcome = self
            .interp
            .session_run(&mut self.fiber, &program, new_from, &reinit_ids);
        self.interp.flush();
        match outcome {
            Ok(()) => {
                self.interp.session_commit(undo);
                self.program = Some(Rc::new(program));
                Ok(())
            }
            Err(info) => {
                let error = panic_error(&self.interp, *info);
                self.interp.session_rollback(undo, &old);
                self.sources = old_sources;
                self.interp
                    .set_owner((self.id, self.epoch), true, Rc::clone(&self.sources));
                Err(SessionError::Failed(error))
            }
        }
    }

    /// The `Show` text of module-level value `name` and its type, for the session to print.
    pub fn session_display(&mut self, name: &str) -> Option<(String, String)> {
        let program = self.program.clone()?;
        let i = program.globals.iter().position(|g| g.name == name)?;
        let ty = program.globals[i].ty.clone();
        let type_text = ty.render(&program.types);
        let id = colibri_check::hir::GlobalId(i as u32);
        let text = match self.interp.session_show(&mut self.fiber, id) {
            Ok(t) => t,
            Err(_) => "<error while showing the value>".to_string(),
        };
        let text = match ty {
            Ty::Str => format!("{text:?}"),
            Ty::Char => format!("{:?}", text.chars().next().unwrap_or(' ')),
            _ => text,
        };
        Some((text, type_text))
    }
}

/// How each function of `old` is numbered in `new`; `u32::MAX` for one that is gone. Declared
/// functions must keep their place; closures follow all declared functions, so they are matched
/// by the function they were made in and their order there.
fn closure_map(
    old: &Program,
    new: &Program,
    gone: &[String],
    reinit: &[&str],
) -> Result<Vec<u32>, String> {
    let is_closure = |n: &str| n.starts_with("<closure in ");
    let declared = |p: &Program| p.fns.iter().take_while(|f| !is_closure(&f.name)).count();
    let (d_old, d_new) = (declared(old), declared(new));
    let tidy = |p: &Program, d: usize| p.fns[d..].iter().all(|f| is_closure(&f.name));
    if !tidy(old, d_old) || !tidy(new, d_new) || d_new < d_old {
        return Err("the functions of the session changed their order".to_string());
    }
    for i in 0..d_old {
        let (a, b) = (&old.fns[i], &new.fns[i]);
        if a.name != b.name || a.host != b.host {
            return Err(format!(
                "this would renumber `{}`; `:reset` starts over",
                a.name
            ));
        }
        if a.sig != b.sig && !gone.contains(&a.name) {
            return Err(format!("`{}` changed its signature", a.name));
        }
    }
    if new.types.len() < old.types.len()
        || old
            .types
            .iter()
            .zip(&new.types)
            .any(|(a, b)| a.name != b.name)
    {
        return Err("this changes the types of the session; `:reset` starts over".to_string());
    }
    if new.host_fns.len() != old.host_fns.len() {
        return Err("this changes the host functions of the session".to_string());
    }
    if new.globals.len() < old.globals.len() {
        return Err("this removes module-level values of the session".to_string());
    }
    for (a, b) in old.globals.iter().zip(&new.globals) {
        if a.name != b.name || (a.ty != b.ty && !reinit.contains(&a.name.as_str())) {
            return Err(format!("`{}` changed its type or place", a.name));
        }
    }
    let mut map: Vec<u32> = (0..d_old as u32).collect();
    let mut seen: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for f in &old.fns[d_old..] {
        let owner = f
            .name
            .strip_prefix("<closure in ")
            .and_then(|n| n.strip_suffix('>'))
            .unwrap_or(&f.name);
        let k = seen.entry(f.name.as_str()).or_insert(0);
        let nth = *k;
        *k += 1;
        if gone.iter().any(|g| g == owner) {
            map.push(u32::MAX);
            continue;
        }
        let found = new.fns[d_new..]
            .iter()
            .enumerate()
            .filter(|(_, g)| g.name == f.name)
            .nth(nth)
            .map(|(j, _)| (d_new + j) as u32);
        map.push(found.unwrap_or(u32::MAX));
    }
    Ok(map)
}
