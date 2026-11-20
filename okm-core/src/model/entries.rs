//! One write's access-method entries, appended into a single allocation.
//!
//! The shape a write wants is not "a list of owned byte strings": every entry is consumed by a
//! store that copies it, so a `Vec<(Vec<u8>, Vec<u8>)>` spends two allocations per entry to
//! hand over bytes nobody keeps. [`Entries`] instead keeps one buffer per collection, appends
//! every key and value into it, and records where each starts — so a collection that has
//! written once writes again without allocating at all.
//!
//! Layout: entries are packed back to back, `[key][value][key][value]…`, with the starts in a
//! side list. A length is therefore the distance to the next start, which is why the list needs
//! only one number per entry per half and why the last one measures against the end of the
//! buffer. Nothing in the buffer is framed: the spans are the frame, and they are computed as
//! the entry is appended.

/// Entries of one write: their bytes in one buffer, their boundaries beside it.
#[derive(Default, Debug, Clone)]
pub struct Entries {
    bytes: Vec<u8>,
    /// `(key start, value start)` per entry, in write order.
    starts: Vec<(u32, u32)>,
}

impl Entries {
    pub fn new() -> Self {
        Self::default()
    }

    /// Empty the entries, KEEPING the buffer: the next write of the same collection reuses it.
    pub fn clear(&mut self) {
        self.bytes.clear();
        self.starts.clear();
    }

    /// Append one entry: `write_key` fills the key, `write_value` the value. Both write
    /// straight into the shared buffer — no per-entry allocation, and no intermediate.
    pub fn push(
        &mut self,
        write_key: impl FnOnce(&mut Vec<u8>),
        write_value: impl FnOnce(&mut Vec<u8>),
    ) {
        let key_start = self.bytes.len() as u32;
        write_key(&mut self.bytes);
        let value_start = self.bytes.len() as u32;
        write_value(&mut self.bytes);
        self.starts.push((key_start, value_start));
    }

    /// Copy a pair that already exists as bytes (the fan-out path, a repair, a test).
    pub fn extend_pair(&mut self, key: &[u8], value: &[u8]) {
        let key_start = self.bytes.len() as u32;
        self.bytes.extend_from_slice(key);
        let value_start = self.bytes.len() as u32;
        self.bytes.extend_from_slice(value);
        self.starts.push((key_start, value_start));
    }

    /// How many entries were appended.
    pub fn len(&self) -> usize {
        self.starts.len()
    }

    pub fn is_empty(&self) -> bool {
        self.starts.is_empty()
    }

    /// The whole buffer — what a frame writer copies, and what spans index into.
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// `(key, value)` spans, in write order. Lengths come from the next start, so the buffer
    /// carries no per-entry framing.
    pub fn iter(&self) -> Iter<'_> {
        Iter {
            bytes: &self.bytes,
            starts: &self.starts,
            next: 0,
        }
    }

    /// The entries as owned pairs. For callers that need to keep them past the write (tests, a
    /// diff against the previous entry set); the write path hands out spans instead.
    pub fn collect(&self) -> Vec<(Vec<u8>, Vec<u8>)> {
        self.iter().map(|(k, v)| (k.to_vec(), v.to_vec())).collect()
    }
}

pub struct Iter<'a> {
    bytes: &'a [u8],
    starts: &'a [(u32, u32)],
    next: usize,
}

impl<'a> Iterator for Iter<'a> {
    type Item = (&'a [u8], &'a [u8]);

    fn next(&mut self) -> Option<Self::Item> {
        let (key_start, value_start) = *self.starts.get(self.next)?;
        let key_end = value_start as usize;
        let value_end = match self.starts.get(self.next + 1) {
            Some((next_key_start, _)) => *next_key_start as usize,
            None => self.bytes.len(),
        };
        self.next += 1;
        Some((
            &self.bytes[key_start as usize..key_end],
            &self.bytes[key_end..value_end],
        ))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let left = self.starts.len() - self.next;
        (left, Some(left))
    }
}

impl ExactSizeIterator for Iter<'_> {}
