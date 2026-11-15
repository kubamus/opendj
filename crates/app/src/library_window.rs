//! The detached library retains the app's backend and shared models.
use crate::{AppWindow, LibraryWindow};
use slint::{ComponentHandle, Timer, TimerMode};
use std::{cell::RefCell, rc::Rc, time::Duration};

thread_local! {
    static CREATING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(target_os = "macos")]
pub fn creating() -> bool {
    CREATING.with(|flag| flag.get())
}

include!(concat!(env!("OUT_DIR"), "/library_bridge.rs"));

struct DetachedLibrary {
    window: LibraryWindow,
    timer: Timer,
    synchronize: Rc<dyn Fn()>,
}

/// Returns the cleanup to include in the main window's shared close handler.
pub fn install(app: &AppWindow) -> impl Fn() {
    let state = Rc::new(RefCell::new(None::<DetachedLibrary>));
    let closing_state = state.clone();
    let weak = app.as_weak();
    app.on_detach_library(move || {
        let Some(app) = weak.upgrade() else { return };
        if let Some(library) = state.borrow().as_ref() {
            if app.get_library_detached() {
                (library.synchronize)();
                let _ = library.window.hide();
                library.timer.stop();
                app.set_library_detached(false);
                return;
            }
        }
        if state.borrow().is_none() {
            CREATING.with(|flag| flag.set(true));
            let result = LibraryWindow::new();
            CREATING.with(|flag| flag.set(false));
            let window = match result {
                Ok(window) => window,
                Err(error) => {
                    app.set_alert(format!("Could not open the library window: {error}").into());
                    return;
                }
            };
            let synchronize = bridge(&app, &window);
            crate::cross_window_drag::install(&app, &window);
            let weak_window = window.as_weak();
            let refresh = synchronize.clone();
            app.on_show_detached_tracks_at(move |row, offset, y| {
                // Restore after the backend has finished changing lists and released its borrow.
                let refresh = refresh.clone();
                let weak_window = weak_window.clone();
                Timer::single_shot(Duration::ZERO, move || {
                    refresh();
                    if let Some(window) = weak_window.upgrade() {
                        window.invoke_show_tracks_at(row, offset, y);
                    }
                });
            });
            let weak_window = window.as_weak();
            app.on_find_detached(move || {
                if let Some(window) = weak_window.upgrade() {
                    use slint::winit_030::WinitWindowAccessor;
                    window.window().with_winit_window(|w| w.focus_window());
                    window.invoke_find();
                }
            });
            let weak_app = app.as_weak();
            let weak_window = window.as_weak();
            let refresh = synchronize.clone();
            window.on_pin_library(move || {
                refresh();
                if let Some(window) = weak_window.upgrade() {
                    let _ = window.hide();
                }
                if let Some(app) = weak_app.upgrade() {
                    app.set_library_detached(false);
                }
            });
            let weak_app = app.as_weak();
            let refresh = synchronize.clone();
            window.window().on_close_requested(move || {
                refresh();
                if let Some(app) = weak_app.upgrade() {
                    app.set_library_detached(false);
                }
                slint::CloseRequestResponse::HideWindow
            });
            let timer = Timer::default();
            let weak_app = app.as_weak();
            let weak_window = window.as_weak();
            let refresh = synchronize.clone();
            timer.start(TimerMode::Repeated, Duration::from_millis(16), move || {
                let (Some(app), Some(window)) = (weak_app.upgrade(), weak_window.upgrade()) else {
                    return;
                };
                if app.get_library_detached() {
                    refresh();
                    app.set_detached_scroll_y(window.get_track_scroll_y());
                }
            });
            *state.borrow_mut() = Some(DetachedLibrary {
                window,
                timer,
                synchronize,
            });
        }
        if let Some(library) = state.borrow().as_ref() {
            (library.synchronize)();
            library.timer.restart();
            match library.window.show() {
                Ok(()) => {
                    app.set_library_detached(true);
                }
                Err(error) => {
                    app.set_alert(format!("Could not show the library window: {error}").into())
                }
            }
        }
    });
    move || {
        if let Some(library) = closing_state.borrow().as_ref() {
            let _ = library.window.hide();
            library.timer.stop();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{StoreUi, Theme};
    use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};
    use slint::platform::{PointerEventButton, WindowAdapter, WindowEvent};
    use slint::{Model, VecModel};

    struct TestPlatform(Rc<RefCell<Vec<Rc<MinimalSoftwareWindow>>>>);
    impl slint::platform::Platform for TestPlatform {
        fn create_window_adapter(
            &self,
        ) -> Result<Rc<dyn slint::platform::WindowAdapter>, slint::PlatformError> {
            let window = MinimalSoftwareWindow::new(RepaintBufferType::ReusedBuffer);
            self.0.borrow_mut().push(window.clone());
            Ok(window)
        }
    }

    #[test]
    fn detached_library_shares_state_actions_and_can_be_repinned() {
        let windows = Rc::new(RefCell::new(Vec::new()));
        slint::platform::set_platform(Box::new(TestPlatform(windows.clone()))).unwrap();
        let app = AppWindow::new().unwrap();
        let view = LibraryWindow::new().unwrap();
        app.set_folder_name("Collection".into());
        let synchronize = bridge(&app, &view);
        assert_eq!(view.get_folder_name(), "Collection");

        app.global::<Theme>().set_light(true);
        synchronize();
        assert!(view.global::<Theme>().get_light());
        view.set_current_track(3);
        synchronize();
        assert_eq!(app.get_current_track(), 3);

        // UI edits must reach the backend before its action is invoked.
        let weak = app.as_weak();
        app.on_select_source(move |source| {
            let app = weak.upgrade().unwrap();
            assert_eq!(app.global::<StoreUi>().get_source(), source);
            app.set_folder_name("Files".into());
        });
        view.global::<StoreUi>().set_source(1);
        view.invoke_select_source(1);
        assert_eq!(view.get_folder_name(), "Files");

        crate::install_window_close(&app);
        app.invoke_detach_library();
        assert!(app.get_library_detached());
        app.invoke_detach_library();
        assert!(!app.get_library_detached());
        app.invoke_detach_library();
        assert!(app.get_library_detached());
        app.invoke_detach_library();

        app.window()
            .set_size(slint::LogicalSize::new(1680.0, 1050.0));
        slint::platform::update_timers_and_animations();
        app.global::<crate::LibraryEdit>().set_window_height(1050.0);
        app.global::<crate::LibraryEdit>().set_window_width(1680.0);
        for layout in [0, 2, 3, 4] {
            app.set_layout(layout);
            app.set_library_expanded(false);
            slint::platform::update_timers_and_animations();
            let docked = app.get_waveform_height();
            app.set_library_detached(true);
            slint::platform::update_timers_and_animations();
            assert!(app.get_performance());
            assert_eq!(app.global::<crate::WaveHeight>().get_layout(), layout + 5);
            assert!(
                app.get_waveform_height() > docked,
                "layout {layout} should give waveforms more room"
            );
            app.set_library_expanded(true);
            assert!(
                app.get_waveform_height() > 1.0,
                "expanded browser must not fold detached decks"
            );
            app.set_library_detached(false);
            slint::platform::update_timers_and_animations();
            assert!(!app.get_performance());
            assert_eq!(app.global::<crate::WaveHeight>().get_layout(), layout);
            app.set_library_expanded(false);
            slint::platform::update_timers_and_animations();
            assert_eq!(app.get_waveform_height(), docked);
        }
        app.set_layout(1);
        app.set_library_detached(true);
        assert!(!app.get_performance(), "Player keeps its own presentation");

        // Exercise the real grip, rather than only setting the height model:
        // a default-sized library must yield room when the waveforms grow.
        app.set_library_detached(false);
        app.set_layout(0);
        let levels = Rc::new(VecModel::from(vec![3; 10]));
        app.global::<crate::WaveHeight>()
            .set_levels(levels.clone().into());
        let changed_levels = levels.clone();
        app.global::<crate::WaveHeight>()
            .on_set_level(move |profile, level| {
                changed_levels.set_row_data(profile as usize, level);
            });
        for layout in [0, 2, 3, 4] {
            app.set_layout(layout);
            levels.set_row_data(layout as usize, 3);
            slint::platform::update_timers_and_animations();
            let before = app.get_waveform_height();
            let rows = if layout == 2 {
                4.0
            } else if layout == 3 {
                1.0
            } else {
                2.0
            };
            let bottom = 53.0 + before * rows + rows + 1.0 + if layout == 3 { 24.0 } else { 0.0 };
            let start = slint::LogicalPosition::new(600.0, bottom);
            let end = slint::LogicalPosition::new(600.0, bottom + 40.0);
            app.window().dispatch_event(WindowEvent::PointerPressed {
                position: start,
                button: PointerEventButton::Left,
            });
            app.window()
                .dispatch_event(WindowEvent::PointerMoved { position: end });
            app.window().dispatch_event(WindowEvent::PointerReleased {
                position: end,
                button: PointerEventButton::Left,
            });
            assert!(
                levels.row_data(layout as usize).unwrap() > 3,
                "waveform grip must grow in layout {layout}"
            );
            assert!(
                app.get_waveform_height() > before,
                "waveform preview must persist in layout {layout}"
            );
        }

        // Prep's default-sized tool pane is shorter than a long checklist.
        // Scroll to its footer and activate the editing control there.
        app.set_layout(3);
        app.show().unwrap();
        app.invoke_detach_library();
        app.global::<crate::ChecklistUi>().set_rows(
            Rc::new(VecModel::from(
                (0..8)
                    .map(|n| crate::CheckRow {
                        name: format!("Check {n}").into(),
                        ..Default::default()
                    })
                    .collect::<Vec<_>>(),
            ))
            .into(),
        );
        let edited = Rc::new(std::cell::Cell::new(false));
        let edited_callback = edited.clone();
        app.global::<crate::ChecklistUi>()
            .on_edit_lines(move || edited_callback.set(true));
        slint::platform::update_timers_and_animations();
        let footer = slint::LogicalPosition::new(600.0, 1050.0 - 36.0 - 22.0);
        app.window().dispatch_event(WindowEvent::PointerScrolled {
            position: footer,
            delta_x: 0.0,
            delta_y: -10000.0,
        });
        app.window().dispatch_event(WindowEvent::PointerPressed {
            position: footer,
            button: PointerEventButton::Left,
        });
        app.window().dispatch_event(WindowEvent::PointerReleased {
            position: footer,
            button: PointerEventButton::Left,
        });
        assert!(
            edited.get(),
            "the checklist footer must be reachable by scrolling"
        );

        // The one main-window handler must close both auxiliary windows.
        let karaoke = crate::KaraokeWindow::new().unwrap();
        karaoke.show().unwrap();
        let closed = Rc::new(std::cell::Cell::new(false));
        let closed_callback = closed.clone();
        app.global::<crate::LyricsUi>().on_set_karaoke(move |on| {
            assert!(!on);
            karaoke.hide().unwrap();
            closed_callback.set(true);
        });
        assert_eq!(
            windows
                .borrow()
                .iter()
                .filter(|w| w.window().is_visible())
                .count(),
            3
        );
        app.window().dispatch_event(WindowEvent::CloseRequested);
        assert!(closed.get(), "closing the app must retain karaoke cleanup");
        assert!(
            windows.borrow().iter().all(|w| !w.window().is_visible()),
            "closing the app must hide every auxiliary window"
        );
    }
}
