//! Tests of the pages of the window, run in the headless testing backend of Slint. They look for
//! elements by their type, not by their labels, which depend on the language of the system.

use std::cell::RefCell;
use std::rc::Rc;

use cxtune::engine::{TestEvent, TestKind, TestProgress, TestState, TestStep as EngineStep};
use i_slint_backend_testing::ElementHandle;
use slint::ComponentHandle;

use crate::test_tab::{self, TestTab};
use crate::ui::{MainWindow, Model, TestJob, TestProblem, TestStep};

const OUTPUT_PAGE: i32 = 0;
const SIDETONE_PAGE: i32 = 2;

fn all(window: &MainWindow, type_name: &str) -> Vec<ElementHandle> {
    ElementHandle::find_by_element_type_name(window, type_name).collect()
}

/// The one element of the page that is a `type_name`.
fn only(window: &MainWindow, type_name: &str) -> ElementHandle {
    let mut found = all(window, type_name);
    assert_eq!(found.len(), 1, "{type_name}");
    found.remove(0)
}

fn enabled(elements: &[ElementHandle]) -> Vec<bool> {
    elements
        .iter()
        .map(|element| element.accessible_enabled().expect("an enabled state"))
        .collect()
}

fn window() -> MainWindow {
    i_slint_backend_testing::init_no_event_loop();
    MainWindow::new().unwrap()
}

/// A window of the size of the real one, so that what does not fit scrolls. The elements of a page
/// that are scrolled out of view are not found, which the other tests do not want.
fn real_window() -> MainWindow {
    let window = window();
    window
        .window()
        .set_size(slint::LogicalSize::new(720.0, 480.0));
    window
}

#[test]
fn the_sidetone_level_can_be_changed_only_while_sidetone_is_on() {
    let window = window();
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

#[test]
fn the_surround_level_can_be_changed_only_while_surround_is_on() {
    let window = window();
    let model = window.global::<Model>();
    model.set_tab(OUTPUT_PAGE);
    model.set_controls_enabled(true);
    model.set_surround(false);

    // the sliders of the page, in order: the speaker volume, the soft-start time, the release
    // time and the surround level
    let sliders = all(&window, "Slider");
    assert_eq!(sliders.len(), 4);
    let surround_level = &sliders[3];
    let switch = only(&window, "Switch");
    assert_eq!(surround_level.accessible_enabled(), Some(false));

    switch.invoke_accessible_default_action();
    assert!(model.get_surround());
    assert_eq!(surround_level.accessible_enabled(), Some(true));

    switch.invoke_accessible_default_action();
    assert_eq!(surround_level.accessible_enabled(), Some(false));

    // without a dongle the level stays disabled whatever the switch says
    model.set_surround(true);
    model.set_controls_enabled(false);
    assert_eq!(surround_level.accessible_enabled(), Some(false));
}

#[test]
fn each_time_slider_follows_the_engine_enabled_state_of_its_mode() {
    let window = window();
    let model = window.global::<Model>();
    model.set_tab(OUTPUT_PAGE);
    let sliders = all(&window, "Slider");
    let (soft_start, release_time) = (&sliders[1], &sliders[2]);

    for (soft, decay) in [(false, false), (true, false), (false, true)] {
        model.set_settle_enabled(soft);
        model.set_decay_enabled(decay);
        assert_eq!(soft_start.accessible_enabled(), Some(soft));
        assert_eq!(release_time.accessible_enabled(), Some(decay));
    }
}

/// The buttons of the Test page, in order: the noise floor, the recording, playing it, the noise
/// reduction comparison, the click test and listening to the noise.
fn test_buttons(window: &MainWindow) -> Vec<ElementHandle> {
    let buttons = all(window, "TestButton");
    assert_eq!(buttons.len(), 6);
    buttons
}

const JOBS: [TestJob; 6] = [
    TestJob::NoiseFloor,
    TestJob::Record,
    TestJob::Play,
    TestJob::Comparison,
    TestJob::Click,
    TestJob::Listen,
];

#[test]
fn the_test_buttons_are_enabled_where_the_engine_would_not_refuse_the_request() {
    let window = window();
    let model = window.global::<Model>();
    model.set_tab(test_tab::TAB);
    model.set_test_available(true);
    let buttons = test_buttons(&window);

    // nothing to play yet
    assert_eq!(enabled(&buttons), [true, true, false, true, true, true]);
    model.set_test_has_recording(true);
    assert_eq!(enabled(&buttons), [true; 6]);

    // while a test runs the engine refuses every other one: only the button of the test that runs
    // can be used, and it cancels
    for (index, job) in JOBS.into_iter().enumerate() {
        model.set_test_job(job);
        let only_this: Vec<bool> = (0..JOBS.len()).map(|i| i == index).collect();
        assert_eq!(enabled(&buttons), only_this, "{job:?}");
    }
    model.set_test_job(TestJob::None);

    // the engine refuses all of them while the dongle is not there (not connected, or not read
    // since it connected or the computer woke up), whatever the other controls can do
    model.set_controls_enabled(true);
    model.set_test_available(false);
    assert_eq!(enabled(&buttons), [false; 6]);
    model.set_controls_enabled(false);
    model.set_test_available(true);
    assert_eq!(enabled(&buttons), [true; 6]);
}

#[test]
fn the_level_switch_is_off_until_the_user_turns_it_on_and_follows_what_rust_says() {
    let window = window();
    let model = window.global::<Model>();
    let toggled = Rc::new(RefCell::new(Vec::new()));
    model.on_meter_toggled({
        let toggled = toggled.clone();
        move |on| toggled.borrow_mut().push(on)
    });
    model.set_test_available(true);
    for tab in [OUTPUT_PAGE, test_tab::TAB] {
        model.set_tab(tab);
        slint::platform::update_timers_and_animations();
    }
    let switch = only(&window, "Switch");
    assert_eq!(switch.accessible_checked(), Some(false));
    assert!(
        toggled.borrow().is_empty(),
        "entering the page asks for nothing"
    );

    switch.invoke_accessible_default_action();
    assert_eq!(*toggled.borrow(), [true]);
    assert_eq!(switch.accessible_checked(), Some(true));
    // Rust turns it off where the meter may not run, and the switch shows it
    model.set_meter_on(false);
    assert_eq!(switch.accessible_checked(), Some(false));
    switch.invoke_accessible_default_action();
    assert_eq!(*toggled.borrow(), [true, true]);
}

#[test]
fn the_level_switch_can_be_used_only_where_the_engine_would_open_the_microphone() {
    let window = window();
    let model = window.global::<Model>();
    model.set_tab(test_tab::TAB);
    let switch = only(&window, "Switch");

    assert_eq!(switch.accessible_enabled(), Some(false), "no dongle");
    model.set_test_available(true);
    assert_eq!(switch.accessible_enabled(), Some(true));
    // the engine refuses the meter while a test runs, and a measurement is no reason to turn it
    // off in the middle of it
    for job in JOBS {
        model.set_test_job(job);
        assert_eq!(switch.accessible_enabled(), Some(false), "{job:?}");
    }
    model.set_test_job(TestJob::None);
    // it is not the controls that change the dongle that it follows
    model.set_controls_enabled(true);
    model.set_test_available(false);
    assert_eq!(switch.accessible_enabled(), Some(false));
}

#[test]
fn a_test_button_starts_its_test_and_while_it_runs_cancels_it() {
    let window = window();
    let model = window.global::<Model>();
    model.set_tab(test_tab::TAB);
    model.set_test_available(true);
    model.set_test_has_recording(true);
    let buttons = test_buttons(&window);

    let requested = Rc::new(RefCell::new(Vec::new()));
    let cancelled = Rc::new(RefCell::new(0));
    model.on_test_requested({
        let requested = requested.clone();
        move |job| requested.borrow_mut().push(job)
    });
    model.on_test_cancelled({
        let cancelled = cancelled.clone();
        move || *cancelled.borrow_mut() += 1
    });

    for (button, job) in buttons.iter().zip(JOBS) {
        button.invoke_accessible_default_action();
        assert_eq!(requested.borrow().last(), Some(&job));
    }
    assert_eq!(requested.borrow().len(), JOBS.len());
    assert_eq!(*cancelled.borrow(), 0);

    for (button, job) in buttons.iter().zip(JOBS) {
        let start = button.accessible_label();
        model.set_test_job(job);
        assert_ne!(button.accessible_label(), start, "{job:?} says Cancel");
        button.invoke_accessible_default_action();
    }
    assert_eq!(*cancelled.borrow(), JOBS.len());
    assert_eq!(requested.borrow().len(), JOBS.len(), "nothing was started");
}

#[test]
fn the_page_change_reaches_rust_so_that_the_meter_can_follow_it() {
    let window = window();
    let model = window.global::<Model>();
    let seen = Rc::new(RefCell::new(Vec::new()));
    model.on_tab_changed({
        let seen = seen.clone();
        move |tab| seen.borrow_mut().push(tab)
    });
    for tab in [test_tab::TAB, OUTPUT_PAGE, test_tab::TAB] {
        model.set_tab(tab);
        slint::platform::update_timers_and_animations();
    }
    assert_eq!(*seen.borrow(), [test_tab::TAB, OUTPUT_PAGE, test_tab::TAB]);
}

#[test]
fn minimizing_and_restoring_the_window_reach_rust_so_that_the_meter_can_follow() {
    let window = window();
    let model = window.global::<Model>();
    let seen = Rc::new(RefCell::new(Vec::new()));
    model.on_minimized_changed({
        let seen = seen.clone();
        move |minimized| seen.borrow_mut().push(minimized)
    });
    for minimized in [true, false] {
        window.window().set_minimized(minimized);
        slint::platform::update_timers_and_animations();
    }
    assert_eq!(*seen.borrow(), [true, false]);
}

#[test]
fn the_page_shows_what_the_tab_holds() {
    let window = window();
    let model = window.global::<Model>();
    let mut tab = TestTab::default();
    tab.switched(true);
    tab.state(
        &TestState {
            meter: true,
            running: Some(TestKind::NoiseReductionAb),
            has_recording: true,
        },
        true,
        true,
    );
    tab.event(&TestEvent::Level(cxtune::capture::Level {
        rms_dbfs: -52.0,
        peak_dbfs: -40.0,
    }));
    tab.event(&TestEvent::Progress(TestProgress {
        kind: TestKind::NoiseReductionAb,
        step: EngineStep::MeasuringWithoutNoiseReduction,
        remaining_ms: Some(7_300),
    }));
    tab.show(&model);
    assert_eq!(model.get_test_job(), TestJob::Comparison);
    assert_eq!(model.get_test_step(), TestStep::WithoutNoiseReduction);
    assert_eq!(model.get_test_remaining(), 7_300);
    assert!(model.get_test_has_recording());
    assert!(model.get_test_caution());
    assert_eq!(model.get_test_problem(), TestProblem::None);
    assert!(model.get_test_available());
    assert!(model.get_meter_on());
    assert!(model.get_meters_active());
    assert_eq!(model.get_mic_db(), -52.0);

    // a test that plays turned the meter off: no level, no countdown, and the caution is gone
    // with the sound
    tab.state(&TestState::default(), true, true);
    tab.requested(TestJob::Click);
    tab.show(&model);
    assert_eq!(model.get_test_job(), TestJob::None);
    assert_eq!(model.get_test_remaining(), -1);
    assert!(!model.get_test_caution());
    assert!(!model.get_meter_on());
    assert!(!model.get_meters_active());
}

const PAGES: i32 = 6;

/// Left, top, width and height of an element, in logical pixels of the window.
fn rect(element: &ElementHandle) -> [f32; 4] {
    let (position, size) = (element.absolute_position(), element.size());
    [position.x, position.y, size.width, size.height]
}

/// The heading of the page that is shown: its title, or the breadcrumb of the About page. A tab
/// that is not a page has none.
fn headings(window: &MainWindow) -> Vec<ElementHandle> {
    let mut found = all(window, "PageTitle");
    found.extend(all(window, "Breadcrumb"));
    found
}

#[test]
fn every_page_is_in_a_scroll_view_that_fills_the_page_area() {
    let window = real_window();
    let model = window.global::<Model>();
    let mut pages = Vec::new();
    // a few tabs past the last page, to catch a page that was added
    for tab in 0..PAGES + 4 {
        model.set_tab(tab);
        let mut headings = headings(&window);
        if headings.is_empty() {
            continue;
        }
        pages.push(tab);
        assert_eq!(headings.len(), 1, "page {tab}");
        let heading = rect(&headings.remove(0));

        // right of the navigation, between the header and the footer
        let area = [160.0, 64.0, 560.0, 356.0];
        assert_eq!(rect(&only(&window, "Page")), area, "page {tab}");
        assert_eq!(rect(&only(&window, "ScrollView")), area, "page {tab}");
        assert!(
            heading[0] >= area[0]
                && heading[1] >= area[1]
                && heading[0] + heading[2] <= area[0] + area[2]
                && heading[1] + heading[3] <= area[1] + area[3],
            "the heading of page {tab} is in the scroll view"
        );
    }
    assert_eq!(pages, (0..PAGES).collect::<Vec<_>>());
}

#[test]
fn the_title_scrolls_with_the_content_of_a_page_that_does_not_fit() {
    use slint::LogicalPosition;
    use slint::platform::WindowEvent;

    let window = real_window();
    for tab in [OUTPUT_PAGE, test_tab::TAB] {
        window.global::<Model>().set_tab(tab);
        let title = only(&window, "PageTitle");
        let before = rect(&title);
        window
            .window()
            .dispatch_event(WindowEvent::PointerScrolled {
                position: LogicalPosition::new(450.0, 250.0),
                delta_x: 0.0,
                delta_y: -100.0,
            });
        let after = rect(&title);
        assert!(after[1] < before[1], "page {tab}: {before:?} {after:?}");
        assert_eq!(after[0], before[0]);
    }
}

#[test]
fn the_grid_is_the_same_on_a_page_with_a_scroll_bar_and_on_one_without() {
    let window = real_window();
    let model = window.global::<Model>();
    let mut rows = 0;
    for tab in 0..PAGES {
        model.set_tab(tab);
        for row in all(&window, "Row") {
            // the 512 px of the page, from its left edge (the navigation and 24 px)
            let [left, _, width, _] = rect(&row);
            assert_eq!((left, width), (184.0, 512.0), "a row of page {tab}");
            rows += 1;
        }
    }
    assert!(rows > 10, "{rows} rows");
}

fn scroll_down(window: &MainWindow) {
    use slint::LogicalPosition;
    use slint::platform::WindowEvent;

    window
        .window()
        .dispatch_event(WindowEvent::PointerScrolled {
            position: LogicalPosition::new(450.0, 250.0),
            delta_x: 0.0,
            delta_y: -400.0,
        });
}

#[test]
fn the_controls_of_the_test_rows_share_one_slot_and_the_results_one_column() {
    // the right edge of the page content, the control slot and the result column in front of it
    const EDGE: f32 = 684.0;
    const SLOT: f32 = 160.0;
    const HALF: f32 = 76.0;
    const GAP: f32 = 8.0;
    const RESULT: f32 = 128.0;
    // the value of the level, in the column of the value of a slider
    const VALUE: f32 = 64.0;

    let window = real_window();
    window.global::<Model>().set_tab(test_tab::TAB);
    let mut paired = Vec::new();
    // the rows below the fold are found after scrolling to them
    for (scrolled, buttons_at_least) in [(false, 4), (true, 2)] {
        if scrolled {
            scroll_down(&window);
        }
        let buttons = all(&window, "TestButton");
        assert!(buttons.len() >= buttons_at_least, "scrolled: {scrolled}");
        for button in &buttons {
            let [left, _, width, _] = rect(button);
            if width == HALF {
                paired.push(left);
            } else {
                assert_eq!(width, SLOT, "a single button fills the slot");
                assert_eq!(left + width, EDGE, "a single button ends at the edge");
            }
        }
        // the level row, where it is found
        let bar = all(&window, "LevelBar").first().map(rect);
        for result in all(&window, "ResultText") {
            let [left, top, width, height] = rect(&result);
            let level = bar.is_some_and(|bar| top < bar[1] + bar[3] && bar[1] < top + height);
            let column = if level { VALUE } else { RESULT };
            assert_eq!(width, column, "a result column, at {top}");
            assert_eq!(left + width, EDGE - SLOT - GAP, "directly left of the slot");
        }
    }
    // Record and Play share the slot with a gap
    paired.sort_by(f32::total_cmp);
    paired.dedup();
    assert_eq!(paired, [EDGE - SLOT, EDGE - HALF]);
}

#[test]
fn the_level_bar_fills_the_space_between_the_label_and_the_result_column() {
    // the right edge of the page content, the slot of the control and the gap before it
    const EDGE: f32 = 684.0;
    const SLOT: f32 = 160.0;
    // between the label and the control of a row
    const ROW_GAP: f32 = 12.0;

    let window = real_window();
    window.global::<Model>().set_tab(test_tab::TAB);
    let bar = rect(&only(&window, "LevelBar"));
    let beside = |other: &[f32; 4]| other[1] < bar[1] + bar[3] && bar[1] < other[1] + other[3];
    let find = |type_name: &str| {
        all(&window, type_name)
            .iter()
            .map(rect)
            .find(beside)
            .unwrap_or_else(|| panic!("a {type_name} in the level row"))
    };
    let (help, result, switch) = (find("Help"), find("ResultText"), find("SwitchControl"));

    assert_eq!(
        bar[0],
        help[0] + help[2] + ROW_GAP,
        "starts after the label"
    );
    assert_eq!(
        bar[0] + bar[2],
        result[0],
        "ends where the result column starts"
    );
    // its segments fill the bar, to within the pitch of one of them
    let segments = find("Meter");
    assert!(
        segments[0] >= bar[0]
            && segments[0] + segments[2] <= bar[0] + bar[2]
            && bar[2] - segments[2] < 7.0,
        "{segments:?} in {bar:?}"
    );
    // the switch is the control of the row, at the right edge, in the slot of the others
    assert_eq!(switch[0] + switch[2], EDGE);
    assert!(switch[0] >= EDGE - SLOT);
    assert!(result[0] + result[2] < switch[0]);
}

#[test]
fn the_header_meter_draws_a_value_only_while_the_meter_runs() {
    let window = window();
    let model = window.global::<Model>();
    model.set_mic_db(-52.0);
    model.set_meters_active(true);
    let [left, top, width, height] = rect(&only(&window, "MeterRow"));
    // the texts in the meter: its tag, and its value
    let texts = |window: &MainWindow| {
        all(window, "Text")
            .iter()
            .map(rect)
            .filter(|t| {
                t[0] >= left
                    && t[0] + t[2] <= left + width
                    && t[1] >= top
                    && t[1] + t[3] <= top + height
            })
            .count()
    };
    assert_eq!(texts(&window), 2, "a tag and a value");
    model.set_meters_active(false);
    assert_eq!(texts(&window), 1, "a tag and no value");
}

#[test]
fn the_header_meter_reads_in_dbfs_like_the_test_page() {
    let window = window();
    let model = window.global::<Model>();
    model.set_mic_db(-52.0);
    let idle = only(&window, "MeterRow").accessible_value().unwrap();
    assert_eq!(idle, "", "no value while the meter does not run");
    model.set_meters_active(true);
    let header = only(&window, "MeterRow").accessible_value().unwrap();
    assert!(header.ends_with("dBFS"), "{header}");
    model.set_tab(test_tab::TAB);
    let page = only(&window, "LevelBar").accessible_value().unwrap();
    assert_eq!(header, page);
}
