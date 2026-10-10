//! The Windows installer wizard as a library, so the flow tests can drive it through
//! reactor's headless harness (`tests/wizard_flow.rs`) the way `clients/windows` drives its
//! shell. `main.rs` is the thin exe over this.

// Cross-platform: the pack step and its tests run on every lane.
pub mod overlay;
pub mod pack;
pub mod payload;

#[cfg(windows)]
pub mod bootstrap;
#[cfg(windows)]
pub mod brand;
#[cfg(windows)]
pub mod real;
#[cfg(windows)]
pub mod silent;
#[cfg(windows)]
pub mod wizard;
