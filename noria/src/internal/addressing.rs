use std::fmt;

use serde::{Deserialize, Serialize};

#[derive(Eq, PartialEq, Ord, PartialOrd, Hash, Clone, Copy, Debug, Serialize, Deserialize)]
pub struct DomainIndex(usize);

impl From<usize> for DomainIndex {
    fn from(i: usize) -> Self {
        DomainIndex(i)
    }
}

impl Into<usize> for DomainIndex {
    fn into(self) -> usize {
        self.0
    }
}

impl DomainIndex {
    pub fn index(self) -> usize {
        self.0
    }
}

/// A domain-local node identifier.
#[derive(Eq, PartialEq, Ord, PartialOrd, Hash, Clone, Copy, Debug, Serialize, Deserialize)]
pub struct LocalNodeIndex {
    id: u32, // not a tuple struct so this field can be made private
}

impl LocalNodeIndex {
    pub unsafe fn make(id: u32) -> LocalNodeIndex {
        LocalNodeIndex { id }
    }

    pub fn id(self) -> usize {
        self.id as usize
    }
}

impl fmt::Display for LocalNodeIndex {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "l{}", self.id)
    }
}
