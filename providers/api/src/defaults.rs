/// Declare a record and keep its nonstandard defaults beside its fields.
/// Expands to an ordinary struct and `Default`; field and serde attributes
/// are forwarded unchanged. Omitted initializers use the field type's default.
#[macro_export]
macro_rules! default_struct {
    (
        $(#[$attribute:meta])*
        $visibility:vis struct $name:ident {
            $(
                $(#[$field_attribute:meta])*
                $field_visibility:vis $field:ident: $field_type:ty $(= $value:expr)?,
            )*
        }
    ) => {
        $(#[$attribute])*
        $visibility struct $name {
            $($(#[$field_attribute])* $field_visibility $field: $field_type,)*
        }
        impl ::core::default::Default for $name {
            fn default() -> Self {
                Self { $($field: $crate::default_struct!(@value $($value)?),)* }
            }
        }
    };
    (@value $value:expr) => { $value };
    (@value) => { ::core::default::Default::default() };
}
