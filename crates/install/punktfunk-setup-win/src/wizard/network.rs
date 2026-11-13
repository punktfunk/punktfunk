//! Network: the bind and exposure answer.

use super::*;

pub(super) fn network_page(w: &Wizard, cx: &ViewContext<'_, Wizard>) -> View {
    let name = w.network_name();
    let (title, make_private) = match &name {
        Some(name) => (
            tools::wizard_network_title(name),
            tools::wizard_network_make_private(name),
        ),
        None => (
            tools::wizard_network_title_unnamed().to_string(),
            tools::wizard_network_make_private_unnamed().to_string(),
        ),
    };
    let selected = match &w.screen.choices.network {
        NetworkAnswer::MakePrivate(_) => 0,
        NetworkAnswer::OpenPublicRules => 1,
        NetworkAnswer::Skip => 2,
    };
    let content = StackPanel::new().spacing(12.0).max_width(640.0).children((
        TextBlock::new()
            .text(title)
            .font_size(16.0)
            .font_weight(FontWeight::SEMI_BOLD),
        note(w, tools::wizard_network_body()),
        RadioButtons::new()
            .items_source([
                make_private,
                tools::wizard_network_open_public().to_string(),
                tools::wizard_network_skip().to_string(),
            ])
            .selected_index(Some(selected))
            .on_selection_changed(cx.callback(Msg::Network)),
    ));
    frame(
        w,
        WizStep::Network,
        content,
        vec![
            back_button(cx, WizStep::Network),
            continue_button(cx, WizStep::Network, tools::wizard_install()),
        ],
    )
}
