//! Task ids.

use std::fmt;
use std::str::FromStr;

use crate::Error;

const ID_LEN: usize = 10;

/// Crockford's base32 in lower case: no i, l, o or u to misread.
const ALPHABET: &[u8; 32] = b"0123456789abcdefghjkmnpqrstvwxyz";

/// Identifies a task. Short enough to type (`openchef status k3m9x2q7ab`) and safe in
/// branch names and paths. A `TaskId` can only be built by [`TaskId::generate`] or by
/// parsing, so every value is valid.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TaskId(String);

impl TaskId {
    /// A new random id: 50 bits from the operating system's random source.
    pub fn generate() -> Result<Self, Error> {
        let mut bytes = [0_u8; 7];
        getrandom::fill(&mut bytes).map_err(|e| Error::Entropy(e.to_string()))?;
        let value = bytes
            .iter()
            .fold(0_u64, |acc, b| (acc << 8) | u64::from(*b));
        // The first 50 of 56 bits, five at a time, most significant first: the same
        // characters Go's base32 encoding of these bytes starts with.
        let id = (0..ID_LEN)
            .map(|i| {
                let shift = 56 - 5 * (i + 1);
                // The mask keeps the index below 32, so the cast and the lookup are exact.
                let index = usize::try_from((value >> shift) & 0x1f).unwrap_or(0);
                char::from(ALPHABET[index])
            })
            .collect();
        Ok(Self(id))
    }

    /// The id as typed by users and stored.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for TaskId {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.chars().count() != ID_LEN {
            return Err(Error::Invalid(format!(
                "task id {s:?}: want {ID_LEN} characters"
            )));
        }
        if let Some(bad) = s
            .chars()
            .find(|c| !c.is_ascii() || !ALPHABET.contains(&(*c as u8)))
        {
            return Err(Error::Invalid(format!(
                "task id {s:?}: unexpected character {bad:?}"
            )));
        }
        Ok(Self(s.to_owned()))
    }
}

impl fmt::Display for TaskId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl serde::Serialize for TaskId {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> serde::Deserialize<'de> for TaskId {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = <std::borrow::Cow<'de, str> as serde::Deserialize>::deserialize(deserializer)?;
        text.parse().map_err(serde::de::Error::custom)
    }
}
