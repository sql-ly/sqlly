//! Settings a newer SQLLY may write with values this build does not know.
//! A notebook carrying one still opens: the value loads as the enum's
//! `Unknown` variant, behaves like the setting's default, and is written
//! back unchanged. The text is interned so those enums stay `Copy`.

use std::collections::BTreeSet;
use std::sync::Mutex;

static INTERNED: Mutex<BTreeSet<&'static str>> = Mutex::new(BTreeSet::new());

/// `text` with a `'static` lifetime, kept once per distinct value.
pub(crate) fn intern(text: &str) -> &'static str {
    let mut set = INTERNED.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(known) = set.get(text) {
        return known;
    }
    let leaked: &'static str = Box::leak(text.to_string().into_boxed_str());
    set.insert(leaked);
    leaked
}

/// A unit enum serialized as snake_case names, with `Unknown(&'static str)`
/// for names from a newer SQLLY.
macro_rules! lenient_names {
    ($ty:ident { $($variant:ident => $name:literal),+ $(,)? }) => {
        impl $ty {
            fn name(self) -> &'static str {
                match self {
                    $($ty::$variant => $name,)+
                    $ty::Unknown(name) => name,
                }
            }
        }

        impl serde::Serialize for $ty {
            fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.serialize_str(self.name())
            }
        }

        impl<'de> serde::Deserialize<'de> for $ty {
            fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                let name = String::deserialize(deserializer)?;
                Ok(match name.as_str() {
                    $($name => $ty::$variant,)+
                    _ => $ty::Unknown($crate::notebook::lenient::intern(&name)),
                })
            }
        }
    };
}

pub(crate) use lenient_names;
