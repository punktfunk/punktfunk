//! Profile pictures on [`adw::Avatar`]s: the picker's circles and the host card's badge.

use adw::prelude::*;
use gtk::{gdk, glib};
use pf_client_core::library::HostApi;
use std::cell::RefCell;
use std::collections::HashMap;

/// The longest edge a picture decodes to, in device pixels: the picker's 64 at 4×.
const EDGE: i32 = 256;

thread_local! {
    /// Textures read this run, by URL, so a rebuilt card has its picture in its first frame.
    static TEXTURES: RefCell<HashMap<String, gdk::Texture>> = RefCell::default();
}

/// Puts the picture at `url` on `avatar`, read off the main loop. Without one the initials stay.
pub fn show(avatar: &adw::Avatar, url: &str, api: HostApi) {
    if let Some(texture) = TEXTURES.with_borrow(|t| t.get(url).cloned()) {
        return avatar.set_custom_image(Some(&texture));
    }
    let (tx, rx) = async_channel::bounded(1);
    let path = url.to_string();
    std::thread::spawn(move || {
        let bytes = pf_client_core::profiles::fetch_avatar(&api, &path);
        let decoded = bytes.and_then(|b| crate::library::art::decode(b, &(EDGE, EDGE)));
        let _ = tx.send_blocking(decoded);
    });
    let (avatar, url) = (avatar.downgrade(), url.to_string());
    glib::spawn_future_local(async move {
        let Ok(Some(decoded)) = rx.recv().await else {
            return;
        };
        let texture = decoded.texture();
        if let Some(avatar) = avatar.upgrade() {
            avatar.set_custom_image(Some(&texture));
        }
        TEXTURES.with_borrow_mut(|t| t.insert(url, texture));
    });
}
