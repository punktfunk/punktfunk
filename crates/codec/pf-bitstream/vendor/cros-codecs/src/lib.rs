// Copyright 2022 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Vendored cros-codecs **parser layer**: the `codec` module (H.264 / H.265 / AV1 / VP9
//! parsers, DPBs, picture types) plus `bitstream_utils`. The decoder/encoder/backend
//! halves of upstream are deliberately not vendored — punktfunk's own decode layer
//! (`pf-bitstream`) sits where upstream's `decoder::stateless` would.
//!
//! This lib.rs is the one heavily-trimmed file: upstream's carries the feature-gated
//! backend modules and CLI-facing enums; only the items the `codec` module actually
//! references survive here (`Resolution` and its round mode). Everything below this
//! module doc is copied verbatim from upstream lib.rs. See PROVENANCE.md for the
//! snapshot source and the full list of deviations.

// Vendored code is not held to the workspace lint bar: CI's `--workspace -- -D warnings`
// clippy leg would otherwise fail on upstream style (27 warnings at vendoring time).
// Held-back lints here, PROVENANCE.md records the posture.
#![allow(clippy::all)]
#![allow(mismatched_lifetime_syntaxes)]
// The one bar vendored code IS held to, and the whole point of this layer: the code that
// parses hostile bitstream bytes contains no unsafe, compiler-enforced. Upstream was one
// pointer-subtraction away from this already (PROVENANCE.md #5); a re-sync that brings
// unsafe into the codec module must fail here and be judged, not slide in.
#![forbid(unsafe_code)]

pub mod bitstream_utils;
pub mod codec;

/// Rounding modes for `Resolution`
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ResolutionRoundMode {
    /// Rounds component-wise to the next even value.
    Even,
}

/// A frame resolution in pixels.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct Resolution {
    pub width: u32,
    pub height: u32,
}

impl Resolution {
    /// Whether `self` can contain `other`.
    pub fn can_contain(&self, other: Self) -> bool {
        self.width >= other.width && self.height >= other.height
    }

    /// Rounds `self` according to `rnd_mode`.
    pub fn round(mut self, rnd_mode: ResolutionRoundMode) -> Self {
        match rnd_mode {
            ResolutionRoundMode::Even => {
                if self.width % 2 != 0 {
                    self.width += 1;
                }

                if self.height % 2 != 0 {
                    self.height += 1;
                }
            }
        }

        self
    }

    pub fn get_area(&self) -> usize {
        (self.width as usize) * (self.height as usize)
    }
}

impl From<(u32, u32)> for Resolution {
    fn from(value: (u32, u32)) -> Self {
        Self {
            width: value.0,
            height: value.1,
        }
    }
}

impl From<Resolution> for (u32, u32) {
    fn from(value: Resolution) -> Self {
        (value.width, value.height)
    }
}
