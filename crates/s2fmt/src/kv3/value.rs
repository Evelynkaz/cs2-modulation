//! The KV3 value tree (FORMATS.md 3.3, 3.4): a dynamically-typed, order-preserving document
//! model shared by the binary and text readers/writers.

/// A KV3 value flag (FORMATS.md 3.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flag {
    None,
    Resource,
    ResourceName,
    Panorama,
    SoundEvent,
    SubClass,
    EntityName,
}

/// A KV3 value.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Int(i64),
    UInt(u64),
    Double(f64),
    String(String),
    Blob(Vec<u8>),
    Array(Vec<Value>),
    Object(Object),
    /// Only ever constructed with `flag != Flag::None`.
    Flagged(Flag, Box<Value>),
}

/// A KV3 object: an ordered list of key/value entries. Duplicate keys are preserved as
/// written; [`Object::get`] returns the first match.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Object {
    entries: Vec<(String, Value)>,
}

impl Object {
    /// Creates an empty object.
    pub fn new() -> Self {
        Object {
            entries: Vec::new(),
        }
    }

    /// Creates an object with room for `capacity` entries.
    pub fn with_capacity(capacity: usize) -> Self {
        Object {
            entries: Vec::with_capacity(capacity),
        }
    }

    /// Appends a key/value entry, preserving any existing entry with the same key.
    pub fn push(&mut self, key: impl Into<String>, value: Value) {
        self.entries.push((key.into(), value));
    }

    /// Returns the value of the first entry with the given key, if any.
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.entries.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    /// Iterates over the entries in document order.
    pub fn iter(&self) -> impl Iterator<Item = &(String, Value)> {
        self.entries.iter()
    }

    /// Number of entries, including duplicates.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// True if the object has no entries.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Iterates over the keys in document order.
    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.entries.iter().map(|(k, _)| k.as_str())
    }
}

impl FromIterator<(String, Value)> for Object {
    fn from_iter<T: IntoIterator<Item = (String, Value)>>(iter: T) -> Self {
        Object {
            entries: iter.into_iter().collect(),
        }
    }
}

impl Value {
    /// Wraps a value with a flag, or returns it unwrapped if `flag` is `Flag::None`.
    pub fn with_flag(flag: Flag, value: Value) -> Value {
        match flag {
            Flag::None => value,
            _ => Value::Flagged(flag, Box::new(value)),
        }
    }

    /// The flag on this value, or `Flag::None` if unflagged.
    pub fn flag(&self) -> Flag {
        match self {
            Value::Flagged(flag, _) => *flag,
            _ => Flag::None,
        }
    }

    /// The value with any `Flagged` wrapper removed.
    pub fn unflagged(&self) -> &Value {
        match self {
            Value::Flagged(_, inner) => inner.unflagged(),
            other => other,
        }
    }

    pub fn is_null(&self) -> bool {
        matches!(self.unflagged(), Value::Null)
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self.unflagged() {
            Value::Bool(b) => Some(*b),
            _ => None,
        }
    }

    /// An `Int`, or a `UInt` that fits in an `i64`.
    pub fn as_i64(&self) -> Option<i64> {
        match self.unflagged() {
            Value::Int(i) => Some(*i),
            Value::UInt(u) => i64::try_from(*u).ok(),
            _ => None,
        }
    }

    /// A `UInt`, or a non-negative `Int`.
    pub fn as_u64(&self) -> Option<u64> {
        match self.unflagged() {
            Value::UInt(u) => Some(*u),
            Value::Int(i) => u64::try_from(*i).ok(),
            _ => None,
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self.unflagged() {
            Value::Double(d) => Some(*d),
            Value::Int(i) => Some(*i as f64),
            Value::UInt(u) => Some(*u as f64),
            _ => None,
        }
    }

    pub fn as_f32(&self) -> Option<f32> {
        self.as_f64().map(|d| d as f32)
    }

    pub fn as_str(&self) -> Option<&str> {
        match self.unflagged() {
            Value::String(s) => Some(s.as_str()),
            _ => None,
        }
    }

    pub fn as_blob(&self) -> Option<&[u8]> {
        match self.unflagged() {
            Value::Blob(b) => Some(b.as_slice()),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&[Value]> {
        match self.unflagged() {
            Value::Array(a) => Some(a.as_slice()),
            _ => None,
        }
    }

    pub fn as_object(&self) -> Option<&Object> {
        match self.unflagged() {
            Value::Object(o) => Some(o),
            _ => None,
        }
    }

    /// Looks up `key` in this value if it is an object, `None` otherwise.
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.as_object().and_then(|o| o.get(key))
    }
}
