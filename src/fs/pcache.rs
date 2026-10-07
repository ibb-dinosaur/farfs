#[cfg(feature = "path-cache")]
mod impl_ {

pub(crate) struct PathCache {
    cache: quick_cache::sync::Cache<Dentry, u64>
}

const PATH_CACHE_CAPACITY: usize = 128;

impl PathCache {
    pub fn new() -> Self {
        PathCache { cache: quick_cache::sync::Cache::new(PATH_CACHE_CAPACITY) }
    }

    pub fn lookup(&self, parent_uid: u64, name: &[u8]) -> Option<u64> {
        self.cache.get(&(parent_uid, name))
    }

    pub fn store(&self, parent_uid: u64, name: &[u8], file_uid: u64) {
        self.cache.insert(Dentry { parent_uid, name: name.into(), }, file_uid);
    }

    pub fn drop(&self, parent_uid: u64, name: &[u8]) {
        self.cache.remove(&(parent_uid, name));
    }
}

#[derive(Hash, PartialEq, Eq)]
struct Dentry {
    parent_uid: u64,
    name: Box<[u8]>,
}

impl quick_cache::Equivalent<Dentry> for (u64, &[u8]) {
    fn equivalent(&self, key: &Dentry) -> bool {
        self.0 == key.parent_uid && self.1 == &*key.name
    }
}

}

#[cfg(not(feature = "path-cache"))]
mod impl_ {
    pub(crate) struct PathCache;
    impl PathCache {
        pub fn new() -> Self { PathCache }
        pub fn lookup(&self, _parent_uid: u64, _name: &[u8]) -> Option<u64> { None }
        pub fn store(&self, _parent_uid: u64, _name: &[u8], _file_uid: u64) {}
        pub fn drop(&self, _parent_uid: u64, _name: &[u8]) {}
    }
}

pub(crate) use impl_::*;