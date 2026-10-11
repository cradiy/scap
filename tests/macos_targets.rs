//! Run with `SCAP_MACOS_SMOKE=1 cargo test --test macos_targets` in a desktop session.

#[cfg(target_os = "macos")]
fn main() {
    use gpui_scap::{capturer::Options, engine::mac, Target, Window};
    use objc2::{MainThreadMarker, MainThreadOnly};
    use objc2_app_kit::{
        NSApplication, NSApplicationActivationPolicy, NSBackingStoreType, NSWindow,
        NSWindowAnimationBehavior, NSWindowStyleMask,
    };
    use objc2_foundation::{NSDate, NSPoint, NSRect, NSRunLoop, NSSize};

    let args: Vec<String> = std::env::args().collect();
    if args.get(1).is_some_and(|arg| arg == "--query-window") {
        let id = args[2].parse().unwrap();
        let expected_width: f64 = args[3].parse().unwrap();
        let expected_height: f64 = args[4].parse().unwrap();
        let expected_scale: f64 = args[5].parse().unwrap();
        // This process has no NSApplication and does not own the capture window.
        std::thread::spawn(move || {
            let options = Options {
                target: Some(Target::Window(Window {
                    id,
                    raw_handle: id,
                    title: "Foreign capture window".into(),
                })),
                ..Default::default()
            };
            let area = mac::get_crop_area(&options);
            assert_eq!(area.size.width, expected_width);
            assert_eq!(area.size.height, expected_height);
            let even = |value: f64| (value as u32) & !1;
            assert_eq!(
                mac::get_output_frame_size(&options),
                [
                    even(expected_width * expected_scale),
                    even(expected_height * expected_scale),
                ]
            );
        })
        .join()
        .unwrap();
        return;
    }
    if std::env::var_os("SCAP_MACOS_SMOKE").is_none() {
        println!("macos_targets skipped; set SCAP_MACOS_SMOKE=1 to open a test window");
        return;
    }
    let mtm = MainThreadMarker::new().unwrap();
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    app.finishLaunching();
    let display_size = mac::get_output_frame_size(&Options::default());
    assert!(display_size[0] > 0 && display_size[1] > 0);
    let window = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            NSWindow::alloc(mtm),
            NSRect::new(NSPoint::new(100.0, 100.0), NSSize::new(320.0, 240.0)),
            NSWindowStyleMask::Borderless,
            NSBackingStoreType::Buffered,
            false,
        )
    };
    unsafe { window.setReleasedWhenClosed(false) };
    window.setAnimationBehavior(NSWindowAnimationBehavior::None);
    window.setHasShadow(false);
    window.orderFrontRegardless();
    window.displayIfNeeded();
    NSRunLoop::currentRunLoop().runUntilDate(&NSDate::dateWithTimeIntervalSinceNow(0.5));
    let frame = window.frame();
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .arg("--query-window")
        .arg(window.windowNumber().to_string())
        .arg(frame.size.width.to_string())
        .arg(frame.size.height.to_string())
        .arg(window.backingScaleFactor().to_string())
        .status()
        .unwrap();
    window.close();
    assert!(status.success(), "foreign window query failed");
    println!("macos_targets passed: foreign window bounds and scale from a background thread");
}

#[cfg(not(target_os = "macos"))]
fn main() {}
