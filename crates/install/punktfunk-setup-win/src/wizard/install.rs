//! Install: the phase checklist and the command transcript.

use super::*;

pub(super) fn install_page(w: &Wizard, cx: &ViewContext<'_, Wizard>) -> View {
    let dim = |text: String, size: f64| -> View {
        TextBlock::new()
            .text(text)
            .font_size(size)
            .text_wrapping(TextWrapping::Wrap)
            .foreground(w.palette.get(Token::TertiaryText))
            .into()
    };
    let items: Vec<View> = w
        .log
        .iter()
        .map(|line| match line.kind {
            LineKind::Phase => TextBlock::new()
                .text(&line.text)
                .font_weight(FontWeight::SEMI_BOLD)
                .font_size(13.5)
                .margin(Thickness::new(0.0, 10.0, 0.0, 2.0))
                .into(),
            LineKind::Cmd => TextBlock::new()
                .text(format!("  + {}", line.text))
                .font_size(12.0)
                .font_family(MONO)
                .text_wrapping(TextWrapping::Wrap)
                .foreground(w.palette.get(Token::TertiaryText))
                .into(),
            LineKind::Ok => note(w, format!("✓ {}", line.text)).into(),
            LineKind::Warn => TextBlock::new()
                .text(format!("⚠ {}", line.text))
                .text_wrapping(TextWrapping::Wrap)
                .foreground(w.palette.get(Token::SystemCaution))
                .into(),
            LineKind::Fail => TextBlock::new()
                .text(format!("✗ {}", line.text))
                .text_wrapping(TextWrapping::Wrap)
                .foreground(ThemeBrush::SystemCritical)
                .into(),
            LineKind::Detail => dim(format!("  {}", line.text), 12.0),
            LineKind::Text => TextBlock::new()
                .text(&line.text)
                .text_wrapping(TextWrapping::Wrap)
                .into(),
        })
        .collect();
    let status = match (w.install, w.uninstall) {
        (InstallPhase::Failed, _) => tools::wizard_install_failed(),
        (_, true) => tools::wizard_removing(),
        (_, false) => tools::wizard_installing(),
    };
    let mut head: Vec<View> = Vec::new();
    if w.install == InstallPhase::Running {
        head.push(
            ProgressRing::new()
                .is_active(true)
                .is_indeterminate(true)
                .width(16.0)
                .height(16.0)
                .into(),
        );
    }
    head.push(if w.install == InstallPhase::Failed {
        TextBlock::new()
            .text(status)
            .foreground(ThemeBrush::SystemCritical)
            .into()
    } else {
        note(w, status).into()
    });
    let content = StackPanel::new().spacing(12.0).children((
        StackPanel::new()
            .orientation(Orientation::Horizontal)
            .spacing(8.0)
            .children(head),
        // Range 0–100 is the widget's default; the value eases in `Wizard::bar`.
        ProgressBar::new().value(w.bar.eased() * 100.0),
        card(ScrollView::new().content(StackPanel::new().spacing(2.0).children(items)))
            .padding(Thickness::new(16.0, 10.0, 16.0, 12.0)),
    ));
    let buttons = if w.install == InstallPhase::Failed {
        vec![Button::new()
            .content(common::common_close())
            .on_click(cx.message(Msg::Exit(1)))
            .into()]
    } else {
        vec![] // no cancel mid-plan: the executor's steps are not interruptible-safe
    };
    frame(w, WizStep::Install, content, buttons)
}
