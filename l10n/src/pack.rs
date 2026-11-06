//! How a language's messages are stored. build.rs writes one string per language holding every
//! message body back to back, and one `u32` end offset per message; lib.rs reads them.
//!
//! - A plain body is its text, each placeholder written as `ARG` and one byte: `'0'` plus the
//!   placeholder's index in the English argument list.
//! - A plural body is one `CASE` per variant, then the byte `'0'` plus the variant's
//!   [`Category`] number and its text.
//! - A language without the message has an empty body.
//!
//! [`Category`]: crate::plural::Category

pub const ARG: char = '\u{1}';
pub const CASE: char = '\u{2}';
