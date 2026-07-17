use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Cursor(u64);
#[derive(Debug, Error)]
pub enum CursorError {
    #[error("invalid decimal cursor: {0}")]
    Invalid(#[from] std::num::ParseIntError),
    #[error("cursor sequence is not monotonic")]
    NonMonotonic,
}
impl Cursor {
    pub const fn new(inner: u64) -> Self {
        Self(inner)
    }
    pub const fn into_inner(self) -> u64 {
        self.0
    }
    pub fn as_string(&self) -> String {
        self.0.to_string()
    }
    pub fn from_string(s: &str) -> Result<Self, std::num::ParseIntError> {
        s.parse().map(Self)
    }
    pub const fn next(self) -> Self {
        Self(self.0.saturating_add(1))
    }
    pub const fn is_consecutive_after(self, p: Self) -> bool {
        self.0 == p.0.saturating_add(1)
    }
}
impl std::fmt::Display for Cursor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}
impl Serialize for Cursor {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.as_string())
    }
}
impl<'de> Deserialize<'de> for Cursor {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Self::from_string(&s).map_err(serde::de::Error::custom)
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Watermark {
    pub cursor: Cursor,
}
pub fn validate_monotonic(previous: Cursor, next: Cursor) -> Result<(), CursorError> {
    if next.is_consecutive_after(previous) {
        Ok(())
    } else {
        Err(CursorError::NonMonotonic)
    }
}
