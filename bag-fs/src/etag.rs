use std::{
    hash::{Hash, Hasher},
    time::SystemTime,
};

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct Etag<'s> {
    pub mtime: SystemTime,
    pub length: u64,
    pub subpath: Option<&'s str>,
}

impl Etag<'_> {
    pub fn hash_u64(&self) -> u64 {
        let mut hasher = std::hash::DefaultHasher::new();
        self.hash(&mut hasher);
        hasher.finish()
    }

    pub fn hash_string(&self) -> String {
        format!("{:x}", self.hash_u64())
    }
}

pub fn check_header(computed: &str, header: &str) -> bool {
    if header == "*" {
        return true;
    }
    let with_quotes = format!("\"{}\"", computed);

    header.split(",").any(|seg| {
        let trimmed = seg.trim();
        return trimmed == with_quotes;
    })
}
