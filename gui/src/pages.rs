//! Tests of the pages of the window, run in the headless testing backend of Slint.

use i_slint_backend_testing::ElementHandle;
use slint::ComponentHandle;

use crate::ui::{MainWindow, Model};

const SIDETONE_PAGE: i32 = 2;

/// The one element of the page that is a `type_name`; the tests do not look for labels, which
/// depend on the language of the system.
fn only(window: &MainWindow, type_name: &str) -> ElementHandle {
    let mut found = ElementHandle::find_by_element_type_name(window, type_name);
    let element = found.next().unwrap_or_else(|| panic!("no {type_name}"));
    assert!(found.next().is_none(), "more than one {type_name}");
    element
}

#[test]
fn the_sidetone_level_can_be_changed_only_while_sidetone_is_on() {
    i_slint_backend_testing::init_no_event_loop();
    let window = MainWindow::new().unwrap();
    let model = window.global::<Model>();
    model.set_tab(SIDETONE_PAGE);
    model.set_controls_enabled(true);
    model.set_sidetone(false);

    let level = only(&window, "Slider");
    let switch = only(&window, "Switch");
    assert_eq!(switch.accessible_checked(), Some(false));
    assert_eq!(level.accessible_enabled(), Some(false));

    switch.invoke_accessible_default_action();
    assert!(model.get_sidetone());
    assert_eq!(level.accessible_enabled(), Some(true));

    switch.invoke_accessible_default_action();
    assert!(!model.get_sidetone());
    assert_eq!(level.accessible_enabled(), Some(false));

    model.set_sidetone(true);
    model.set_controls_enabled(false);
    assert_eq!(level.accessible_enabled(), Some(false));
    assert_eq!(switch.accessible_enabled(), Some(false));
}
