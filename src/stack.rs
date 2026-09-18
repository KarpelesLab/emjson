/// Nesting stack storing one bit per level (1 = object, 0 = array).
#[derive(Debug, Clone)]
pub(crate) struct BitStack<const N: usize> {
    bits: [u8; N],
    depth: u16,
}

impl<const N: usize> BitStack<N> {
    pub(crate) const MAX: usize = if N * 8 < u16::MAX as usize { N * 8 } else { u16::MAX as usize };

    pub(crate) const fn new() -> Self {
        Self { bits: [0; N], depth: 0 }
    }

    #[inline]
    pub(crate) fn depth(&self) -> usize {
        self.depth as usize
    }

    /// Pushes a level; returns false when the stack is full.
    #[inline]
    pub(crate) fn push(&mut self, object: bool) -> bool {
        let d = self.depth as usize;
        if d >= Self::MAX {
            return false;
        }
        let mask = 1 << (d % 8);
        if object {
            self.bits[d / 8] |= mask;
        } else {
            self.bits[d / 8] &= !mask;
        }
        self.depth += 1;
        true
    }

    #[inline]
    pub(crate) fn pop(&mut self) {
        debug_assert!(self.depth > 0);
        self.depth = self.depth.saturating_sub(1);
    }

    /// `Some(true)` inside an object, `Some(false)` inside an array, `None` at top level.
    #[inline]
    pub(crate) fn top(&self) -> Option<bool> {
        let d = (self.depth as usize).checked_sub(1)?;
        Some(self.bits[d / 8] & (1 << (d % 8)) != 0)
    }
}
