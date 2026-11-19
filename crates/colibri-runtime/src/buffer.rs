//! A clonable output sink, for capturing what a script prints.

use std::cell::RefCell;
use std::io::{self, Write};
use std::rc::Rc;

/// An in-memory writer whose clones share one buffer. Give one clone to
/// [`crate::VmBuilder::stdout`] and keep the other to read the output:
///
/// ```
/// use colibri_runtime as colibri;
/// let out = colibri::SharedBuffer::new();
/// let mut vm = colibri::Vm::builder().stdout(Box::new(out.clone())).build();
/// vm.load_source("hi.colibri", "print(\"hi\")").unwrap();
/// assert_eq!(out.contents(), "hi\n");
/// ```
#[derive(Clone, Debug, Default)]
pub struct SharedBuffer(Rc<RefCell<Vec<u8>>>);

impl SharedBuffer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Everything written so far, as text.
    pub fn contents(&self) -> String {
        String::from_utf8_lossy(&self.0.borrow()).into_owned()
    }

    /// Returns what was written and empties the buffer.
    pub fn take(&self) -> String {
        let bytes = std::mem::take(&mut *self.0.borrow_mut());
        String::from_utf8_lossy(&bytes).into_owned()
    }
}

impl Write for SharedBuffer {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.borrow_mut().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
