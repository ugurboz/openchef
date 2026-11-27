//! `string_enum!`: a closed vocabulary with a fixed string form.

/// Defines a fieldless `enum` whose variants each have one fixed string form, with
/// `as_str`, `ALL`, `Display`, `FromStr` and `serde` implemented from that form.
///
/// Variants are ordered as declared (`Ord`), which is what levels such as `Importance`
/// rely on. Parsing an unknown string returns [`crate::Error::UnknownValue`]. Other
/// crates use it as `openchef_core::string_enum!` for their own closed vocabularies.
#[macro_export]
macro_rules! string_enum {
    (
        $(#[$meta:meta])*
        $vis:vis enum $name:ident: $kind:literal {
            $( $(#[$vmeta:meta])* $variant:ident => $text:literal ),+ $(,)?
        }
    ) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
        $vis enum $name {
            $( $(#[$vmeta])* $variant ),+
        }

        impl $name {
            /// Every variant, in declaration order.
            pub const ALL: &'static [Self] = &[ $( Self::$variant ),+ ];

            /// The fixed string form (stored, sent over the API, shown to users).
            #[must_use]
            pub const fn as_str(self) -> &'static str {
                match self {
                    $( Self::$variant => $text ),+
                }
            }
        }

        impl ::std::fmt::Display for $name {
            fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                f.write_str(self.as_str())
            }
        }

        impl ::std::str::FromStr for $name {
            type Err = $crate::Error;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                match s {
                    $( $text => Ok(Self::$variant), )+
                    other => Err($crate::Error::UnknownValue {
                        kind: $kind,
                        value: other.to_owned(),
                    }),
                }
            }
        }

        impl $crate::__serde::Serialize for $name {
            fn serialize<S: $crate::__serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.serialize_str(self.as_str())
            }
        }

        impl<'de> $crate::__serde::Deserialize<'de> for $name {
            fn deserialize<D: $crate::__serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                let text = <::std::borrow::Cow<'de, str> as $crate::__serde::Deserialize>::deserialize(deserializer)?;
                text.parse().map_err($crate::__serde::de::Error::custom)
            }
        }
    };
}
