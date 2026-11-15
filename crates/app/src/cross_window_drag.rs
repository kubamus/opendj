//! Bridge Slint's in-window track drag from the detached library to the decks.
//!
//! The row arms its payload on mouse-down, before winit can coalesce movement
//! events. A real drag must travel at least five logical pixels. Winit retains
//! the source window's mouse capture until release; native window geometry
//! translates the release into the main window's coordinates.
use crate::{AppWindow, LibraryWindow, TrackDrag};
use slint::winit_030::winit::event::{ElementState, MouseButton, WindowEvent};
use slint::winit_030::winit::keyboard::{Key, KeyCode, NamedKey, PhysicalKey};
use slint::winit_030::{EventResult, WinitWindowAccessor};
use slint::{ComponentHandle, DataTransfer};
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};

const DRAG_DISTANCE: f64 = 5.0;
type Point = (f64, f64);

struct Pending {
    data: DataTransfer,
    origin: Point,
    traveled: bool,
}

fn traveled(origin: Point, cursor: Point) -> bool {
    let dx = cursor.0 - origin.0;
    let dy = cursor.1 - origin.1;
    dx * dx + dy * dy >= DRAG_DISTANCE * DRAG_DISTANCE
}

/// The pointer is eligible only outside the source and inside the target.
fn target_point(
    source: Option<(f64, f64, f64, f64)>,
    target: Option<(f64, f64, f64, f64)>,
) -> Option<(f32, f32)> {
    let (x, y, width, height) = source?;
    if x >= 0.0 && y >= 0.0 && x < width && y < height {
        return None;
    }
    let (x, y, width, height) = target?;
    (x >= 0.0 && y >= 0.0 && x < width && y < height).then_some((x as f32, y as f32))
}

struct ClientFrame {
    origin: Point,
    size: Point,
    // Logical client pixels to the shared screen coordinate space. AppKit
    // screen points are already logical; Windows and X11 use physical pixels.
    scale: f64,
}

fn target_in_frames(source: ClientFrame, target: ClientFrame, cursor: Point) -> Option<(f32, f32)> {
    let x = (source.origin.0 + cursor.0 * source.scale - target.origin.0) / target.scale;
    let y = (source.origin.1 + cursor.1 * source.scale - target.origin.1) / target.scale;
    target_point(
        Some((cursor.0, cursor.1, source.size.0, source.size.1)),
        Some((x, y, target.size.0, target.size.1)),
    )
}

#[cfg(target_os = "macos")]
fn client_frame(window: &slint::Window) -> Option<ClientFrame> {
    use objc2::rc::Retained;
    use objc2_app_kit::NSView;
    use objc2_foundation::NSPoint;
    use slint::winit_030::winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
    window
        .with_winit_window(|w| {
            let RawWindowHandle::AppKit(handle) = w.window_handle().ok()?.as_raw() else {
                return None;
            };
            // SAFETY: winit's content view is live and accessed on the main thread.
            let view: Retained<NSView> =
                unsafe { Retained::retain(handle.ns_view.as_ptr().cast::<NSView>()) }?;
            let bounds = view.bounds();
            let top_left = NSPoint::new(
                bounds.origin.x,
                if view.isFlipped() {
                    bounds.origin.y
                } else {
                    bounds.origin.y + bounds.size.height
                },
            );
            let origin = view
                .window()?
                .convertPointToScreen(view.convertPoint_toView(top_left, None));
            // Both windows use AppKit screen points, regardless of their backing
            // scales. Negate screen Y to match Slint's downward client Y axis.
            Some(ClientFrame {
                origin: (origin.x, -origin.y),
                size: (bounds.size.width, bounds.size.height),
                scale: 1.0,
            })
        })
        .flatten()
}

#[cfg(not(target_os = "macos"))]
fn client_frame(window: &slint::Window) -> Option<ClientFrame> {
    window
        .with_winit_window(|w| {
            let origin = w.inner_position().ok()?;
            let scale = w.scale_factor();
            let size = w.inner_size().to_logical::<f64>(scale);
            Some(ClientFrame {
                origin: (origin.x as f64, origin.y as f64),
                size: (size.width, size.height),
                scale,
            })
        })
        .flatten()
}

fn cursor_target(
    source: &slint::Window,
    target: &slint::Window,
    cursor: Point,
) -> Option<(f32, f32)> {
    target_in_frames(client_frame(source)?, client_frame(target)?, cursor)
}

// AppKit may omit outside-client motion events for a captured drag. The
// mouse-up still carries its precise position, unlike winit's MouseInput.
#[cfg(target_os = "macos")]
fn release_cursor(source: &slint::Window) -> Option<Point> {
    use objc2::{rc::Retained, MainThreadMarker};
    use objc2_app_kit::{NSApplication, NSEventType, NSView};
    use slint::winit_030::winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
    let mtm = MainThreadMarker::new()?;
    let event = NSApplication::sharedApplication(mtm).currentEvent()?;
    if event.r#type() != NSEventType::LeftMouseUp {
        return None;
    }
    let event_window = event.window(mtm)?;
    let screen = event_window.convertPointToScreen(event.locationInWindow());
    source
        .with_winit_window(|w| {
            let RawWindowHandle::AppKit(handle) = w.window_handle().ok()?.as_raw() else {
                return None;
            };
            // SAFETY: this is winit's live content view, accessed on the main thread.
            let view: Retained<NSView> =
                unsafe { Retained::retain(handle.ns_view.as_ptr().cast::<NSView>()) }?;
            let point =
                view.convertPoint_fromView(view.window()?.convertPointFromScreen(screen), None);
            Some((
                point.x,
                if view.isFlipped() {
                    point.y
                } else {
                    view.bounds().size.height - point.y
                },
            ))
        })
        .flatten()
}
#[cfg(not(target_os = "macos"))]
fn release_cursor(_: &slint::Window) -> Option<Point> {
    None
}

pub fn install(app: &AppWindow, library: &LibraryWindow) {
    let pending = Rc::new(RefCell::new(None::<Pending>));
    // The event filter observes the press before Slint dispatches it to the
    // row. Keep its last native pointer position; querying the OS only after
    // that dispatch can see a later move if events arrived in a quick burst.
    let last_cursor = Rc::new(Cell::new(None::<Point>));
    let press_origin = Rc::new(Cell::new(None::<Point>));
    let candidate = pending.clone();
    let origin = press_origin.clone();
    let weak_app = app.as_weak();
    app.global::<TrackDrag>().on_cross_start(move |data| {
        if weak_app
            .upgrade()
            .is_some_and(|app| app.get_library_detached())
        {
            if let Some(origin) = origin.get() {
                *candidate.borrow_mut() = Some(Pending {
                    data,
                    origin,
                    traveled: false,
                });
            }
        }
    });
    let weak_app = app.as_weak();
    library
        .window()
        .on_winit_window_event(move |source, event| {
            let Some(app) = weak_app.upgrade() else {
                return EventResult::Propagate;
            };
            match event {
                WindowEvent::MouseInput {
                    state: ElementState::Pressed,
                    button: MouseButton::Left,
                    ..
                } => {
                    pending.borrow_mut().take();
                    app.global::<TrackDrag>().set_cross_over(false);
                    press_origin.set(last_cursor.get());
                }
                WindowEvent::CursorMoved {
                    position: cursor, ..
                } => {
                    let p = cursor.to_logical::<f64>(source.scale_factor() as f64);
                    let cursor = (p.x, p.y);
                    last_cursor.set(Some(cursor));
                    let moving = if let Some(pending) = pending.borrow_mut().as_mut() {
                        pending.traveled |= traveled(pending.origin, cursor);
                        pending.traveled
                    } else {
                        false
                    };
                    if moving {
                        let at = cursor_target(source, app.window(), cursor);
                        let drag = app.global::<TrackDrag>();
                        if let Some((x, y)) = at {
                            drag.set_cross_x(x);
                            drag.set_cross_y(y);
                        }
                        drag.set_cross_over(at.is_some());
                    }
                }
                WindowEvent::MouseInput {
                    state: ElementState::Released,
                    button: MouseButton::Left,
                    ..
                } => {
                    let cursor = release_cursor(source).or(last_cursor.get());
                    let at = cursor.and_then(|cursor| cursor_target(source, app.window(), cursor));
                    let candidate = pending.borrow_mut().take();
                    press_origin.set(None);
                    let drag = app.global::<TrackDrag>();
                    if let (Some((x, y)), Some(candidate)) = (at, candidate) {
                        // The release itself also counts: some fast drags deliver no
                        // intermediate move after the row armed its payload.
                        if candidate.traveled
                            || cursor.is_some_and(|p| traveled(candidate.origin, p))
                        {
                            drag.set_cross_x(x);
                            drag.set_cross_y(y);
                            drag.set_cross_data(candidate.data);
                            drag.set_cross_dropped(drag.get_cross_dropped().wrapping_add(1));
                        }
                    }
                    drag.set_cross_over(false);
                }
                // Esc cancels a cross-window drag even if the mouse is later
                // released over a deck.
                WindowEvent::KeyboardInput { event, .. }
                    if event.state == ElementState::Pressed
                        && (matches!(&event.logical_key, Key::Named(NamedKey::Escape))
                            || matches!(
                                event.physical_key,
                                PhysicalKey::Code(KeyCode::Escape)
                            )) =>
                {
                    pending.borrow_mut().take();
                    press_origin.set(None);
                    app.global::<TrackDrag>().set_cross_over(false);
                }
                // Key focus can change during an active OS mouse capture.
                WindowEvent::Focused(false) => app.global::<TrackDrag>().set_cross_over(false),
                WindowEvent::Destroyed => {
                    pending.borrow_mut().take();
                    press_origin.set(None);
                    app.global::<TrackDrag>().set_cross_over(false);
                }
                _ => {}
            }
            EventResult::Propagate
        });
}

#[cfg(test)]
mod tests {
    use super::{target_in_frames, target_point, traveled, ClientFrame};

    #[test]
    fn appkit_screen_points_are_independent_of_backing_scale() {
        // Retina source and 1x target: both frames come from AppKit screen
        // points, not winit's independently scaled physical screen origins.
        let source = ClientFrame {
            origin: (0.0, -900.0),
            size: (1400.0, 800.0),
            scale: 1.0,
        };
        let target = ClientFrame {
            origin: (1600.0, -800.0),
            size: (1280.0, 720.0),
            scale: 1.0,
        };
        assert_eq!(
            target_in_frames(source, target, (1700.0, 300.0)),
            Some((100.0, 200.0))
        );
    }

    #[test]
    fn physical_screen_coordinates_preserve_each_windows_scale() {
        let source = ClientFrame {
            origin: (0.0, 0.0),
            size: (1400.0, 800.0),
            scale: 2.0,
        };
        let target = ClientFrame {
            origin: (2800.0, 100.0),
            size: (1280.0, 720.0),
            scale: 1.0,
        };
        assert_eq!(
            target_in_frames(source, target, (1450.0, 200.0)),
            Some((100.0, 300.0))
        );
    }

    #[test]
    fn only_delivers_to_the_exposed_main_window() {
        assert_eq!(
            target_point(
                Some((20.0, 10.0, 100.0, 100.0)),
                Some((40.0, 10.0, 300.0, 300.0))
            ),
            None
        );
        assert_eq!(
            target_point(
                Some((-20.0, 10.0, 100.0, 100.0)),
                Some((40.0, 10.0, 300.0, 300.0))
            ),
            Some((40.0, 10.0))
        );
        assert_eq!(
            target_point(
                Some((-20.0, 10.0, 100.0, 100.0)),
                Some((-1.0, 10.0, 300.0, 300.0))
            ),
            None
        );
        assert_eq!(target_point(None, Some((40.0, 10.0, 300.0, 300.0))), None);
        assert_eq!(
            target_point(
                Some((-1.0, 2.0, 100.0, 100.0)),
                Some((300.0, 10.0, 300.0, 300.0))
            ),
            None
        );
    }

    #[test]
    fn threshold_distinguishes_clicks_and_fast_drags() {
        assert!(!traveled((10.0, 20.0), (10.0, 20.0)));
        assert!(!traveled((10.0, 20.0), (14.9, 20.0)));
        assert!(traveled((10.0, 20.0), (13.0, 24.0)));
        assert!(traveled((10.0, 20.0), (-20.0, 20.0)));
    }
}
