use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SingleOrArray<T> {
    Single(T),
    Array(Vec<T>),
}

impl<T> SingleOrArray<T> {
    pub fn items(&self) -> Vec<&T> {
        match self {
            SingleOrArray::Single(v) => vec![v],
            SingleOrArray::Array(v) => v.iter().collect(),
        }
    }

    #[allow(dead_code)] // reserved: consuming converters in deserialization helpers
    pub fn into_vec(self) -> Vec<T> {
        match self {
            SingleOrArray::Single(v) => vec![v],
            SingleOrArray::Array(v) => v,
        }
    }
}
