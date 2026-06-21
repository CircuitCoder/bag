use std::{hash::{Hash, Hasher}, time::SystemTime};

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct Etag {
    pub mtime: SystemTime,
    pub length: u64,
}

impl Etag {
    pub fn hash_u64(&self) -> u64 {
        let mut hasher = std::hash::DefaultHasher::new();
        self.hash(&mut hasher);
        hasher.finish()
    }

    pub fn hash_string(&self) -> String {
        format!("{:x}", self.hash_u64())
    }
}
