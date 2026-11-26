//! The `Vm` side of thread tasks: submitting a task from the main `Vm`, and building and running
//! an isolate on a worker thread (ARCHITECTURE §36.3).

use super::*;

use colibri_core::portable::{NotSendable, Portable, RebuildError};
use colibri_vm::{WalkRole, on_walk_stack};

/// The failure of a task that panicked, as plain data.
fn failure_of(error: Error) -> TaskFailure {
    match error {
        Error::Panic {
            message,
            stack,
            location,
            ..
        } => {
            let mut trace = String::new();
            for frame in &stack {
                trace.push_str(&format!(
                    "{} ({}:{}:{})\n",
                    frame.function, frame.location.file, frame.location.line, frame.location.column
                ));
            }
            if trace.is_empty()
                && let Some(at) = location
            {
                trace = format!("({}:{}:{})\n", at.file, at.line, at.column);
            }
            TaskFailure::Panic { message, trace }
        }
        Error::BudgetExhausted { .. } => TaskFailure::BudgetExhausted,
        Error::MemoryLimit { limit, .. } => TaskFailure::MemoryLimit { limit },
        other => TaskFailure::Internal(other.to_string()),
    }
}

impl Vm {
    /// How many threads the pool of this `Vm` runs once it has started ([`VmBuilder::worker_threads`]).
    #[doc(hidden)]
    pub fn worker_pool_size(&self) -> usize {
        self.pool.size()
    }

    /// What the pool counts: threads started and alive, worker `Vm`s built, tasks run. The
    /// counters stay readable after the `Vm` is dropped, which is how a test sees that every
    /// thread ended.
    #[doc(hidden)]
    pub fn worker_stats(&self) -> Arc<PoolStats> {
        Arc::clone(&self.stats)
    }

    /// The image a worker builds its isolate from, once a program is loaded.
    #[doc(hidden)]
    pub fn program_image(&self) -> Option<Arc<ProgramImage>> {
        self.image.clone()
    }

    /// Makes the next task of this `Vm` carry a hash that its workers will not reproduce, so that
    /// the image check refuses it (tests of the check).
    #[doc(hidden)]
    pub fn corrupt_image_hash(&mut self) {
        let hash = self.image_hash_now();
        self.image_hash = hash.map(|h| h ^ 1);
    }

    /// The hash of the loaded program's tables that every worker must reproduce.
    #[doc(hidden)]
    pub fn program_table_hash(&mut self) -> Option<u64> {
        self.image_hash_now()
    }

    fn image_hash_now(&mut self) -> Option<u64> {
        if self.image_hash.is_none() {
            self.image_hash = self.program.as_deref().map(program_hash);
        }
        self.image_hash
    }

    /// Calls the function `function` of the loaded program on a worker thread with `args` and
    /// gives a handle to wait for its result. This is the substrate of `task.run`: the arguments
    /// and the result are [`Portable`]s, the worker runs under this `Vm`'s instruction budget
    /// (for each task) and memory account, and what it prints goes to this `Vm`'s writer when the
    /// host gave one with [`VmBuilder::stdout_shared`].
    ///
    /// Not part of the embedding API yet, hence hidden.
    #[doc(hidden)]
    pub fn submit_portable(
        &mut self,
        function: &str,
        args: Vec<Portable>,
    ) -> Result<PendingTask, TaskFailure> {
        let (Some(image), Some(hash)) = (self.image.clone(), self.image_hash_now()) else {
            return Err(TaskFailure::Internal(
                "no program is loaded that a thread task could run".to_string(),
            ));
        };
        let core = self.engine.core();
        let budget = core.budget.allotment;
        let account = (core.mem.tag(), core.memory_limit());
        self.pool.submit(Submission {
            image,
            hash,
            function: function.to_string(),
            args,
            budget,
            account,
        })
    }

    // ---- on a worker thread ----

    /// Builds the isolate of a pool thread: a `Vm` with what `worker_setup` registers, holding
    /// the program of `image`. Refuses with a reason when the image does not match.
    pub(crate) fn build_worker(
        cfg: &WorkerCfg,
        image: &ProgramImage,
        expected_hash: u64,
    ) -> Result<Vm, String> {
        let mut builder = Vm::builder()
            .build_cfg(image.cfg.clone())
            .max_call_depth(cfg.max_call_depth)
            .native_stack_budget(crate::tasks::WORKER_NATIVE_BUDGET);
        builder.caps = cfg.caps;
        if let WorkerOut::Shared(shared) = &cfg.out {
            builder.shared_out = Some(Arc::clone(shared));
        }
        let mut vm = builder.build();
        if let Some(setup) = &cfg.setup {
            catch_unwind(AssertUnwindSafe(|| setup(&mut vm))).map_err(|payload| {
                format!(
                    "worker_setup panicked: {}",
                    payload_message(payload.as_ref())
                )
            })?;
        }
        vm.load_image(image, expected_hash)?;
        Ok(vm)
    }

    /// Checks the registrations of a worker against the main `Vm`'s interface: it may register
    /// less (what is missing becomes a trap), never more or different.
    fn check_worker_interface(&self, image: &ProgramImage) -> Result<(), String> {
        for module in self.module_names() {
            let own = self.rendered_module(&module).text;
            let main = image
                .iface
                .iter()
                .find(|(m, _)| *m == module)
                .map(|(_, text)| text.as_str());
            for line in own.lines().filter(|l| !l.trim().is_empty()) {
                // The comment and the header (which carries the hash of what follows) are
                // not declarations.
                if line.starts_with("//") || line.starts_with("@interface(") {
                    continue;
                }
                if !main.is_some_and(|text| text.lines().any(|l| l == line)) {
                    return Err(format!(
                        "worker_setup registers `{}` in the module `{module}`, which the main isolate does not declare in that form: the worker's interface must be part of the main one",
                        line.trim()
                    ));
                }
            }
        }
        Ok(())
    }

    /// Reads the image, checks it against the main `Vm`'s interface, and loads it as an isolate:
    /// the `const`s are initialised and nothing else runs.
    fn load_image(&mut self, image: &ProgramImage, expected_hash: u64) -> Result<(), String> {
        let entry = image.entry.as_entry();
        let (sources, loaded) = self
            .read_program(&image.files, entry, LoadMode::Run)
            .map_err(|e| format!("the worker could not read the program image: {e}"))?;
        self.materialize_imports(&loaded).map_err(|e| {
            format!("the worker could not register the modules the program imports: {e}")
        })?;
        self.check_worker_interface(image)?;
        let env = self
            .host_env_from(&image.iface)
            .map_err(|e| e.to_string())?;
        let checked = self
            .check_loaded_in(&env, image.iface.clone(), sources, loaded, LoadMode::Run)
            .map_err(|e| match e {
                Error::Compile(report) => format!(
                    "the worker could not check the program image:\n{}",
                    report.render(false)
                ),
                other => other.to_string(),
            })?;
        let hash = program_hash(&checked.program);
        if hash != expected_hash {
            return Err(format!(
                "the program image does not match the main isolate's (the worker's tables hash to {hash:016x}, the main isolate's to {expected_hash:016x}): the worker checked a different program or host interface"
            ));
        }
        self.install_worker(checked).map_err(|e| e.to_string())
    }

    /// [`Vm::install`] for an isolate: installs the program as a worker, with a trap in every
    /// host function slot that `worker_setup` did not fill, and runs the `const` initialisers.
    fn install_worker(&mut self, checked: Checked) -> Result<(), Error> {
        let Checked {
            sources,
            program,
            warnings,
            ..
        } = checked;
        self.sources = Rc::new(sources);
        self.sync_debug_locator();
        self.warnings = warnings;
        self.epoch += 1;
        self.engine.load_worker(&program);
        self.install_host_classes();
        self.install_host_fns(&program)?;
        for (i, def) in program.host_fns.iter().enumerate() {
            if def.ext.is_some() || self.find_raw(&def.module, &def.name).is_some() {
                continue;
            }
            let message = format!(
                "`{}` is not available in a thread task (the host did not register it for workers)",
                def.name
            );
            let trap: HostFnImpl = Rc::new(move |_, args| {
                // The arguments are the caller's references; dropping them here is what the
                // trampoline would do after a call.
                drop(args);
                Err(HostFault::Message(message.clone()))
            });
            self.engine.set_host_fn(HostFnId(i as u32), trap, false);
        }
        self.engine
            .core_mut()
            .set_owner((self.id, self.epoch), true, Rc::clone(&self.sources));
        let outcome = self.engine.run_module_inits();
        self.engine.core_mut().flush();
        match outcome {
            Ok(()) => {
                self.program = Some(Rc::new(program));
                Ok(())
            }
            Err(info) => Err(panic_error(self.engine.core(), *info)),
        }
    }

    /// Runs one task on this isolate: rebuilds the arguments, calls the function under the
    /// parent's budget and memory account, and copies the result out.
    pub(crate) fn run_task(
        &mut self,
        function: &str,
        args: Vec<Portable>,
        budget: Option<u64>,
        account: (u64, Option<u64>),
    ) -> Result<TaskDone, TaskFailure> {
        let program = self
            .program
            .clone()
            .ok_or_else(|| TaskFailure::Internal("the worker has no program".to_string()))?;
        let Some(f) = program.fn_by_name(function) else {
            return Err(TaskFailure::Internal(format!(
                "the program has no function `{function}`"
            )));
        };
        {
            let core = self.engine.core_mut();
            core.set_instruction_budget(budget);
            core.adopt_memory_account(account.0, account.1);
        }
        // Everything the task allocates, the rebuilt arguments and the result's portable form
        // included, is charged to the parent's account.
        let entry_account = self.engine.core_mut().mem.enter();
        let outcome = self.run_task_in(f, args);
        self.engine.core_mut().mem.leave(entry_account);
        self.engine.core_mut().flush();
        outcome
    }

    fn run_task_in(
        &mut self,
        f: colibri_check::hir::FnId,
        args: Vec<Portable>,
    ) -> Result<TaskDone, TaskFailure> {
        let mut values = Vec::with_capacity(args.len());
        for arg in args {
            let rebuilt = on_walk_stack(WalkRole::Receiver, |guard| {
                colibri_core::portable::from_portable(arg, self.engine.core_mut(), guard)
            });
            match rebuilt {
                Ok(v) => values.push(v),
                Err(e) => {
                    for v in values {
                        self.engine.release_in_entry(v);
                    }
                    return Err(match e {
                        RebuildError::MemoryLimit => TaskFailure::MemoryLimit {
                            limit: self.engine.core().memory_limit().unwrap_or(0),
                        },
                        other => TaskFailure::Copy(other.to_string()),
                    });
                }
            }
        }
        let result = self.engine.call(f, values);
        let steps = self.engine.core().steps_used();
        match result {
            Err(info) => Err(failure_of(panic_error(self.engine.core(), *info))),
            Ok(value) => {
                let copied: Result<Portable, NotSendable> = on_walk_stack(WalkRole::Sender, |guard| {
                    self.engine.to_portable(&value, guard)
                });
                self.engine.release_in_entry(value);
                match copied {
                    Ok(value) => Ok(TaskDone { value, steps }),
                    Err(e) => Err(TaskFailure::Copy(e.to_string())),
                }
            }
        }
    }
}
