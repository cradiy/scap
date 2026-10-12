use crate::capturer::engine::mac::shareable_content;
use anyhow::Result;
use core_foundation::{
    array::CFArray,
    base::{CFType, TCFType},
    dictionary::CFDictionary,
    number::CFNumber,
    string::CFString,
};
use core_graphics_helmer_fork::{
    display::{CGDirectDisplayID, CGDisplay, CGMainDisplayID},
    geometry::{CGPoint, CGRect, CGSize},
    window::{
        kCGWindowBounds, kCGWindowListOptionIncludingWindow, CGWindowID, CGWindowListCopyWindowInfo,
    },
};
use objc2::MainThreadMarker;
use objc2_app_kit::NSScreen;
use objc2_foundation::{NSNumber, NSString};

use super::{Display, Target};

fn get_display_name(display_id: CGDirectDisplayID) -> String {
    // AppKit screen names are only available on the main thread. Background callers
    // can enumerate capture targets without an AppKit application or event loop.
    if let Some(mtm) = MainThreadMarker::new() {
        let screen_number = NSString::from_str("NSScreenNumber");
        for screen in NSScreen::screens(mtm) {
            let description = screen.deviceDescription();
            let number = description.objectForKey(&screen_number);
            if number
                .as_deref()
                .and_then(|n| n.downcast_ref::<NSNumber>())
                .is_some_and(|n| n.unsignedIntValue() == display_id)
            {
                return screen.localizedName().to_string();
            }
        }
    }
    format!("Display {display_id}")
}

pub fn get_all_targets() -> Result<Vec<Target>> {
    let mut targets: Vec<Target> = Vec::new();

    let content = shareable_content()?;

    // Add displays to targets
    for display in unsafe { content.displays() } {
        let id: CGDirectDisplayID = unsafe { display.displayID() };
        let raw_handle = CGDisplay::new(id);
        let title = get_display_name(id);

        let target = Target::Display(super::Display {
            id,
            title,
            raw_handle,
        });

        targets.push(target);
    }

    // Add windows to targets
    for window in unsafe { content.windows() } {
        if let Some(title) = unsafe { window.title() } {
            let id = unsafe { window.windowID() };
            let title = title.to_string();
            let raw_handle: CGWindowID = id;

            let target = Target::Window(super::Window {
                id,
                title,
                raw_handle,
            });
            targets.push(target);
        }
    }

    Ok(targets)
}

pub fn get_main_display() -> Result<Display> {
    let id = unsafe { CGMainDisplayID() };
    let title = get_display_name(id);

    Ok(Display {
        id,
        title,
        raw_handle: CGDisplay::new(id),
    })
}

fn window_bounds(window_id: CGWindowID) -> Option<CGRect> {
    // CoreGraphics includes windows owned by other processes and works off the main thread.
    let info = unsafe { CGWindowListCopyWindowInfo(kCGWindowListOptionIncludingWindow, window_id) };
    if info.is_null() {
        return None;
    }
    let info: CFArray<CFDictionary<CFString, CFType>> =
        unsafe { CFArray::wrap_under_create_rule(info.cast()) };
    let window = info.get(0)?;
    let bounds = window
        .find(unsafe { CFString::wrap_under_get_rule(kCGWindowBounds.cast()) })?
        .downcast::<CFDictionary>()?;
    let bounds: CFDictionary<CFString, CFType> =
        unsafe { CFDictionary::wrap_under_get_rule(bounds.as_concrete_TypeRef()) };
    let number = |key| {
        bounds
            .find(CFString::new(key))?
            .downcast::<CFNumber>()?
            .to_f64()
    };
    Some(CGRect::new(
        &CGPoint::new(number("X")?, number("Y")?),
        &CGSize::new(number("Width")?, number("Height")?),
    ))
}

fn display_scale(display: CGDisplay) -> f64 {
    display
        .display_mode()
        .filter(|mode| mode.width() != 0)
        .map_or(1.0, |mode| mode.pixel_width() as f64 / mode.width() as f64)
}

fn overlap_area(a: CGRect, b: CGRect) -> f64 {
    let width =
        (a.origin.x + a.size.width).min(b.origin.x + b.size.width) - a.origin.x.max(b.origin.x);
    let height =
        (a.origin.y + a.size.height).min(b.origin.y + b.size.height) - a.origin.y.max(b.origin.y);
    width.max(0.0) * height.max(0.0)
}

pub fn get_scale_factor(target: &Target) -> f64 {
    match target {
        Target::Window(window) => {
            let display = window_bounds(window.raw_handle)
                .and_then(|bounds| {
                    CGDisplay::active_displays()
                        .ok()?
                        .into_iter()
                        .map(CGDisplay::new)
                        .map(|display| (display, overlap_area(bounds, display.bounds())))
                        .filter(|(_, area)| *area > 0.0)
                        .max_by(|(_, a), (_, b)| a.total_cmp(b))
                        .map(|(display, _)| display)
                })
                .unwrap_or_else(CGDisplay::main);
            display_scale(display)
        }
        Target::Display(display) => display_scale(display.raw_handle),
    }
}

pub fn get_target_dimensions(target: &Target) -> (u64, u64) {
    match target {
        Target::Window(window) => window_bounds(window.raw_handle).map_or((0, 0), |bounds| {
            (bounds.size.width as u64, bounds.size.height as u64)
        }),
        Target::Display(display) => display
            .raw_handle
            .display_mode()
            .map_or((0, 0), |mode| (mode.width(), mode.height())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_overlap_handles_negative_origins_and_disjoint_screens() {
        let window = CGRect::new(&CGPoint::new(-100.0, 20.0), &CGSize::new(300.0, 200.0));
        let left = CGRect::new(&CGPoint::new(-1920.0, 0.0), &CGSize::new(1920.0, 1080.0));
        let right = CGRect::new(&CGPoint::new(0.0, 0.0), &CGSize::new(1920.0, 1080.0));
        let far = CGRect::new(&CGPoint::new(1920.0, 0.0), &CGSize::new(1920.0, 1080.0));
        assert_eq!(overlap_area(window, left), 20_000.0);
        assert_eq!(overlap_area(window, right), 40_000.0);
        assert_eq!(overlap_area(window, far), 0.0);
    }

    #[test]
    fn unknown_window_has_no_bounds() {
        assert!(window_bounds(u32::MAX).is_none());
    }

    #[test]
    fn display_lookup_works_without_a_main_thread_event_loop() {
        std::thread::spawn(|| {
            assert!(MainThreadMarker::new().is_none());
            let display = get_main_display().unwrap();
            assert!(!display.title.is_empty());
            let target = Target::Display(display);
            assert!(get_scale_factor(&target) > 0.0);
        })
        .join()
        .unwrap();
    }
}
