//! The host heap: the objects of the embedding program that scripts hold (EMBEDDING §3).

use colibri_check::types::TypeId;

/// What a script value holds for a host object. It names a slot of the host heap and the
/// generation the slot had when the reference was made.
#[derive(Debug)]
pub struct HostRef {
    /// The host class, in the loaded program.
    pub ty: TypeId,
    pub index: u32,
    pub generation: u32,
}
