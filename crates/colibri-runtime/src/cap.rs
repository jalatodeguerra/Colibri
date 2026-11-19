//! Capabilities (EMBEDDING §5): what a host lets scripts reach. A host function tagged with
//! `.requires(Cap::Fs)` can only be called by scripts when the host granted that capability;
//! a script that uses one that was not granted fails to check, at the call.

use std::fmt;

/// A kind of access a host item can need. The standard library tags its modules with these too.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[non_exhaustive]
pub enum Cap {
    /// The file system.
    Fs,
    /// The operating system: processes, environment, clocks.
    Os,
    /// The network.
    Net,
    /// Foreign functions: `extern` blocks.
    Ffi,
}

impl Cap {
    /// Every capability.
    pub const ALL: [Cap; 4] = [Cap::Fs, Cap::Os, Cap::Net, Cap::Ffi];

    /// The name used in interface files (`@requires(fs)`) and messages.
    pub fn name(self) -> &'static str {
        match self {
            Cap::Fs => "fs",
            Cap::Os => "os",
            Cap::Net => "net",
            Cap::Ffi => "ffi",
        }
    }

    fn bit(self) -> u8 {
        1 << (self as u8)
    }
}

impl fmt::Display for Cap {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// A set of capabilities.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CapSet(u8);

impl CapSet {
    /// No capability.
    pub const fn none() -> Self {
        CapSet(0)
    }

    /// Every capability.
    pub fn all() -> Self {
        Cap::ALL.into_iter().collect()
    }

    pub fn insert(&mut self, cap: Cap) {
        self.0 |= cap.bit();
    }

    pub fn contains(&self, cap: Cap) -> bool {
        self.0 & cap.bit() != 0
    }

    pub fn is_empty(&self) -> bool {
        self.0 == 0
    }

    /// The capabilities in the set, in a fixed order.
    pub fn iter(&self) -> impl Iterator<Item = Cap> + '_ {
        Cap::ALL.into_iter().filter(|c| self.contains(*c))
    }
}

impl FromIterator<Cap> for CapSet {
    fn from_iter<I: IntoIterator<Item = Cap>>(iter: I) -> Self {
        let mut set = CapSet::none();
        for cap in iter {
            set.insert(cap);
        }
        set
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sets_hold_what_was_put_in() {
        let mut set = CapSet::none();
        assert!(set.is_empty());
        set.insert(Cap::Net);
        assert!(set.contains(Cap::Net) && !set.contains(Cap::Fs));
        assert_eq!(CapSet::all().iter().count(), Cap::ALL.len());
        assert_eq!(set.iter().map(Cap::name).collect::<Vec<_>>(), ["net"]);
    }
}
