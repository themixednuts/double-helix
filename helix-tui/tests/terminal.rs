use helix_tui::{
    backend::{Backend, CrosstermBackend, TestBackend},
    buffer::Buffer,
    terminal::{Terminal, TerminalOptions, Viewport},
};
use helix_view::{
    editor::KittyKeyboardProtocolConfig,
    graphics::Rect,
};

#[test]
fn terminal_buffer_size_should_not_be_limited() {
    let backend = TestBackend::new(400, 400);
    let terminal = Terminal::new(backend).unwrap();
    let size = terminal.backend().size().unwrap();
    assert_eq!(size.width, 400);
    assert_eq!(size.height, 400);
}

#[test]
#[ignore = "targeted local repro for large terminal diff serialization"]
fn terminal_flush_large_diff_repro() {
    let area = Rect::new(0, 0, 160, 61);
    let backend = CrosstermBackend::new(
        Vec::<u8>::new(),
        helix_tui::terminal::Config {
            enable_mouse_capture: false,
            force_enable_extended_underlines: false,
            kitty_keyboard_protocol: KittyKeyboardProtocolConfig::Auto,
        },
    );
    let mut terminal = Terminal::with_options(
        backend,
        TerminalOptions {
            viewport: Viewport::fixed(area),
        },
    )
    .expect("terminal");

    let mut previous = Buffer::empty(area);
    for y in 0..area.height {
        for x in 0..area.width {
            previous[(x, y)].set_symbol("a");
        }
    }
    *terminal.current_buffer_mut() = previous.clone();
    terminal
        .draw(None, helix_view::graphics::CursorKind::Hidden)
        .expect("first draw");

    let mut next = Buffer::empty(area);
    for y in 0..area.height {
        for x in 0..area.width {
            next[(x, y)].set_symbol(if (x + y) % 2 == 0 { "x" } else { "y" });
        }
    }
    let update_count = previous.diff(&next).len();
    *terminal.current_buffer_mut() = next;

    let start = std::time::Instant::now();
    terminal
        .draw(None, helix_view::graphics::CursorKind::Hidden)
        .expect("second draw");
    let elapsed = start.elapsed();

    eprintln!(
        "terminal_flush_large_diff_repro: elapsed_us={} updates={}",
        elapsed.as_micros(),
        update_count,
    );
}

// #[test]
// fn terminal_draw_returns_the_completed_frame() -> Result<(), Box<dyn Error>> {
//     let backend = TestBackend::new(10, 10);
//     let mut terminal = Terminal::new(backend)?;
//     let frame = terminal.draw(|f| {
//         let text = Text::from("Test");
//         let paragraph = Paragraph::new(&text);
//         f.render_widget(paragraph, f.size());
//     })?;
//     assert_eq!(frame.buffer.get(0, 0).symbol, "T");
//     assert_eq!(frame.area, Rect::new(0, 0, 10, 10));
//     terminal.backend_mut().resize(8, 8);
//     let frame = terminal.draw(|f| {
//         let text = Text::from("test");
//         let paragraph = Paragraph::new(&text);
//         f.render_widget(paragraph, f.size());
//     })?;
//     assert_eq!(frame.buffer.get(0, 0).symbol, "t");
//     assert_eq!(frame.area, Rect::new(0, 0, 8, 8));
//     Ok(())
// }
