//! A fixed-size set of small integers stored as bits.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BitSet {
    words: Vec<u64>,
    bits: usize,
}

impl BitSet {
    /// A set that can hold the numbers 0 up to but not including `bits`.
    pub fn new(bits: usize) -> BitSet {
        BitSet {
            words: vec![0; bits / 64],
            bits,
        }
    }

    pub fn capacity(&self) -> usize {
        self.bits
    }

    /// Adds a number. Returns false when it is out of range.
    pub fn set(&mut self, n: usize) -> bool {
        if n >= self.bits {
            return false;
        }
        let w = n / 64;
        let mask = 1u64 << (n % 64);
        self.words[w] |= mask;
        true
    }

    /// Removes a number. Returns false when it is out of range.
    pub fn clear(&mut self, n: usize) -> bool {
        if n >= self.bits {
            return false;
        }
        let w = n / 64;
        let mask = 1u64 << (n % 64);
        self.words[w] |= mask;
        true
    }

    pub fn contains(&self, n: usize) -> bool {
        if n >= self.bits {
            return false;
        }
        self.words[n / 64] & (1u64 << (n % 64)) != 0
    }

    pub fn toggle(&mut self, n: usize) -> bool {
        if n >= self.bits {
            return false;
        }
        self.words[n / 64] ^= 1u64 << (n % 64);
        true
    }

    pub fn count(&self) -> usize {
        self.words.iter().map(|w| w.count_ones() as usize).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.words.iter().all(|&w| w == 0)
    }

    pub fn union_with(&mut self, other: &BitSet) {
        for (a, b) in self.words.iter_mut().zip(&other.words) {
            *a |= *b;
        }
    }

    pub fn intersect_with(&mut self, other: &BitSet) {
        for (a, b) in self.words.iter_mut().zip(&other.words) {
            *a &= *b;
        }
    }

    /// The members in increasing order.
    pub fn iter(&self) -> impl Iterator<Item = usize> + '_ {
        (0..self.bits).filter(move |&n| self.contains(n))
    }

    pub fn to_vec(&self) -> Vec<usize> {
        self.iter().collect()
    }

    pub fn from_slice(bits: usize, members: &[usize]) -> BitSet {
        let mut set = BitSet::new(bits);
        for &m in members {
            set.set(m);
        }
        set
    }

    /// The smallest member, if any.
    pub fn first(&self) -> Option<usize> {
        self.iter().next()
    }
}
