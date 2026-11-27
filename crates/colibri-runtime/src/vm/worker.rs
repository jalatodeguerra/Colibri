//! The `Vm` side of thread tasks: submitting a task from the main `Vm`, and building and running
//! an isolate on a worker thread (ARCHITECTURE §36.3).

use super::*;

use crate::tasks::{JobEntry, Shared, program_hash};
use colibri_core::portable::{NotSendable, Portable, RebuildError, Refused};
use colibri_core::taskrun::failure_of_panic;
use colibri_vm::{WalkRole, on_walk_stack};
use std::sync::atomic::AtomicBool;

/// How many steps a running task goes between two looks at the pool's cancel flag.
const CANCEL_SLICE: u32 = 4096;

impl Vm {
    /// How many threads the pool of this `Vm` runs once it has started ([`VmBuilder::worker_threads`]).
    #[doc(hidden)]
    pub fn worker_pool_size(&self) -> usize {
        self.launch.pool().size()
    }

    /// What the pool counts: threads started and alive, worker `Vm`s built, tasks run. The
    /// counters stay readable after the `Vm` is dropped, which is how a test sees that every
    /// thread ended.
    #[doc(hidden)]
    pub fn worker_stats(&self) -> Arc<PoolStats> {
        Arc::clone(&self.stats)
    }

    /// How many `Task` handles of this `Vm` are alive (a task whose handle was dropped is
    /// detached and no longer counts).
    #[doc(hidden)]
    pub fn task_handles(&self) -> usize {
        self.engine.core().task_count()
    }

    /// The image a worker builds its isolate from, once a program is loaded.
    #[doc(hidden)]
    pub fn program_image(&self) -> Option<Arc<ProgramImage>> {
        self.launch.image()
    }

    /// Makes the next task of this `Vm` carry a hash that its workers will not reproduce, so that
    /// the image check refuses it (tests of the check).
    #[doc(hidden)]
    pub fn corrupt_image_hash(&mut self) {
        self.launch.corrupt_hash();
    }

    /// The hash of the loaded program's tables that every worker must reproduce.
    #[doc(hidden)]
    pub fn program_table_hash(&mut self) -> Option<u64> {
        self.launch.hash()
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
        let Some((image, hash)) = self.launch.spec() else {
            return Err(TaskFailure::Internal(
                "no program is loaded that a thread task could run".to_string(),
            ));
        };
        let core = self.engine.core();
        let budget = core.budget.allotment;
        let account = (core.mem.tag(), core.memory_limit());
        self.launch.pool().submit(Submission {
            image,
            hash,
            entry: JobEntry::Call {
                function: function.to_string(),
                args,
            },
            budget,
            account,
            serial: 0,
        })
    }

    // ---- on a worker thread ----

    /// Builds the isolate of a pool thread: a `Vm` with what `worker_setup` registers, holding
    /// the program of `image`. Refuses with a reason when the image does not match.
    pub(crate) fn build_worker(
        cfg: &WorkerCfg,
        image: &ProgramImage,
        expected_hash: u64,
        shared: &Arc<Shared>,
    ) -> Result<Vm, String> {
        let cancel: &Arc<AtomicBool> = &shared.cancel;
        let mut builder = Vm::builder()
            .build_cfg(image.cfg.clone())
            .max_call_depth(cfg.max_call_depth)
            .native_stack_budget(crate::tasks::WORKER_NATIVE_BUDGET);
        builder.caps = cfg.caps;
        if let WorkerOut::Shared(shared) = &cfg.out {
            builder.shared_out = Some(Arc::clone(shared));
        }
        let mut vm = builder.build();
        vm.engine
            .core_mut()
            .set_cancel_flag(Some(Arc::clone(cancel)));
        // A task of this isolate queues the tasks it starts on the pool it runs in.
        let launch = Rc::new(WorkerLaunch::new(Arc::clone(shared)));
        vm.engine
            .core_mut()
            .set_task_spawner(Some(Rc::clone(&launch) as Rc<dyn TaskSpawner>));
        vm.worker_launch = Some(launch);
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
    /// parent's budget and memory account, and copies the result out. `image` and `hash` say which
    /// program the tasks it starts are for, and `serial` names the task (see
    /// `colibri_core::taskrun::TaskScope`).
    pub(crate) fn run_task(
        &mut self,
        entry: JobEntry,
        budget: Option<u64>,
        account: (u64, Option<u64>),
        serial: u64,
        program: (&Arc<ProgramImage>, u64),
    ) -> Result<TaskDone, TaskFailure> {
        let program_fns = self
            .program
            .clone()
            .ok_or_else(|| TaskFailure::Internal("the worker has no program".to_string()))?;
        let entry = match entry {
            JobEntry::Call { function, args } => {
                let Some(f) = program_fns.fn_by_name(&function) else {
                    return Err(TaskFailure::Internal(format!(
                        "the program has no function `{function}`"
                    )));
                };
                Run::Call(f, args)
            }
            JobEntry::Closure(closure) => Run::Closure(closure),
        };
        if let Some(launch) = &self.worker_launch {
            launch.enter(program.0, program.1, serial);
        }
        {
            let core = self.engine.core_mut();
            core.set_instruction_budget(budget);
            core.adopt_memory_account(account.0, account.1);
            // The pool asks a task to stop (the `Vm` was dropped) at a safepoint.
            if let Some(every) = std::num::NonZeroU32::new(CANCEL_SLICE) {
                core.want_safepoints(colibri_core::safepoint::SafepointKind::Cancel, every);
            }
        }
        // Everything the task allocates, the rebuilt arguments and the result's portable form
        // included, is charged to the parent's account.
        let entry_account = self.engine.core_mut().mem.enter();
        let restore = self.engine.core_mut().enter_task_scope(serial);
        let outcome = self.run_task_in(entry);
        restore(self.engine.core_mut());
        // A coroutine the task left must not run inside the next task of this isolate (nor hold
        // memory of the parent's account): it is cancelled, newest first, its `defer`s run, as
        // at the end of a program. The budget and the limit it runs under are the task's own,
        // renewed, so that a task that used them up still cleans up.
        {
            let core = self.engine.core_mut();
            core.set_instruction_budget(budget);
            core.set_memory_limit(account.1);
        }
        self.cancel_coroutines();
        self.engine.core_mut().mem.leave(entry_account);
        self.engine.core_mut().flush();
        match outcome {
            // The pool asked: the budget's unwinding was a cancellation.
            Err(TaskFailure::BudgetExhausted) if self.engine.core().cancelled() => {
                Err(TaskFailure::Cancelled)
            }
            other => other,
        }
    }

    fn run_task_in(&mut self, run: Run) -> Result<TaskDone, TaskFailure> {
        let result = match run {
            Run::Call(f, args) => {
                let mut values = Vec::with_capacity(args.len());
                for arg in args {
                    match self.rebuild(arg) {
                        Ok(v) => values.push(v),
                        Err(failure) => {
                            for v in values {
                                self.engine.release_in_entry(v);
                            }
                            return Err(failure);
                        }
                    }
                }
                self.engine.call(f, values)
            }
            Run::Closure(closure) => {
                let f = self.rebuild(closure)?;
                let result = self.engine.call_function(&f, Vec::new());
                self.engine.release_in_entry(f);
                result
            }
        };
        let steps = self.engine.core().steps_used();
        match result {
            Err(info) => Err(failure_of_panic(self.engine.core(), *info)),
            Ok(value) => {
                let copied: Result<Portable, NotSendable> = on_walk_stack(
                    WalkRole::Sender,
                    crate::tasks::WORKER_NATIVE_BUDGET,
                    |guard| self.engine.to_portable(&value, guard),
                );
                self.engine.release_in_entry(value);
                match copied {
                    Ok(value) => Ok(TaskDone { value, steps }),
                    // A result too large to copy back is the memory limit's failure, as the
                    // arguments' rebuilding is.
                    Err(e) if e.refused == Refused::MemoryLimit => Err(TaskFailure::MemoryLimit {
                        limit: self.engine.core().memory_limit().unwrap_or(0),
                    }),
                    Err(e) => Err(TaskFailure::Copy(e.to_string())),
                }
            }
        }
    }

    /// Rebuilds a value that was sent to this isolate. Rebuilding it past the memory limit is the
    /// memory limit's failure, not a failure to send.
    fn rebuild(&mut self, portable: Portable) -> Result<colibri_core::value::Value, TaskFailure> {
        on_walk_stack(
            WalkRole::Receiver,
            crate::tasks::WORKER_NATIVE_BUDGET,
            |guard| colibri_core::portable::from_portable(portable, self.engine.core_mut(), guard),
        )
        .map_err(|e| match e {
            RebuildError::MemoryLimit => TaskFailure::MemoryLimit {
                limit: self.engine.core().memory_limit().unwrap_or(0),
            },
            other => TaskFailure::Copy(other.to_string()),
        })
    }
}

/// What a worker runs, with the function resolved.
enum Run {
    Call(colibri_check::hir::FnId, Vec<Portable>),
    Closure(Portable),
}
