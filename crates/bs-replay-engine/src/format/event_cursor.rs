// SPDX-License-Identifier: MIT
//! `EventCursor` — sequential walk of recorded events.
//!
//! The replay-driver use case: given a target event index, decode
//! events one at a time starting from there. The cursor caches the
//! currently-loaded segment (one decompression cost per segment
//! crossing) and yields owned [`Event`] values via [`Self::next`].
//!
//! Why owned, not borrowed? rkyv archived views borrow from the
//! segment's decompressed buffer, which the cursor itself owns.
//! Borrowed iteration would require GATs (`Iterator` trait
//! signature is `Item = &'static T` in spirit) or self-referential
//! lifetimes neither rkyv 0.8 nor stable Rust accommodate cleanly.
//! The deserialize-one cost is small (few hundred ns) and dwarfed
//! by the work the replay engine has to do per event anyway.

use super::event::Event;
use super::trace_reader::{SegmentReader, TraceReadError, TraceReader};

/// Walks events in record order from a starting event index.
///
/// Created via [`TraceReader::cursor`] or
/// [`TraceReader::cursor_at`]. Lazily decompresses each segment as
/// the cursor crosses it.
#[derive(Debug)]
pub struct EventCursor<'a> {
    reader: &'a TraceReader,
    /// Next event index in the global event-index space.
    next_event_index: u64,
    /// Currently-loaded segment, if any. `(segment_index, reader,
    /// next_offset_within_segment)`.
    current: Option<(u64, SegmentReader, usize)>,
}

impl<'a> EventCursor<'a> {
    pub(crate) fn new(reader: &'a TraceReader, start: u64) -> Self {
        Self {
            reader,
            next_event_index: start,
            current: None,
        }
    }

    /// The event index that [`Self::next`] would return next.
    pub fn position(&self) -> u64 {
        self.next_event_index
    }

    /// Move the cursor to `event_index` without yielding events.
    /// The next call to [`Self::next`] returns the event at that
    /// index. Cheap — does not decompress until [`Self::next`] is
    /// called.
    pub fn seek_to(&mut self, event_index: u64) {
        // If the target lies inside the currently-loaded segment
        // we can keep the decompressed buffer; otherwise drop it
        // so `next()` reloads.
        if let Some((seg_idx, _, _)) = &self.current {
            let ranges = match self.reader.segment_event_ranges() {
                Ok(r) => r,
                Err(_) => {
                    self.current = None;
                    self.next_event_index = event_index;
                    return;
                }
            };
            if let Some(r) = ranges.iter().find(|r| r.segment_index == *seg_idx)
                && r.contains(event_index)
            {
                let new_offset = (event_index - r.first_event_index) as usize;
                if let Some((_, _, off)) = &mut self.current {
                    *off = new_offset;
                }
                self.next_event_index = event_index;
                return;
            }
        }
        self.current = None;
        self.next_event_index = event_index;
    }

    /// Decode the next event in record order, or `Ok(None)` if the
    /// cursor has run past the end of the trace.
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> Result<Option<Event>, TraceReadError> {
        // Fast path: still inside the loaded segment.
        if let Some((_, seg, off)) = &mut self.current
            && *off < seg.event_count()?
        {
            let ev = seg.event_at(*off)?;
            *off += 1;
            self.next_event_index += 1;
            return Ok(Some(ev));
        }
        // Need to load the segment that holds `next_event_index`.
        let routed = self.reader.segment_for_event(self.next_event_index)?;
        let Some((seg_idx, offset_in_seg)) = routed else {
            // Past end of trace.
            return Ok(None);
        };
        let seg = self.reader.open_segment(seg_idx)?;
        let ev = seg.event_at(offset_in_seg as usize)?;
        // Stash the segment reader and advance.
        let next_off = offset_in_seg as usize + 1;
        self.current = Some((seg_idx, seg, next_off));
        self.next_event_index += 1;
        Ok(Some(ev))
    }
}

impl TraceReader {
    /// Cursor positioned at event 0 (the start of the trace).
    pub fn cursor(&self) -> EventCursor<'_> {
        EventCursor::new(self, 0)
    }

    /// Cursor positioned at `event_index`. Out-of-range indices
    /// don't fail here — the failure surfaces from [`EventCursor::next`]
    /// returning `Ok(None)`.
    pub fn cursor_at(&self, event_index: u64) -> EventCursor<'_> {
        EventCursor::new(self, event_index)
    }
}
