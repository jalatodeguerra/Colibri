//! Struct values that cross the host boundary field by field, for types that are not `Copy`
//! (`#[colibri::export(value)]` asserts `Copy`, and `process.Output` holds strings).

/// Implements `IntoColibri` and `FromColibri` for a Rust struct as the Colibri struct of the
/// same name, with the fields in the order written. The names must be those of the interface.
macro_rules! value_struct {
    ($name:ident { $($field:ident : $ty:ty),+ $(,)? }) => {
        impl colibri_runtime::IntoColibri for $name {
            fn ty(reg: &mut colibri_runtime::TypeRegistry) -> colibri_runtime::TypeDesc {
                let fields = vec![
                    $((stringify!($field), <$ty as colibri_runtime::IntoColibri>::ty(reg))),+
                ];
                reg.define_struct(stringify!($name), fields)
            }
            fn into_colibri(self, cx: &mut colibri_runtime::Cx<'_>) -> colibri_runtime::Value {
                let fields = vec![
                    $(<$ty as colibri_runtime::IntoColibri>::into_colibri(self.$field, cx)),+
                ];
                cx.new_struct(stringify!($name), fields)
            }
        }
        impl colibri_runtime::FromColibri for $name {
            fn ty(reg: &mut colibri_runtime::TypeRegistry) -> colibri_runtime::TypeDesc {
                <Self as colibri_runtime::IntoColibri>::ty(reg)
            }
            #[allow(unused_assignments)]
            fn from_colibri(
                value: &colibri_runtime::Value,
                cx: &mut colibri_runtime::Cx<'_>,
            ) -> Result<Self, colibri_runtime::Error> {
                let mut i = 0usize;
                Ok($name {
                    $($field: {
                        let field = value.field(i).ok_or_else(|| {
                            colibri_runtime::Error::Convert(format!(
                                "expected a `{}` struct with a field at position {i}",
                                stringify!($name)
                            ))
                        })?;
                        i += 1;
                        <$ty as colibri_runtime::FromColibri>::from_colibri(&field, cx)?
                    }),+
                })
            }
        }
    };
}

pub(crate) use value_struct;
