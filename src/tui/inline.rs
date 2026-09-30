use super::transcript::Transcript;
use anyhow::Result;
use ratatui::{
    backend::Backend,
    layout::{Rect, Size},
    style::{Color, Style},
    widgets::{Paragraph, Widget, Wrap},
    Terminal, TerminalOptions, Viewport,
};

pub fn options(height: u16) -> TerminalOptions {
    TerminalOptions {
        viewport: Viewport::Inline(height),
    }
}
pub fn bottom_rect(size: Size, height: u16) -> Rect {
    Rect::new(
        0,
        size.height.saturating_sub(height),
        size.width,
        height.min(size.height),
    )
}

// Ratatui's inline height is immutable. Rebuild only when geometry changes, using
// the same terminal surface. Clear transient content before full-screen scrolling
// so composer/popup cells never become transcript or native scrollback.
pub fn sync_bottom<B: Backend>(
    terminal: &mut Terminal<B>,
    height: u16,
    fresh_backend: impl FnOnce(&B) -> B,
) -> Result<()> {
    let current_size = terminal.size()?;
    let previous_area = terminal.get_frame().area();
    if previous_area.bottom() > current_size.height || previous_area.width > current_size.width {
        let cursor = terminal.backend_mut().get_cursor_position()?;
        terminal.backend_mut().set_cursor_position((
            cursor.x.min(current_size.width.saturating_sub(1)),
            cursor.y.min(current_size.height.saturating_sub(1)),
        ))?;
    }
    terminal.autoresize()?;
    let size = terminal.size()?;
    let desired = bottom_rect(size, height);
    let previous = terminal.get_frame().area();
    if previous == desired {
        return Ok(());
    }
    terminal.clear()?;
    let scroll = previous.y.saturating_sub(desired.y);
    if scroll > 0 {
        terminal
            .backend_mut()
            .set_cursor_position((0, size.height.saturating_sub(1)))?;
        terminal.backend_mut().append_lines(scroll)?;
    }
    terminal
        .backend_mut()
        .set_cursor_position(desired.as_position())?;
    *terminal = Terminal::with_options(fresh_backend(terminal.backend()), options(desired.height))?;
    Ok(())
}

pub fn append_pending<B: Backend>(
    terminal: &mut Terminal<B>,
    transcript: &mut Transcript,
) -> Result<()> {
    terminal.autoresize()?;
    let size = terminal.size()?;
    let pane_height = terminal.get_frame().area().height;
    let width = size.width.max(1);
    let chunk = size.height.saturating_sub(pane_height).max(1);
    while let Some(entry) = transcript.pending.pop_front() {
        let paragraph = Paragraph::new(entry.text)
            .style(Style::default().fg(entry.color.unwrap_or(Color::Reset)))
            .wrap(Wrap { trim: false });
        let total = paragraph.line_count(width).max(1);
        let mut offset = 0;
        while offset < total {
            let rows = (total - offset).min(chunk as usize) as u16;
            terminal.insert_before(rows, |buf| {
                paragraph
                    .clone()
                    .scroll((offset.min(u16::MAX as usize) as u16, 0))
                    .render(buf.area, buf)
            })?;
            offset += rows as usize;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{backend::TestBackend, widgets::Paragraph};
    #[test]
    fn pane_is_fixed_before_and_after_transcript_insertions_and_resize() {
        let mut terminal = Terminal::with_options(TestBackend::new(40, 20), options(4)).unwrap();
        sync_bottom(&mut terminal, 4, Clone::clone).unwrap();
        let expected = bottom_rect(Size::new(40, 20), 4);
        assert_eq!(terminal.get_frame().area(), expected);
        let mut transcript = Transcript::default();
        for i in 0..30 {
            transcript.push(format!("entry {i}"), None);
            append_pending(&mut terminal, &mut transcript).unwrap();
            assert_eq!(terminal.get_frame().area(), expected);
        }
        let scrollback = terminal.backend().scrollback().clone();
        append_pending(&mut terminal, &mut transcript).unwrap();
        assert_eq!(*terminal.backend().scrollback(), scrollback);
        terminal.backend_mut().resize(30, 25);
        sync_bottom(&mut terminal, 6, Clone::clone).unwrap();
        assert_eq!(
            terminal.get_frame().area(),
            bottom_rect(Size::new(30, 25), 6)
        );
        terminal.backend_mut().resize(25, 12);
        sync_bottom(&mut terminal, 4, Clone::clone).unwrap();
        assert_eq!(
            terminal.get_frame().area(),
            bottom_rect(Size::new(25, 12), 4)
        );
    }
    #[test]
    fn popup_is_removed_before_growing_pane_or_adding_transcript() {
        let mut terminal = Terminal::with_options(TestBackend::new(40, 20), options(4)).unwrap();
        sync_bottom(&mut terminal, 4, Clone::clone).unwrap();
        terminal
            .draw(|f| f.render_widget(Paragraph::new("temporary popup"), f.area()))
            .unwrap();
        sync_bottom(&mut terminal, 10, Clone::clone).unwrap();
        terminal
            .draw(|f| f.render_widget(Paragraph::new("temporary popup"), f.area()))
            .unwrap();
        let mut transcript = Transcript::default();
        for _ in 0..30 {
            transcript.push("human transcript", None);
        }
        append_pending(&mut terminal, &mut transcript).unwrap();
        let scrollback = terminal
            .backend()
            .scrollback()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect::<String>();
        assert!(scrollback.contains("human transcript"));
        assert!(!scrollback.contains("temporary popup"));
        sync_bottom(&mut terminal, 4, Clone::clone).unwrap();
        assert_eq!(terminal.get_frame().area().bottom(), 20);
    }
}
