use std::collections::{BTreeMap, HashSet};

/// A deterministic consistent-hash ring with virtual replicas.
pub struct ConsistentHash {
    replicas: usize,
    nodes: HashSet<String>,
    ring: BTreeMap<u64, Vec<String>>,
}

impl ConsistentHash {
    pub fn new(replicas: usize) -> Self {
        assert!(replicas > 0, "replica count must be greater than zero");
        Self {
            replicas,
            nodes: HashSet::new(),
            ring: BTreeMap::new(),
        }
    }

    pub fn add(&mut self, node: impl Into<String>) -> bool {
        let node = node.into();
        if !self.nodes.insert(node.clone()) {
            return false;
        }

        for replica in 0..self.replicas {
            self.ring
                .entry(hash_bytes(format!("{node}#{replica}").as_bytes()))
                .or_default()
                .push(node.clone());
        }
        true
    }

    pub fn remove(&mut self, node: &str) -> bool {
        if !self.nodes.remove(node) {
            return false;
        }

        for replica in 0..self.replicas {
            let hash = hash_bytes(format!("{node}#{replica}").as_bytes());
            let remove_entry = if let Some(nodes) = self.ring.get_mut(&hash) {
                nodes.retain(|candidate| candidate != node);
                nodes.is_empty()
            } else {
                false
            };
            if remove_entry {
                self.ring.remove(&hash);
            }
        }
        true
    }

    pub fn get(&self, key: impl AsRef<[u8]>) -> Option<&str> {
        let hash = hash_bytes(key.as_ref());
        self.ring
            .range(hash..)
            .next()
            .or_else(|| self.ring.first_key_value())
            .and_then(|(_, nodes)| nodes.first())
            .map(String::as_str)
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }
}

fn hash_bytes(bytes: &[u8]) -> u64 {
    const OFFSET_BASIS: u64 = 0xcbf29ce484222325;
    const PRIME: u64 = 0x100000001b3;

    bytes.iter().fold(OFFSET_BASIS, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(PRIME)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_keys_to_added_nodes() {
        let mut ring = ConsistentHash::new(100);
        ring.add("a");
        ring.add("b");

        assert!(matches!(ring.get("customer-42"), Some("a" | "b")));
    }

    #[test]
    fn removed_nodes_are_never_returned() {
        let mut ring = ConsistentHash::new(100);
        ring.add("a");
        ring.add("b");
        assert!(ring.remove("a"));

        for customer in 0..100 {
            assert_eq!(ring.get(format!("customer-{customer}")), Some("b"));
        }
    }
}
