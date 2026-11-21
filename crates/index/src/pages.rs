//! A vector whose clones share their pages until one is written: a
//! snapshot of the folder table costs a page's worth of pointers, and a
//! change copies only the pages it writes.

use std::sync::Arc;

const PAGE: usize = 1024;

#[derive(Clone, Debug)]
pub struct Pages<T: Clone> {
    slots: Vec<Arc<Vec<T>>>,
    len: usize,
    fill: T,
}

impl<T: Clone> Pages<T> {
    /// An empty vector, whose new places hold `fill`.
    pub fn new(fill: T) -> Self {
        Pages {
            slots: Vec::new(),
            len: 0,
            fill,
        }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn get(&self, i: usize) -> Option<&T> {
        (i < self.len).then(|| &self.slots[i / PAGE][i % PAGE])
    }

    /// Set place `i`, the vector grown to hold it.
    pub fn set(&mut self, i: usize, value: T) {
        if i >= self.len {
            self.grow(i + 1);
        }
        Arc::make_mut(&mut self.slots[i / PAGE])[i % PAGE] = value;
    }

    fn grow(&mut self, len: usize) {
        while self.slots.len() * PAGE < len {
            self.slots.push(Arc::new(vec![self.fill.clone(); PAGE]));
        }
        self.len = len;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_clone_keeps_what_it_held_when_the_other_changes() {
        let mut a = Pages::new(0u32);
        for i in 0..3000 {
            a.set(i, i as u32);
        }
        let b = a.clone();
        a.set(5, 99);
        a.set(2999, 7);
        assert_eq!(b.get(5), Some(&5));
        assert_eq!(b.get(2999), Some(&2999));
        assert_eq!(a.get(5), Some(&99));
        assert_eq!(a.get(3000), None);
        assert!(
            Arc::ptr_eq(&a.slots[1], &b.slots[1]),
            "a page not written is shared"
        );
    }
}
