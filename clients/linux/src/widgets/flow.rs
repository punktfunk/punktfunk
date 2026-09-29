//! One shared fix for a GTK4 footgun every card grid in this shell walks into.
//!
//! A pointer click (and keyboard activate) on a [`gtk::FlowBoxChild`] emits
//! `child-activated` on the *FlowBox*, never the child's own `activate` signal — so a
//! grid whose per-card handler hangs off `child.connect_activate()` has to bridge the
//! one to the other. The naive bridge is a stack overflow: `FlowBoxChild`'s default
//! `activate` handler re-emits `child-activated` on its parent, which calls the bridge,
//! which activates the child again, forever.
//!
//! [`bridge_child_activation`] is that bridge with the re-entrancy guard that breaks the
//! cycle. Use it for every `child-activated → child.activate()` hop; do not hand-roll it,
//! since a bare `flow.connect_child_activated(|_, c| c.activate())` aborts the process on
//! the first click and looks perfectly reasonable in review.

use gtk::prelude::*;

/// Bridge a FlowBox's `child-activated` to the activated child's own `activate` signal,
/// exactly once per click. The re-entrant emission the child's default handler bounces
/// back is swallowed rather than recursed into.
pub(crate) fn bridge_child_activation(flow: &gtk::FlowBox) {
    let activating = std::cell::Cell::new(false);
    flow.connect_child_activated(move |_, child| {
        if activating.replace(true) {
            return;
        }
        child.activate();
        activating.set(false);
    });
}

#[cfg(test)]
mod tests {
    use super::bridge_child_activation;
    use gtk::prelude::*;
    use std::cell::Cell;
    use std::rc::Rc;

    // Reproduces the exact FlowBox/FlowBoxChild wiring the card grids use: the bridge
    // calls `child.activate()`, whose own default handler re-emits `child-activated` —
    // that ping-pong recursed forever (a real stack overflow on every card click/Enter,
    // reported on the hosts page and then again on the library page) until the
    // re-entrancy guard landed here, where both pages share it.
    #[test]
    #[ignore = "needs a Wayland/X display"]
    fn flow_box_activation_bridge_does_not_recurse() {
        assert!(gtk::init().is_ok(), "no display");

        let flow = gtk::FlowBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .activate_on_single_click(true)
            .build();
        bridge_child_activation(&flow);

        let child = gtk::FlowBoxChild::new();
        flow.insert(&child, -1);
        let fired = Rc::new(Cell::new(0u32));
        {
            let fired = fired.clone();
            child.connect_activate(move |_| fired.set(fired.get() + 1));
        }

        flow.emit_by_name::<()>("child-activated", &[&child]);

        assert_eq!(
            fired.get(),
            1,
            "the per-card handler should fire exactly once"
        );
    }
}
