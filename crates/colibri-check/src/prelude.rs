//! The prelude: the built-in interfaces of DESIGN §9.2, written in Colibri.
//!
//! The text is parsed once and its interfaces are declared before the user's file, so their
//! type ids are the constants of [`crate::types`] (`EQ`, `ORD`, ...). Their implementations
//! for built-in types are derived by the checker (§7.4), not written here.
//!
//! `Ord.cmp` returns an `int`: negative, zero or positive (ARCHITECTURE §6). `IntoError` is
//! the interface of types that convert to `Error` (DESIGN §10.1); its default `message` is the
//! type's `Show`. `Error` itself is declared as an empty class so that the checker treats it
//! as the reference type it is; it has no fields to build and its methods are built in
//! (`crate::optres`).

use std::sync::OnceLock;

use colibri_syntax::ast::File;
use colibri_syntax::{FileId, Span};

/// The source file id of the prelude; its spans never point into a user's file, and
/// diagnostics never label them.
pub(crate) const PRELUDE_FILE: FileId = FileId(u32::MAX);

const SOURCE: &str = r#"
interface Eq {
    fn eq(self, other: Self) -> bool
}
interface Ord {
    fn cmp(self, other: Self) -> int
}
interface Hash {
    fn hash(self) -> int
}
interface Show {
    fn show(self) -> str
}
interface Clone {
    fn clone(self) -> Self
}
interface Drop {
    fn drop(self)
}
interface Iter[T] {
    fn next(var self) -> T?
}
interface Iterable[T] {
    fn iter(self) -> Iter[T]
}
interface IntoError {
    fn message(self) -> str => "{self}"
}
class Error {}
"#;

/// The parsed prelude.
pub(crate) fn prelude() -> &'static File {
    static PARSED: OnceLock<File> = OnceLock::new();
    PARSED.get_or_init(|| colibri_syntax::parse_file(PRELUDE_FILE, SOURCE).0)
}

/// Whether a span belongs to the prelude.
pub(crate) fn in_prelude(span: Span) -> bool {
    span.file == PRELUDE_FILE
}
