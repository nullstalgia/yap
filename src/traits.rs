//! Module for the more generic helper traits I've needed while working on this project

use std::{borrow::Cow, collections::BTreeMap, ops::Range};

use crate::buffer::{HEX_UPPER, LineEnding};
use itertools::Itertools;
use ratatui::{
    style::{Style, Stylize},
    text::{Line, Span},
};

/// Trait that provides simple methods to get the last valid index of a collection or slice.
pub trait LastIndex {
    /// Returns the index of the last element in the collection.
    ///
    /// Returns `None` if the collection is empty.
    ///
    /// Under most cirumstances, this should be the only trait function you need to define.
    fn last_index_checked(&self) -> Option<usize>;
    /// Returns `true` if the given index matches the index of the last element in the collection.
    ///
    /// Returns `false` if the index doesn't match, or if the collection is empty.
    fn last_index_eq(&self, index: usize) -> bool {
        if let Some(last_index) = self.last_index_checked() {
            last_index == index
        } else {
            false
        }
    }
    /// Returns `true` if the given index matches or is greater than the index of the last element in the collection.
    ///
    /// Returns `false` if the index doesn't fit either condition, or if the collection is empty.
    fn last_index_eq_or_under(&self, index: usize) -> bool {
        if let Some(last_index) = self.last_index_checked() {
            index >= last_index
        } else {
            false
        }
    }
    /// Returns the index of the last element in the collection.
    ///
    /// **Panics** if the collection is empty.
    fn last_index(&self) -> usize {
        self.last_index_checked()
            .expect("empty collection; no final element exists")
    }
}
impl<T> LastIndex for [T] {
    fn last_index_checked(&self) -> Option<usize> {
        if self.is_empty() {
            None
        } else {
            Some(self.len() - 1)
        }
    }
}

/// Trait that provides a method to check if a `[u8]` ends with the contents of another supplied `[u8]`
pub trait ByteSuffixCheck {
    /// Returns `true` if the collection ends with the supplied byte slice.
    ///
    /// Returns `false` if there's any mismatch, or if the checked collection is shorter than the suffix.
    fn has_byte_suffix(&self, expected: &[u8]) -> bool;
    /// Returns `true` if the collection ends with the supplied line ending.
    ///
    /// Returns `false` if there's any mismatch, if the checked collection is shorter than the suffix, or if the line ending is None.
    fn has_line_ending(&self, line_ending: &LineEnding) -> bool;
}
impl ByteSuffixCheck for [u8] {
    fn has_byte_suffix(&self, expected: &[u8]) -> bool {
        if self.len() < expected.len() {
            false
        } else {
            let start = self.len() - expected.len();

            &self[start..] == expected
        }
    }
    fn has_line_ending(&self, line_ending: &LineEnding) -> bool {
        match line_ending {
            LineEnding::None => false,
            LineEnding::Byte(byte) => self.has_byte_suffix(&[*byte]),
            LineEnding::MultiByte(finder) => self.has_byte_suffix(finder.needle()),
        }
    }
}

/// Trait that provides a single method to get the first `N` "Unicode Scalar Values" from a string slice.
pub trait FirstChars {
    /// Return the first `N` "Unicode Scalar Values" from a string slice.
    fn first_chars(&self, char_count: usize) -> Option<&str>;
}
impl FirstChars for str {
    fn first_chars(&self, char_count: usize) -> Option<&str> {
        let actual_count = self.chars().count();
        // Not enough characters to make a slice
        if actual_count < char_count {
            None
        } else if actual_count == char_count {
            Some(self)
        } else {
            let end = self
                .char_indices()
                .nth(char_count)
                .map(|(i, _)| i)
                .expect("Not enough chars?");
            Some(&self[..end])
        }
    }
}

/// Trait that provides a single method to get the first `N` "Unicode Scalar Values" from a string slice.
#[cfg(feature = "macros")]
pub trait StrAsOption {
    /// If the string slice refers to an empty slice, return `None`, otherwise return the contained string.
    fn as_option(&self) -> Option<&str>;
}
#[cfg(feature = "macros")]
impl StrAsOption for str {
    fn as_option(&self) -> Option<&str> {
        if self.is_empty() { None } else { Some(self) }
    }
}

#[allow(dead_code)] // Not using the bottom two methods, but I don't wish to comment them out/remove them yet.
pub trait LineHelpers<'a> {
    /// Removes all tabs, carriage returns, newlines, and control characters from all spans in the line.
    fn remove_unsavory_chars(&mut self, replace: bool);
    /// Returns `true` if no `Spans` exist, or if all `Spans` are also empty.
    fn is_empty(&self) -> bool;
    /// Iterates through all Spans and sets the given style to all.
    fn style_all_spans(&mut self, new_style: Style);
    /// Consumes the `Line` and returns a new one with all `Span`'s styles set to the specified style.
    fn all_spans_styled(self, new_style: Style) -> Line<'a>;
    /// Returns a new `Line` object that owns all of it's spans, copied from the original.
    fn new_owned(&'a self) -> Line<'static>;
    /// Generates an iterator that creates owned `Span` objects whose content borrows from the original line's spans.
    fn borrowed_spans_iter(&'a self) -> impl DoubleEndedIterator<Item = Span<'a>>;

    /// Returns `true` if either the `Line` or any of it's `Spans` are styled.
    fn is_styled(&self) -> bool;
    /// Returns a new `Line` that borrows from all of the current line's spans.
    fn new_borrowing(&'a self) -> Line<'a>;

    fn spaced(self, both_sides_padding: usize) -> Line<'a>;
    fn spaced_l(self, left_padding: usize) -> Line<'a>;
    fn spaced_r(self, right_padding: usize) -> Line<'a>;
}

impl<'a> LineHelpers<'a> for Line<'a> {
    fn new_owned(&'a self) -> Line<'static> {
        let mut line: Line<'static> = Line::from_iter(self.borrowed_spans_iter().map(|s| {
            let span: Span<'static> = Span {
                content: s.content.as_ref().to_owned().into(),
                ..s
            };
            span
        }));
        if self.alignment.is_some() {
            line.alignment = self.alignment;
        }
        line.style = self.style;
        line
    }
    fn remove_unsavory_chars(&mut self, replace: bool) {
        // let now = Instant::now();
        let is_char_unsavory = |c: char| -> bool { c.is_ascii_control() || c.is_control() };

        let mut chars_to_escape: BTreeMap<usize, char> = BTreeMap::new();

        let mut line_char_index = 0;
        self.spans
            .iter()
            .flat_map(|s| s.content.chars())
            .for_each(|c| {
                if is_char_unsavory(c) {
                    chars_to_escape.insert(line_char_index, c);
                }
                line_char_index += c.len_utf8();
            });

        // let now2 = Instant::now();
        let dark_gray = Style::new().dark_gray();

        let mut offset: isize = 0;
        for (index, char) in chars_to_escape {
            let corrected = index.checked_add_signed(offset).expect("overflow!");
            let char_len = char.len_utf8();

            // let now = Instant::now();
            self.remove_slice(corrected..corrected + char_len);
            // debug!("took {:?} to scream", now.elapsed());
            let replacement_len = match char {
                // TODO handle tab width properly?
                '\t' => {
                    let tab = "\\t";
                    self.insert_slice(corrected, tab, Some(dark_gray));
                    tab.len()
                }
                _ if !replace => 0,
                '\n' => {
                    let lf = "\\n";
                    self.insert_slice(corrected, lf, Some(dark_gray));
                    lf.len()
                }
                '\r' => {
                    let cr = "\\r";
                    self.insert_slice(corrected, cr, Some(dark_gray));
                    cr.len()
                }
                _ => {
                    let mut buffer = [0u8; 4];
                    let len = char.encode_utf8(&mut buffer).len();
                    let mut added_len = 0;

                    for i in 0..len {
                        let hex_prefix = "\\x";
                        self.insert_slice(corrected + added_len, hex_prefix, Some(dark_gray));
                        added_len += hex_prefix.len();
                        let upper_hex = HEX_UPPER[buffer[i] as usize];
                        self.insert_slice(corrected + added_len, upper_hex, Some(dark_gray));
                        added_len += upper_hex.len();
                    }
                    added_len
                }
            };

            let offset_mod = replacement_len as isize - char_len as isize;
            offset += offset_mod;
        }

        // debug!(
        //     "unsavory took {:?} for modifying {line_char_index}",
        //     now2.elapsed()
        // );
    }
    fn is_empty(&self) -> bool {
        self.spans.is_empty() || self.spans.iter().all(|s| s.content.is_empty())
    }
    fn style_all_spans(&mut self, new_style: Style) {
        for span in self.spans.iter_mut() {
            span.style = new_style;
        }
    }
    fn borrowed_spans_iter(&'a self) -> impl DoubleEndedIterator<Item = Span<'a>> {
        self.spans
            .iter()
            .map(|s| Span::styled(Cow::Borrowed(s.content.as_ref()), s.style))
    }
    fn all_spans_styled(mut self, new_style: Style) -> Line<'a> {
        self.style_all_spans(new_style);
        self
    }

    fn is_styled(&self) -> bool {
        self.style != Style::default() || self.spans.iter().any(|s| s.style != Style::default())
    }
    fn new_borrowing(&self) -> Line<'_> {
        let mut line = Line::from_iter(self.borrowed_spans_iter());
        if self.alignment.is_some() {
            line.alignment = self.alignment;
        }
        line.style = self.style;
        line
    }

    fn spaced(self, both_sides_padding: usize) -> Line<'a> {
        self.spaced_l(both_sides_padding)
            .spaced_r(both_sides_padding)
    }
    fn spaced_l(mut self, left_padding: usize) -> Line<'a> {
        let spaces = std::iter::repeat_n(Span::raw(" "), left_padding);
        self.spans.splice(0..0, spaces);
        self
    }
    fn spaced_r(mut self, right_padding: usize) -> Line<'a> {
        let spaces = std::iter::repeat_n(Span::raw(" "), right_padding);
        self.spans.extend(spaces);
        self
    }
}

pub trait ToggleBool {
    /// Flips the boolean value in-place, returning the new value.
    fn flip(&mut self) -> bool;
}

impl ToggleBool for bool {
    fn flip(&mut self) -> bool {
        *self = !*self;
        *self
    }
}

#[cfg(feature = "macros")]
pub trait HasEscapedBytes {
    fn has_escaped_bytes(&self) -> bool;
}

#[cfg(feature = "macros")]
impl HasEscapedBytes for str {
    /// Returns `true` only if the given &str contains escaped bytes.
    ///
    /// Note: unescapes whole to check, so isn't the cheapest check yet.
    fn has_escaped_bytes(&self) -> bool {
        // Fast path: if not even a single backslash exists, then bail.

        if memchr::memchr(b'\\', self.as_bytes()).is_none() {
            return false;
        }

        // Otherwise, directly compare the results of a full unescape.

        use bstr::ByteVec;
        let unescaped = Vec::unescape_bytes(self);

        unescaped != self.as_bytes()
    }
}

pub trait LineMutator<'a> {
    fn insert_slice(
        &mut self,
        index: usize,
        content: impl Into<Cow<'a, str>>,
        style: Option<Style>,
    );
    fn style_slice(&mut self, range: Range<usize>, style: Style);
    fn censor_slice(&mut self, range: Range<usize>, style: Option<Style>);
    fn remove_slice(&mut self, range: Range<usize>);
}

impl<'a> LineMutator<'a> for Line<'a> {
    /// ## Panics if index intersects char boundaries or goes out of bounds!
    fn insert_slice(
        &mut self,
        index: usize,
        content: impl Into<Cow<'a, str>>,
        style: Option<Style>,
    ) {
        let total_len = self.spans.iter().map(|s| s.content.len()).sum();
        assert!(
            index <= total_len,
            "Insertion operation index out of bounds: the index is {index} but the total length is {total_len}"
        );

        // if total_len == 0 {
        //     return;
        // }

        let mut index_within_span_opt = None;
        let mut last_span_style = None;
        let mut passed_len = 0;
        let Some((span_index, span)) = self.spans.iter().find_position(|s| {
            passed_len += s.content.len();

            // Span is before requested index, ignore.
            if passed_len < index {
                last_span_style = Some(s.style);
                false
            } else if passed_len == index {
                // Span ended at index! Ideal!
                true
            } else {
                // Index is within this span.
                index_within_span_opt = Some(s.content.len() - (passed_len - index));
                true
            }
        }) else {
            unreachable!("requested index: {index}, len: {total_len}");
        };

        match index_within_span_opt {
            None => {
                self.spans.insert(
                    span_index + 1,
                    Span::styled(content, style.unwrap_or(span.style)),
                );
            }
            Some(0) => {
                self.spans.insert(
                    span_index,
                    Span::styled(
                        content,
                        style.unwrap_or(last_span_style.unwrap_or_default()),
                    ),
                );
            }
            Some(index_within_span) => match &span.content {
                Cow::Borrowed(borrowed) => {
                    let (pre, post) = borrowed.split_at(index_within_span);
                    let pre = Span::styled(pre, span.style);
                    let mid = Span::styled(content, style.unwrap_or(span.style));
                    let post = Span::styled(post, span.style);
                    let range = Range {
                        start: span_index,
                        end: (span_index + 2).min(self.spans.len()),
                    };
                    self.spans.splice(range, [pre, mid, post]);
                }
                Cow::Owned(owned) => {
                    let (pre, post) = owned.split_at(index_within_span);
                    let pre = Span::styled(pre.to_owned(), span.style);
                    let mid = Span::styled(content, style.unwrap_or(span.style));
                    let post = Span::styled(post.to_owned(), span.style);
                    let range = Range {
                        start: span_index,
                        end: (span_index + 2).min(self.spans.len()),
                    };
                    self.spans.splice(range, [pre, mid, post]);
                }
            },
        }
    }
    /// ## Panics if range intersects char boundaries or goes out of bounds!
    #[inline]
    fn style_slice(&mut self, range: Range<usize>, style: Style) {
        // #[cfg(debug_assertions)]
        // debug!("Styling {range:?} with {style:?}");
        let mut new_spans = Vec::with_capacity(self.spans.len());
        let old_spans = std::mem::take(&mut self.spans);

        let mut current = 0;
        for span in old_spans {
            let span_len = span.content.len();
            let span_start = current;
            let span_end = current + span_len;

            let (overlap_start, overlap_end) =
                overlap_region((span_start, span_end), (range.start, range.end));

            if let Some((overlap_start, overlap_end)) = overlap_start.zip(overlap_end) {
                if overlap_start < overlap_end {
                    // This span intersects the range. May need to split span.
                    let offset_start = overlap_start - span_start;
                    let offset_end = overlap_end - span_start;

                    let style = span.style.patch(style);

                    if offset_start == 0 && offset_end == span_len {
                        // Entire span is inside range, style whole span.
                        new_spans.push(Span::styled(span.content, style));
                    } else {
                        let orig_style = span.style;
                        match &span.content {
                            // Try to borrow again if already borrowed
                            Cow::Borrowed(borrowed) => {
                                let (pre, mid, post) =
                                    split_span_content(borrowed, offset_start..offset_end);
                                if !pre.is_empty() {
                                    new_spans.push(Span::styled(Cow::Borrowed(pre), orig_style));
                                }
                                if !mid.is_empty() {
                                    new_spans.push(Span::styled(Cow::Borrowed(mid), style));
                                }
                                if !post.is_empty() {
                                    new_spans.push(Span::styled(Cow::Borrowed(post), orig_style));
                                }
                            }
                            // Otherwise, we need to make new owned versions
                            Cow::Owned(owned) => {
                                let (pre, mid, post) =
                                    split_span_content(owned, offset_start..offset_end);
                                if !pre.is_empty() {
                                    new_spans.push(Span::styled(
                                        Cow::Owned(pre.to_string()),
                                        orig_style,
                                    ));
                                }
                                if !mid.is_empty() {
                                    new_spans
                                        .push(Span::styled(Cow::Owned(mid.to_string()), style));
                                }
                                if !post.is_empty() {
                                    new_spans.push(Span::styled(
                                        Cow::Owned(post.to_string()),
                                        orig_style,
                                    ));
                                }
                            }
                        }
                    }
                } else {
                    new_spans.push(span);
                }
            } else {
                new_spans.push(span);
            }
            current += span_len;
        }
        self.spans = new_spans;
    }
    #[inline]
    fn censor_slice(&mut self, range: Range<usize>, style: Option<Style>) {
        // #[cfg(debug_assertions)]
        // debug!("Censoring {range:?} with style {style:?}");
        let mut new_spans = Vec::with_capacity(self.spans.len());
        let spans = std::mem::take(&mut self.spans);

        let mut current = 0;
        for span in spans {
            let span_len = span.content.len();
            let span_start = current;
            let span_end = current + span_len;

            let (overlap_start, overlap_end) =
                overlap_region((span_start, span_end), (range.start, range.end));

            if let Some((overlap_start, overlap_end)) = overlap_start.zip(overlap_end) {
                if overlap_start < overlap_end {
                    // This span is at least partially in the censor range.
                    let offset_start = overlap_start - span_start;
                    let offset_end = overlap_end - span_start;
                    let orig_style = span.style;

                    // Doubled up to give borrow checker more detailed info.
                    match span.content {
                        Cow::Borrowed(borrowed) => {
                            let (pre, mid, post) =
                                split_span_content(borrowed, offset_start..offset_end);

                            if !pre.is_empty() {
                                new_spans.push(Span::styled(Cow::Borrowed(pre), orig_style));
                            }
                            if !mid.is_empty() {
                                // Actually perform the censorship: replace char's _bytes_ with '*'
                                // (bytes instead of chars due to us working on the byte-scale, and we might replace
                                // a multi-byte char, so this keeps char boundaries valid.)
                                let bullet = "*";
                                for _ in 0..mid.len() {
                                    new_spans
                                        .push(Span::styled(bullet, style.unwrap_or(orig_style)));
                                }
                            }
                            if !post.is_empty() {
                                new_spans.push(Span::styled(Cow::Borrowed(post), orig_style));
                            }
                        }
                        Cow::Owned(owned) => {
                            let (pre, mid, post) =
                                split_span_content(&owned, offset_start..offset_end);

                            if !pre.is_empty() {
                                new_spans
                                    .push(Span::styled(Cow::Owned(pre.to_owned()), orig_style));
                            }
                            if !mid.is_empty() {
                                let bullet = "*";
                                for _ in 0..mid.len() {
                                    new_spans
                                        .push(Span::styled(bullet, style.unwrap_or(orig_style)));
                                }
                            }
                            if !post.is_empty() {
                                new_spans
                                    .push(Span::styled(Cow::Owned(post.to_owned()), orig_style));
                            }
                        }
                    }
                } else {
                    new_spans.push(span);
                }
            } else {
                new_spans.push(span);
            }
            current += span_len;
        }
        // new_spans.shrink_to_fit();
        self.spans = new_spans;
    }
    #[inline]
    fn remove_slice(&mut self, range: Range<usize>) {
        // #[cfg(debug_assertions)]
        // debug!("Removing slice {:?}", range);

        let mut current = 0;

        let mut held_output = None;

        for (index, span) in self.spans.iter_mut().enumerate() {
            let span_len = span.content.len();
            let span_start = current;
            let span_end = current + span_len;

            let (overlap_start, overlap_end) =
                overlap_region((span_start, span_end), (range.start, range.end));

            if let Some((overlap_start, overlap_end)) = overlap_start.zip(overlap_end)
                && overlap_start < overlap_end
            {
                // This span is at least partly in the removal range.
                let offset_start = overlap_start - span_start;
                let offset_end = overlap_end - span_start;

                // Doubled up to give borrow checker more detailed info.
                match &span.content {
                    Cow::Borrowed(borrowed) => {
                        let (pre, _mid, post) =
                            split_span_content(borrowed, offset_start..offset_end);

                        match (pre.is_empty(), post.is_empty()) {
                            // empty! easy to handle
                            (true, true) => span.content = Cow::Borrowed(""),
                            // pre has content
                            (false, true) => {
                                span.content = Cow::Borrowed(pre);
                            }
                            // post has content!
                            // but this means we can leave now, as nothing else can
                            // be cut after it
                            (true, false) => {
                                span.content = Cow::Borrowed(post);
                                break;
                            }
                            // both had content!
                            // but this means we can just leave early!
                            (false, false) => {
                                span.content = Cow::Borrowed(pre);
                                held_output = Some((index + 1, Span::styled(post, span.style)));
                                break;
                            }
                        }
                    }
                    Cow::Owned(owned) => {
                        let (pre, _mid, post) = split_span_content(owned, offset_start..offset_end);

                        match (pre.is_empty(), post.is_empty()) {
                            // empty! easy to handle
                            (true, true) => span.content = Cow::Borrowed(""),
                            // pre has content
                            (false, true) => {
                                span.content = Cow::Owned(pre.to_owned());
                            }
                            // post has content!
                            (true, false) => {
                                span.content = Cow::Owned(post.to_owned());
                                break;
                            }
                            // both had content!!
                            (false, false) => {
                                held_output =
                                    Some((index + 1, Span::styled(post.to_owned(), span.style)));
                                span.content = Cow::Owned(pre.to_owned());
                                break;
                            }
                        }
                    }
                }
            }
            current += span_len;
        }
        if let Some((index, retained_span)) = held_output {
            self.spans.insert(index, retained_span);
        }
        // self.spans.retain(|s| !s.content.is_empty());
    }
}

/// Compute the overlap (start, end) region between two (start, end) pairs.
/// Both start/end are exclusive bounds.
fn overlap_region(a: (usize, usize), b: (usize, usize)) -> (Option<usize>, Option<usize>) {
    let (a_start, a_end) = a;
    let (b_start, b_end) = b;
    let overlap_start = a_start.max(b_start);
    let overlap_end = a_end.min(b_end);
    if overlap_start < overlap_end {
        (Some(overlap_start), Some(overlap_end))
    } else {
        (None, None)
    }
}

/// Splits the span's content into (pre, mid, post) based on byte offsets.
///
/// Assumes the range is on valid char boundaries, **will panic otherwise!**
#[inline]
fn split_span_content(content: &str, range: Range<usize>) -> (&str, &str, &str) {
    let pre = &content[..range.start];
    let mid = &content[range.start..range.end];
    let post = &content[range.end..];
    (pre, mid, post)
}

// Not traits, but helpful functions used in a few spots

// /// Interleaves two ordered iterators, yielding the smallest element from either iterator at each step.
// ///
// /// Elements are compared using their `Ord` implementation. If elements are equal,
// /// the element from the `left` iterator is yielded first. Once one iterator is exhausted,
// /// the rest of the elements from the other iterator are yielded.
// ///
// /// # Arguments
// ///
// /// * `left` - The first iterator to interleave.
// /// * `right` - The second iterator to interleave.
// ///
// /// # Returns
// ///
// /// An iterator yielding elements from both iterators in sorted order.
// #[inline]
// pub fn interleave<A, B, I>(left: A, right: B) -> impl Iterator<Item = I>
// where
//     A: Iterator<Item = I>,
//     B: Iterator<Item = I>,
//     I: Ord,
// {
//     let mut left = left.peekable();
//     let mut right = right.peekable();
//     std::iter::from_fn(move || match (left.peek(), right.peek()) {
//         (Some(li), Some(ri)) => {
//             if li <= ri {
//                 left.next()
//             } else {
//                 right.next()
//             }
//         }
//         (Some(_), None) => left.next(),
//         (None, Some(_)) => right.next(),
//         (None, None) => None,
//     })
// }

/// Interleaves two iterators using a closure to determine which iterator to pull from next.
///
/// The `decider` closure receives a reference to the next element of each iterator and should return `true`
/// to select the `left` iterator's item, or `false` to select the `right` iterator's item.
///
/// The item not chosen is not consumed, and will appear in the same binding next iteration.
///
/// Once one iterator is exhausted, the rest of the elements from the other iterator are yielded.
///
/// # Arguments
///
/// * `left` - The first iterator to interleave.
/// * `right` - The second iterator to interleave.
/// * `decider` - A function that takes references to items from each iterator and decides which to yield next.
///
/// # Returns
///
/// An iterator that yields values from both inputs, in the order determined by `decider`.
#[inline]
pub fn interleave_by<A, B, I, F>(left: A, right: B, mut decider: F) -> impl Iterator<Item = I>
where
    A: Iterator<Item = I>,
    B: Iterator<Item = I>,
    F: FnMut(&I, &I) -> bool,
{
    let mut left = left.peekable();
    let mut right = right.peekable();
    std::iter::from_fn(move || match (left.peek(), right.peek()) {
        (Some(li), Some(ri)) => {
            if decider(li, ri) {
                left.next()
            } else {
                right.next()
            }
        }
        (Some(_), None) => left.next(),
        (None, Some(_)) => right.next(),
        (None, None) => None,
    })
}

/// Simple trait for Keybind Actions to determine whether they should be allowed in certain conditions.
pub trait RequiresPort {
    /// Returns `true` if the Action requires an active, healthy connection to the port.
    fn requires_connection(&self) -> bool;
    /// Returns `true` if the Action requires at least the terminal view to be active (the port can be lent out or disconnected).
    ///
    /// Definition only required if conditions should differ from `requires_connection`.
    fn requires_terminal_view(&self) -> bool {
        self.requires_connection()
    }
}
