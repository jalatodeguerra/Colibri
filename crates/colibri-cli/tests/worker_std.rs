//! The CLI's `Vm` hands the standard library to thread tasks' workers (phase 3 L17): a worker
//! installs it with `worker_setup`, with the capabilities of the main `Vm`.

use colibri_lang as lang;
use colibri_runtime::tasks::Portable;

#[test]
fn a_worker_has_the_standard_library_and_the_scripts_arguments() {
    lang::on_big_stack(|| {
        let builder = lang::vm_builder().allow_all().worker_threads(1);
        let mut vm = lang::build_vm(builder, vec!["one".to_string(), "two".to_string()]);
        vm.load_source(
            "worker_std.colibri",
            "import fs\nimport math\nimport os\n\nfn root() -> float {\n    return math.sqrt(16.0)\n}\n\nfn count() -> int {\n    return os.args().len()\n}\n\nfn here() -> bool {\n    return fs.exists(\".\")\n}\n",
        )
        .unwrap();
        let mut ask = |name: &str| {
            vm.submit_portable(name, vec![])
                .unwrap()
                .wait()
                .unwrap_or_else(|e| panic!("{name}: {e}"))
                .value
        };
        assert!(matches!(ask("root"), Portable::Float(x) if x == 4.0));
        assert!(matches!(ask("count"), Portable::Int(2)));
        // `fs` needs a capability: the worker has the one the main `Vm` was granted.
        assert!(matches!(ask("here"), Portable::Bool(true)));
    });
}
