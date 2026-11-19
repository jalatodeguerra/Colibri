//! The `vec` module: what makes the vector types nameable (DESIGN §15.1).
//!
//! The types `vec2`..`vec4` and `ivec2`..`ivec4` are built into the checker and the interpreter
//! (constructors, fields, swizzles, operators, methods); `import vec` brings their names into a
//! script. The module is the standard library's, `colibri_std::install` registers it, and a host
//! that does not use the standard library but wants vectors in its scripts calls
//! [`install_vec`].

use glam::Vec2;

use crate::{Error, Vm};

/// Registers the standard library's `vec` module (`std.vec`) into `vm`: the names of the vector
/// types and the two functions that belong to no one vector, `from_angle` and `angle`. It must
/// happen before a program is loaded; calling it twice is an error.
pub fn install_vec(vm: &mut Vm) -> Result<(), Error> {
    let mut m = vm.std_module("vec");
    // The unit vector that points `radians` counter-clockwise from the x axis.
    m.register_fn_named("from_angle", ["radians"], |radians: f32| {
        Vec2::new(radians.cos(), radians.sin())
    })?;
    // The angle of a vector from the x axis, in radians, in `-PI..=PI` (`0` for the zero vector).
    m.register_fn_named("angle", ["v"], |v: Vec2| v.y.atan2(v.x))?;
    Ok(())
}
