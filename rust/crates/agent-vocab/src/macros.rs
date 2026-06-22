/// Generate a string-backed enum with `as_str`, `Display`, `FromStr`, and
/// serde impls from `Variant => "wire_string"` pairs.
#[macro_export]
macro_rules! text_enum {
    ($(
        $(#[$meta:meta])*
        pub enum $name:ident {
            $($(#[$variant_meta:meta])* $variant:ident => $wire:literal),+ $(,)?
        }
    )+) => {
        $(
            $(#[$meta])*
            #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
            pub enum $name {
                $($(#[$variant_meta])* $variant),+
            }

            impl $name {
                pub fn as_str(self) -> &'static str {
                    match self {
                        $(Self::$variant => $wire),+
                    }
                }
            }

            impl ::std::fmt::Display for $name {
                fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                    f.write_str(self.as_str())
                }
            }

            impl ::std::str::FromStr for $name {
                type Err = String;

                fn from_str(value: &str) -> Result<Self, Self::Err> {
                    match value {
                        $($wire => Ok(Self::$variant),)+
                        other => Err(format!(
                            "unknown {}: {other}",
                            stringify!($name),
                        )),
                    }
                }
            }

            impl ::serde::Serialize for $name {
                fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
                where
                    S: ::serde::Serializer,
                {
                    serializer.serialize_str(self.as_str())
                }
            }

            impl<'de> ::serde::Deserialize<'de> for $name {
                fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
                where
                    D: ::serde::Deserializer<'de>,
                {
                    let value = String::deserialize(deserializer)?;
                    <Self as ::std::str::FromStr>::from_str(&value)
                        .map_err(::serde::de::Error::custom)
                }
            }
        )+
    };
}
