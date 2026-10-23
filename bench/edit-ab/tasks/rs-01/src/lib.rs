//! A fixed-capacity ring buffer that overwrites its oldest item when full.

#[derive(Debug, Clone)]
pub struct Ring<T> {
    buf: Vec<Option<T>>,
    head: usize,
    len: usize,
}

impl<T> Ring<T> {
    /// Makes an empty ring. Panics when the capacity is zero.
    pub fn new(capacity: usize) -> Self {
        assert!(capacity > 0, "capacity must be at least 1");
        let mut buf = Vec::with_capacity(capacity);
        for _ in 0..capacity {
            buf.push(None);
        }
        Ring { buf, head: 0, len: 0 }
    }

    pub fn capacity(&self) -> usize {
        self.buf.len()
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn is_full(&self) -> bool {
        self.len == self.buf.len()
    }

    /// Adds an item at the back. When the ring is full the oldest item is
    /// dropped to make room and returned.
    pub fn push(&mut self, value: T) -> Option<T> {
        let cap = self.buf.len();
        if self.len == cap {
            let old = self.buf[self.head].replace(value);
            self.head = (self.head + 1) % cap;
            old
        } else {
            let tail = (self.head + self.len) % cap;
            self.buf[tail] = Some(value);
            self.len += 1;
            None
        }
    }

    /// Removes and returns the oldest item.
    pub fn pop_front(&mut self) -> Option<T> {
        if self.len == 0 {
            return None;
        }
        let item = self.buf[self.head].take();
        self.head += 1;
        self.len -= 1;
        item
    }

    /// The item at an offset from the oldest one, with 0 being the oldest.
    pub fn get(&self, index: usize) -> Option<&T> {
        if index > self.len {
            return None;
        }
        let cap = self.buf.len();
        self.buf[(self.head + index) % cap].as_ref()
    }

    pub fn front(&self) -> Option<&T> {
        self.get(0)
    }

    pub fn back(&self) -> Option<&T> {
        if self.len == 0 {
            None
        } else {
            self.get(self.len - 1)
        }
    }

    pub fn clear(&mut self) {
        for slot in self.buf.iter_mut() {
            *slot = None;
        }
        self.head = 0;
        self.len = 0;
    }

    pub fn iter(&self) -> impl Iterator<Item = &T> + '_ {
        (0..self.len).filter_map(move |i| self.get(i))
    }
}

impl<T: Clone> Ring<T> {
    pub fn to_vec(&self) -> Vec<T> {
        self.iter().cloned().collect()
    }
}

impl<T> Extend<T> for Ring<T> {
    fn extend<I: IntoIterator<Item = T>>(&mut self, iter: I) {
        for item in iter {
            self.push(item);
        }
    }
}
