use helix_tui::backend::{Backend, TestBackend};

#[test]
fn terminal_backend_size_should_not_be_limited() {
    let backend = TestBackend::new(400, 400);
    let size = backend.size().unwrap();
    assert_eq!(size.width, 400);
    assert_eq!(size.height, 400);
}
