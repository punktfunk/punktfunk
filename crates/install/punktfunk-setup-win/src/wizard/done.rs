//! Done: the outcome, the fresh host's password card and the next steps.

use super::*;

/// What Done shows in place of the password until Show is pressed.
pub const PASSWORD_MASK: &str = "••••••••••••";

/// Segoe Fluent glyphs for the password card's icon buttons.
const GLYPH_VIEW: &str = "\u{E890}";
const GLYPH_HIDE: &str = "\u{ED1A}";
const GLYPH_COPY: &str = "\u{E8C8}";
const GLYPH_ACCEPT: &str = "\u{E8FB}";

/// A square, icon-only button; `name` is what UI Automation (and the flow tests) call it.
fn icon_button(cx: &ViewContext<'_, Wizard>, glyph: &str, name: &str, msg: Msg) -> View {
    Button::new()
        .content(FontIcon::new().glyph(glyph))
        .width(32.0)
        .height(32.0)
        .automation_name(name)
        .resource_overrides(ResourceOverrides::new().set("ButtonPadding", Thickness::uniform(0.0)))
        .on_click(cx.message(msg))
        .into()
}

/// Plain text onto the system clipboard. Best effort: a refused clipboard (another app holding
/// it, no UI thread in a test host) is not worth an error on the finish page.
pub(super) fn copy_to_clipboard(text: &str) {
    use windows::ApplicationModel::DataTransfer::{Clipboard, DataPackage};
    let _ = (|| -> windows::core::Result<()> {
        let package = DataPackage::new()?;
        package.SetText(&windows::core::HSTRING::from(text))?;
        Clipboard::SetContent(&package)?;
        Clipboard::Flush()
    })();
}

/// A link button; a URI WinUI refuses renders as a plain label.
fn link(label: &str, uri: &str) -> HyperlinkButton {
    HyperlinkButton::new()
        .content(label)
        .navigate_uri(uri)
        .unwrap_or_else(|_| HyperlinkButton::new().content(label))
}

/// A follow-up action on Done: what to do, why, and the link that does it.
fn next_step(w: &Wizard, title: &str, detail: &str, label: &str, uri: &str) -> Border {
    card(
        StackPanel::new().spacing(4.0).children((
            TextBlock::new()
                .text(title)
                .font_weight(FontWeight::SEMI_BOLD),
            note(w, detail).font_size(12.0),
            link(label, uri).margin(Thickness::new(-10.0, 4.0, 0.0, 0.0)),
        )),
    )
}

pub(super) fn done_page(w: &Wizard, cx: &ViewContext<'_, Wizard>) -> View {
    let children = if w.uninstall {
        done_uninstalled(w)
    } else {
        done_installed(w, cx)
    };
    let content = ScrollView::new().content(
        StackPanel::new()
            .spacing(12.0)
            .max_width(760.0)
            .children(children),
    );
    frame(
        w,
        WizStep::Done,
        content,
        vec![Button::new()
            .content(tools::wizard_finish())
            .style(ButtonStyle::Accent)
            .min_width(124.0)
            .on_click(cx.message(Msg::Exit(0)))
            .into()],
    )
}

fn done_uninstalled(w: &Wizard) -> Vec<View> {
    vec![
        TextBlock::new()
            .text(tools::wizard_done_removed())
            .font_size(18.0)
            .font_weight(FontWeight::SEMI_BOLD)
            .into(),
        note(w, tools::wizard_done_kept()).into(),
    ]
}

/// The outcome, the password card on a fresh host, the next steps, and the D11/D12
/// footnotes in the transcript's words as attention cards.
fn done_installed(w: &Wizard, cx: &ViewContext<'_, Wizard>) -> Vec<View> {
    use punktfunk_setup::facts::DOCS;
    let fresh = w.screen.fresh();
    let mut children: Vec<View> = vec![TextBlock::new()
        .text(match (w.preset.artifact, fresh) {
            (Artifact::Host, true) => tools::wizard_done_host_installed(),
            (Artifact::Host, false) => tools::wizard_done_host_updated(),
            (Artifact::Client, true) => tools::wizard_done_client_installed(),
            (Artifact::Client, false) => tools::wizard_done_client_updated(),
        })
        .font_size(18.0)
        .font_weight(FontWeight::SEMI_BOLD)
        .into()];
    // Fresh host: the password card, the one thing the user must leave with (D9) — and the
    // only place a Recommended install shows it.
    if w.preset.artifact == Artifact::Host
        && fresh
        && let Some(pw) = &w.screen.choices.web_password
    {
        children.push(password_card(w, cx, pw));
    }
    children.push(
        note(w, tools::wizard_done_next())
            .font_size(12.0)
            .font_weight(FontWeight::SEMI_BOLD)
            .margin(Thickness::new(2.0, 6.0, 0.0, -4.0))
            .into(),
    );
    let (first, second) = next_steps(w, w.preset.artifact);
    children.push(
        Grid::new()
            .columns([GridLength::STAR, GridLength::STAR])
            .column_spacing(12.0)
            .children((first.grid_column(0), second.grid_column(1)))
            .into(),
    );
    for footnote in win_report::footnotes(&w.screen.facts, &w.screen.effective_choices()) {
        children.push(
            card(
                TextBlock::new()
                    .text(footnote)
                    .text_wrapping(TextWrapping::Wrap)
                    .foreground(w.palette.get(Token::SystemAttention)),
            )
            .into(),
        );
    }
    children.push(
        link(
            tools::wizard_done_stuck(),
            &format!("{DOCS}/troubleshooting"),
        )
        .margin(Thickness::new(-10.0, 0.0, 0.0, 0.0))
        .into(),
    );
    children
}

/// The fresh host's console password, masked until Show: a finish page can sit on a screen
/// for a while. Copy works either way. Changing it later is a file edit and a service
/// restart (docs: web-console.md), not a console feature.
fn password_card(w: &Wizard, cx: &ViewContext<'_, Wizard>, pw: &str) -> View {
    let reveal = w.reveal_done;
    card(
        StackPanel::new().spacing(6.0).children((
            TextBlock::new()
                .text(tools::wizard_password_title())
                .font_weight(FontWeight::SEMI_BOLD),
            StackPanel::new()
                .orientation(Orientation::Horizontal)
                .spacing(6.0)
                .vertical_alignment(VerticalAlignment::Center)
                .children((
                    TextBlock::new()
                        .text(if reveal { pw } else { PASSWORD_MASK })
                        .font_size(22.0)
                        .font_family(MONO)
                        .is_text_selection_enabled(true)
                        .min_width(240.0)
                        .vertical_alignment(VerticalAlignment::Center),
                    icon_button(
                        cx,
                        if reveal { GLYPH_HIDE } else { GLYPH_VIEW },
                        if reveal {
                            tools::wizard_password_hide()
                        } else {
                            tools::wizard_password_show()
                        },
                        Msg::RevealDone(!reveal),
                    ),
                    icon_button(
                        cx,
                        if w.copied { GLYPH_ACCEPT } else { GLYPH_COPY },
                        if w.copied {
                            tools::wizard_password_copied()
                        } else {
                            tools::wizard_password_copy()
                        },
                        Msg::Copy,
                    ),
                )),
            note(w, tools::wizard_password_stored()).font_size(12.0),
        )),
    )
    .into()
}

/// The two follow-up cards for what was installed.
fn next_steps(w: &Wizard, artifact: Artifact) -> (Border, Border) {
    use punktfunk_setup::facts::DOCS;
    match artifact {
        Artifact::Host => (
            next_step(
                w,
                tools::wizard_next_console(),
                tools::wizard_next_console_body(),
                "https://127.0.0.1:47992",
                "https://127.0.0.1:47992/",
            ),
            next_step(
                w,
                tools::wizard_next_client(),
                tools::wizard_next_client_body(),
                tools::wizard_next_client_link(),
                &format!("{DOCS}/install-client"),
            ),
        ),
        Artifact::Client => (
            next_step(
                w,
                tools::wizard_next_open(),
                tools::wizard_next_open_body(),
                tools::wizard_next_pairing_link(),
                &format!("{DOCS}/pairing"),
            ),
            next_step(
                w,
                tools::wizard_next_no_host(),
                tools::wizard_next_no_host_body(),
                tools::wizard_next_host_link(),
                &format!("{DOCS}/windows-host"),
            ),
        ),
    }
}
