use std::{
    fmt,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Mutex,
    },
};

/// A thread-safe in-memory Bloom filter.
///
/// Bloom filters never produce false negatives. A positive result means that an item may exist,
/// while a negative result guarantees that it has not been inserted.
#[derive(Debug)]
pub struct BloomFilter {
    bits: Mutex<Vec<u64>>,
    bit_count: usize,
    hash_functions: u32,
    insertions: AtomicUsize,
}

impl BloomFilter {
    /// Creates a filter with an explicit number of bits and hash functions.
    pub fn new(bit_count: usize, hash_functions: u32) -> Result<Self, BloomError> {
        if bit_count == 0 {
            return Err(BloomError::ZeroBits);
        }
        if hash_functions == 0 {
            return Err(BloomError::ZeroHashFunctions);
        }

        Ok(Self {
            bits: Mutex::new(vec![0; bit_count.div_ceil(64)]),
            bit_count,
            hash_functions,
            insertions: AtomicUsize::new(0),
        })
    }

    /// Sizes a filter for the expected number of items and desired false-positive rate.
    pub fn with_rate(expected_items: usize, false_positive_rate: f64) -> Result<Self, BloomError> {
        if expected_items == 0 {
            return Err(BloomError::ZeroExpectedItems);
        }
        if !(0.0..1.0).contains(&false_positive_rate) {
            return Err(BloomError::InvalidFalsePositiveRate(false_positive_rate));
        }

        let item_count = expected_items as f64;
        let logarithm = std::f64::consts::LN_2;
        let bit_count =
            (-(item_count * false_positive_rate.ln()) / logarithm.powi(2)).ceil() as usize;
        let hash_functions = ((bit_count as f64 / item_count) * logarithm)
            .round()
            .max(1.0) as u32;
        Self::new(bit_count, hash_functions)
    }

    /// Inserts an item and reports whether at least one bit changed.
    pub fn insert(&self, value: impl AsRef<[u8]>) -> bool {
        let (first, second) = hashes(value.as_ref());
        let mut bits = self.bits.lock().expect("Bloom filter mutex poisoned");
        let mut changed = false;

        for index in self.indices(first, second) {
            let word = index / 64;
            let mask = 1_u64 << (index % 64);
            changed |= bits[word] & mask == 0;
            bits[word] |= mask;
        }

        if changed {
            self.insertions.fetch_add(1, Ordering::Relaxed);
        }
        changed
    }

    /// Returns `false` only when the item definitely has not been inserted.
    pub fn contains(&self, value: impl AsRef<[u8]>) -> bool {
        let (first, second) = hashes(value.as_ref());
        let bits = self.bits.lock().expect("Bloom filter mutex poisoned");
        self.indices(first, second).all(|index| {
            let word = index / 64;
            let mask = 1_u64 << (index % 64);
            bits[word] & mask != 0
        })
    }

    pub fn bit_count(&self) -> usize {
        self.bit_count
    }

    pub fn hash_functions(&self) -> u32 {
        self.hash_functions
    }

    /// Returns the number of insert calls that changed the filter.
    pub fn insertions(&self) -> usize {
        self.insertions.load(Ordering::Relaxed)
    }

    pub fn clear(&self) {
        self.bits
            .lock()
            .expect("Bloom filter mutex poisoned")
            .fill(0);
        self.insertions.store(0, Ordering::Relaxed);
    }

    fn indices(&self, first: u64, second: u64) -> impl Iterator<Item = usize> + '_ {
        (0..self.hash_functions).map(move |iteration| {
            first
                .wrapping_add(u64::from(iteration).wrapping_mul(second))
                .wrapping_rem(self.bit_count as u64) as usize
        })
    }
}

fn hashes(bytes: &[u8]) -> (u64, u64) {
    const OFFSET_BASIS: u64 = 0xcbf29ce484222325;
    const PRIME: u64 = 0x100000001b3;

    let first = bytes.iter().fold(OFFSET_BASIS, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(PRIME)
    });
    let second = bytes
        .iter()
        .rev()
        .fold(OFFSET_BASIS ^ 0x9e3779b97f4a7c15, |hash, byte| {
            (hash ^ u64::from(*byte)).wrapping_mul(PRIME)
        })
        | 1;
    (first, second)
}

#[derive(Debug, Clone, PartialEq)]
pub enum BloomError {
    ZeroBits,
    ZeroHashFunctions,
    ZeroExpectedItems,
    InvalidFalsePositiveRate(f64),
}

impl fmt::Display for BloomError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroBits => {
                formatter.write_str("Bloom filter bit count must be greater than zero")
            }
            Self::ZeroHashFunctions => {
                formatter.write_str("Bloom filter hash count must be greater than zero")
            }
            Self::ZeroExpectedItems => {
                formatter.write_str("Bloom filter expected item count must be greater than zero")
            }
            Self::InvalidFalsePositiveRate(rate) => write!(
                formatter,
                "Bloom filter false-positive rate must be between zero and one: {rate}"
            ),
        }
    }
}

impl std::error::Error for BloomError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inserted_items_are_never_missing() {
        let filter = BloomFilter::with_rate(1_000, 0.001).unwrap();
        for item in 0..1_000 {
            filter.insert(item.to_string());
        }

        for item in 0..1_000 {
            assert!(filter.contains(item.to_string()));
        }
        assert_eq!(filter.insertions(), 1_000);
    }

    #[test]
    fn reports_absent_items_and_can_be_cleared() {
        let filter = BloomFilter::new(1_024, 4).unwrap();
        filter.insert("known");

        assert!(filter.contains("known"));
        assert!(!filter.contains("definitely-absent"));
        filter.clear();
        assert!(!filter.contains("known"));
        assert_eq!(filter.insertions(), 0);
    }

    #[test]
    fn validates_filter_dimensions() {
        assert_eq!(BloomFilter::new(0, 1).unwrap_err(), BloomError::ZeroBits);
        assert_eq!(
            BloomFilter::new(1, 0).unwrap_err(),
            BloomError::ZeroHashFunctions
        );
        assert!(matches!(
            BloomFilter::with_rate(100, 1.0),
            Err(BloomError::InvalidFalsePositiveRate(1.0))
        ));
    }
}
