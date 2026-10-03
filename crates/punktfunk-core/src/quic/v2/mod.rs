//! `punktfunk/2` — the wire behind ALPN `pkf2` that replaces `punktfunk/1`.
//!
//! A frame is `type ‖ len ‖ field*` and a field is `tag ‖ len ‖ value`, with QUIC varints for
//! the numbers ([`field`]). A reader skips a type or a tag it does not know, so a message grows
//! by adding a tag; nothing is positional. Every number on this wire is allocated in
//! [`registry`], and the capability bitset is [`features::FeatureSet`].
//!
//! The structs in [`super`] are the semantic model both wires share: the v2 codecs read and
//! write them, so host and client logic does not care which wire a session runs.

pub mod features;
pub mod field;
pub mod registry;
