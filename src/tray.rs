//! The only native UI bridge: an NSStatusItem with an event-driven Rust action.
use crate::ui::{self, Panel};
use async_channel::Sender;
use gpui_rhai::gpui::{
    App, AppContext, Bounds, Window, WindowBounds, WindowHandle, WindowKind, WindowOptions, point,
    px,
};
use mi_clip::file_feedback::{Event as FileEvent, FeedbackGroup as Feedback, Icon};
use objc2::rc::Retained;
use objc2::{AnyThread, DefinedClass, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSImage, NSScreen, NSStatusBar,
    NSStatusBarButton, NSStatusItem, NSView, NSWindow, NSWorkspace,
};
use objc2_foundation::{MainThreadMarker, NSData, NSObject, NSObjectProtocol, NSSize, ns_string};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use std::time::{Duration, Instant};

#[derive(Clone, Copy)]
pub(crate) struct Anchor {
    x: f32,
    y: f32,
}
fn native_popup(window: &Window) -> anyhow::Result<Retained<NSWindow>> {
    let raw = HasWindowHandle::window_handle(window)
        .map_err(|error| anyhow::anyhow!("Popup handle unavailable: {error}"))?;
    let RawWindowHandle::AppKit(handle) = raw.as_raw() else {
        anyhow::bail!("Expected an AppKit popup");
    };
    // SAFETY: GPUI provides a live NSView for the lifetime of the window borrow.
    let view = unsafe { handle.ns_view.cast::<NSView>().as_ref() };
    view.window()
        .ok_or_else(|| anyhow::anyhow!("Popup has no AppKit window"))
}

fn hide_popup(panel: &mut Panel, window: &mut Window) -> anyhow::Result<()> {
    // Keep GPUI's AccessKit adapter alive. Dropping it on close changes the
    // responder's ObjC class while AppKit's Touch Bar still observes it.
    panel.stop_monitoring();
    native_popup(window)?.orderOut(None);
    Ok(())
}

pub(crate) enum TrayEvent {
    Toggle(Anchor),
    Close,
    Status(&'static str, String),
    File(FileEvent),
    SessionFile(std::net::SocketAddr, FileEvent),
    AnimationTick,
    CheckUpdate,
    RefreshDevices,
    Capture,
    CaptureSettings,
    Discovering,
    Devices(Result<Vec<crate::devices::Device>, ()>),
    SessionStatus(std::net::SocketAddr, &'static str, String),
    Update(crate::update_ui::Notice),
}
struct ActionState {
    events: Sender<TrayEvent>,
}
define_class!(
    #[unsafe(super = NSObject)]
    #[thread_kind = MainThreadOnly]
    #[ivars = ActionState]
    struct TrayAction;
    unsafe impl NSObjectProtocol for TrayAction {}
    impl TrayAction {
        #[unsafe(method(togglePanel:))]
        fn toggle_panel(&self, sender: &NSStatusBarButton) {
            if let Some(window) = sender.window() {
                let rect = window.frame();
                let screens = NSScreen::screens(self.mtm());
                let main_height = screens.firstObject().map(|s| s.frame().size.height).unwrap_or(0.0);
                let screen_left = window.screen().map(|s| s.frame().origin.x).unwrap_or(0.0);
                let anchor = Anchor {
                    x: ((rect.origin.x + rect.size.width - f64::from(ui::PANEL_WIDTH)).max(screen_left + 4.0)) as f32,
                    y: (main_height - rect.origin.y + 4.0) as f32,
                };
                // A bounded channel coalesces repeated clicks without timers or busy polling.
                let _ = self.ivars().events.try_send(TrayEvent::Toggle(anchor));
            }
        }
    }
);
impl TrayAction {
    fn new(mtm: MainThreadMarker, events: Sender<TrayEvent>) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(ActionState { events });
        // SAFETY: NSObject's initializer is valid for this subclass.
        unsafe { msg_send![super(this), init] }
    }
}
struct StatusItem {
    item: Retained<NSStatusItem>,
    _action: Retained<TrayAction>,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

struct FileIcons {
    // Generated only when a file event first occurs. Frames remain vector PDFs.
    frames: Vec<(Icon, Vec<Retained<NSImage>>)>,
}
impl FileIcons {
    fn new() -> Self {
        Self { frames: Vec::new() }
    }
    fn image(&mut self, icon: Icon, frame: usize) -> anyhow::Result<&NSImage> {
        if !self.frames.iter().any(|(kind, _)| *kind == icon) {
            let count = if matches!(icon, Icon::Saved | Icon::Receiving) {
                12
            } else {
                1
            };
            let images = (0..count)
                .map(|frame| {
                    let pdf = mi_clip::file_feedback::icon_pdf(icon, frame);
                    let image = NSImage::initWithData(NSImage::alloc(), &NSData::with_bytes(&pdf))
                        .ok_or_else(|| anyhow::anyhow!("Invalid file feedback icon"))?;
                    image.setSize(NSSize::new(18., 18.));
                    image.setTemplate(false);
                    Ok(image)
                })
                .collect::<anyhow::Result<Vec<_>>>()?;
            self.frames.push((icon, images));
        }
        let (_, images) = self.frames.iter().find(|(kind, _)| *kind == icon).unwrap();
        Ok(&images[frame % images.len()])
    }
}

fn schedule_frame(cx: &mut App, events: Sender<TrayEvent>, reduced_motion: bool) {
    cx.spawn(async move |cx| {
        cx.background_executor()
            .timer(if reduced_motion {
                Duration::from_secs(1)
            } else {
                Duration::from_millis(83)
            })
            .await;
        let _ = events.send(TrayEvent::AnimationTick).await;
    })
    .detach();
}

fn visible_detail(feedback: &Feedback, status_detail: &str) -> String {
    if status_detail.is_empty() {
        feedback.detail().unwrap_or_default()
    } else {
        status_detail.to_owned()
    }
}

fn render_feedback(
    feedback: &Feedback,
    icons: &mut FileIcons,
    button: &NSStatusBarButton,
    idle: &NSImage,
    now: Duration,
    reduced_motion: bool,
    status_detail: &str,
) -> anyhow::Result<()> {
    let (icon, mut frame, mut animated) = feedback.appearance(now);
    if reduced_motion {
        frame = 0;
        animated = false;
    }
    if icon == Icon::Idle {
        button.setImage(Some(idle));
    } else {
        button.setImage(Some(icons.image(icon, frame)?));
    }
    let alpha = if animated && matches!(icon, Icon::Connecting | Icon::Failed) {
        0.75 + 0.25 * (frame as f64 * std::f64::consts::TAU / 12.).cos()
    } else {
        1.
    };
    button.setAlphaValue(alpha);
    let detail = visible_detail(feedback, status_detail);
    if !detail.is_empty() {
        button.setToolTip(Some(&objc2_foundation::NSString::from_str(&format!(
            "Mi Clip · {detail}"
        ))));
    }
    Ok(())
}
impl Drop for StatusItem {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        NSStatusBar::systemStatusBar().removeStatusItem(&self.item);
    }
}

pub fn install(
    cx: &mut App,
    smoke: Option<std::rc::Rc<std::cell::Cell<bool>>>,
    config: Option<std::path::PathBuf>,
    runtime: crate::Runtime,
) -> anyhow::Result<()> {
    let mtm = MainThreadMarker::new().ok_or_else(|| anyhow::anyhow!("main thread required"))?;
    let app = NSApplication::sharedApplication(mtm);
    let reduced_motion = NSWorkspace::sharedWorkspace().accessibilityDisplayShouldReduceMotion();
    if !app.setActivationPolicy(NSApplicationActivationPolicy::Accessory) {
        anyhow::bail!("accessory activation policy rejected");
    }
    let (sender, receiver) = async_channel::bounded(32);
    let testing = smoke.is_some();
    let action = TrayAction::new(mtm, sender.clone());
    let stop = runtime.stop.clone();
    if let Some(config) = config {
        crate::worker::start(config, sender.clone(), stop.clone());
    }
    let discovery_request = if testing {
        None
    } else {
        Some(crate::devices::start(sender.clone(), stop.clone()))
    };
    let update_request = Some(if smoke.is_none() {
        crate::update_ui::start(sender.clone(), stop.clone())
    } else {
        crate::update_ui::start_smoke(sender.clone(), stop.clone())
    });
    let item = NSStatusBar::systemStatusBar().statusItemWithLength(28.0);
    let button = item
        .button(mtm)
        .ok_or_else(|| anyhow::anyhow!("status button unavailable"))?;
    let image = NSImage::initWithData(
        NSImage::alloc(),
        &NSData::with_bytes(include_bytes!("../assets/status-template.pdf")),
    )
    .ok_or_else(|| anyhow::anyhow!("invalid embedded icon"))?;
    image.setSize(NSSize::new(18.0, 18.0));
    image.setTemplate(true);
    button.setImage(Some(&image));
    button.setToolTip(Some(ns_string!("Mi Clip · 尚未连接")));
    // SAFETY: The target is retained for the entire event loop. Selector signature
    // matches the action method above; Cocoa supplies the status button as sender.
    unsafe {
        button.setTarget(Some(&action));
        button.setAction(Some(sel!(togglePanel:)));
    }
    mi_clip::updater::mark_ready()?;
    if let Some(passed) = smoke {
        let test_button = button.clone();
        let test_events = sender.clone();
        let quit_expected = std::rc::Rc::new(std::cell::Cell::new(false));
        let quitting = quit_expected.clone();
        cx.on_app_quit(move |_| {
            if !quitting.get() {
                eprintln!("UI smoke test ended before the Quit button was clicked");
                std::process::exit(1);
            }
            if quitting.get() {
                passed.set(true);
                println!("PASS: native file feedback, update click, popup hide and reuse via toggle, Escape and outside click, and Quit");
            }
            std::future::ready(())
        }).detach();
        cx.spawn(async move |cx| {
use std::time::Duration;
            let result = async {
                cx.background_executor()
                    .timer(Duration::from_millis(250))
                    .await;
                // Synthetic lifecycle, no account, network, file write or OS banner.
                cx.update(|_| -> anyhow::Result<()> {
                    let mut frames = FileIcons::new();
                    for icon in [Icon::Connecting, Icon::Receiving, Icon::Saved, Icon::Failed] {
                        for frame in 0..12 {
                            anyhow::ensure!(
                                frames.image(icon, frame)?.isValid(),
                                "Invalid native vector frame"
                            );
                        }
                    }
                    Ok(())
                })?;
                for event in [
                    FileEvent::Requested,
                    FileEvent::Failed(mi_clip::file_feedback::Failure::Unavailable),
                    FileEvent::Requested,
                    FileEvent::Progress {
                        received: 4,
                        total: 10,
                    },
                    FileEvent::Progress {
                        received: 10,
                        total: 10,
                    },
                    FileEvent::Saved { files: 1 },
                ] {
                    test_events.send(TrayEvent::File(event)).await?;
                    cx.background_executor()
                        .timer(Duration::from_millis(250))
                        .await;
                    cx.update(|_| -> anyhow::Result<()> {
                        anyhow::ensure!(
                            !test_button.image().is_none_or(|image| image.isTemplate()),
                            "File event did not change native icon"
                        );
                        if matches!(event, FileEvent::Saved { .. }) {
                            anyhow::ensure!(
                                test_button
                                    .toolTip()
                                    .is_some_and(|text| text.to_string().contains("保存")),
                                "Completion tooltip missing"
                            );
                        }
                        Ok(())
                    })?;
                    if matches!(event, FileEvent::Failed(_)) {
                        test_events
                            .send(TrayEvent::Status(
                                "已连接",
                                "此内容暂不支持 · 当前仅同步短文本".into(),
                            ))
                            .await?;
                        cx.background_executor()
                            .timer(Duration::from_millis(250))
                            .await;
                        cx.update(|_| -> anyhow::Result<()> {
                            anyhow::ensure!(
                                test_button
                                    .toolTip()
                                    .is_some_and(|text| text.to_string().contains("仅同步短文本")),
                                "Old file feedback hid clipboard format hint"
                            );
                            Ok(())
                        })?;
                    }
                }
                // Populate enough synthetic peers to exercise the scroll area.
                test_events
                    .send(TrayEvent::Devices(Ok((0..8)
                        .map(|i| crate::devices::Device {
                            name: format!("测试设备 {i}"),
                            host: format!("fixture-{i}.local."),
                            port: 1234,
                            device_type: if i == 0 { 1 } else { 2 },
                            addresses: vec![format!("192.0.2.{}:1234", i + 1).parse().unwrap()],
                        })
                        .collect())))
                    .await?;
                cx.update(|cx| -> anyhow::Result<()> {
                    anyhow::ensure!(cx.windows().is_empty(), "Unexpected startup window");
                    // SAFETY: Retained target and button, main thread.
                    unsafe {
                        test_button.performClick(None);
                    }
                    Ok(())
                })?;
                for step in 0..4 {
                    cx.background_executor()
                        .timer(Duration::from_millis(700))
                        .await;
                    cx.update(|cx| -> anyhow::Result<()> {
                        anyhow::ensure!(
                            test_button.image().is_some_and(|image| image.isTemplate()),
                            "Opening the panel did not acknowledge the badge"
                        );
                        anyhow::ensure!(
                            test_button.alphaValue() == 1.,
                            "Animation did not restore opacity"
                        );
                        let handle = cx
                            .windows()
                            .first()
                            .copied()
                            .and_then(|w| w.downcast::<Panel>())
                            .ok_or_else(|| anyhow::anyhow!("Popup missing before refresh"))?;
                        handle.update(cx, |panel, window, cx| {
                            panel.refresh(
                                "已连接",
                                if step == 0 { "" } else { "测试状态更新" },
                                window,
                                cx,
                            )?;
                            if step == 0 {
                                panel.update_label(&ui::current_version_label(), cx);
                            }
                            Ok::<(), anyhow::Error>(())
                        })??;
                        Ok(())
                    })?;
                    // Let the new Rhai tree commit its focus path and hitboxes.
                    cx.background_executor()
                        .timer(Duration::from_millis(250))
                        .await;
                    cx.update(|cx| -> anyhow::Result<()> {
                        use gpui_rhai::gpui::Keystroke;
                        anyhow::ensure!(cx.windows().len() == 1, "Click must open one popup");
                        let handle = cx.windows()[0]
                            .downcast::<Panel>()
                            .ok_or_else(|| anyhow::anyhow!("Wrong root"))?;
                        handle.update(cx, |panel, window, cx| -> anyhow::Result<()> {
                            panel.validate(cx)?;
                            anyhow::ensure!(
                                panel.device_count() == 8,
                                "Device discovery did not reach the popup"
                            );
                            anyhow::ensure!(
                                window.viewport_size().height >= px(320.0),
                                "Device list was clipped by the popup"
                            );
                            let _ = window;
                            Ok(())
                        })??;
                        cx.update_window(handle.into(), |_, window, cx| -> anyhow::Result<()> {
                            // Commit the refreshed layout before injecting native events,
                            // including when macOS throttles background drawing.
                            window.draw(cx).clear(cx);
                            if step == 1 {
                                window.dispatch_keystroke(Keystroke::parse("escape")?, cx);
                            } else if step == 0 || step == 3 {
                                // Route through AppKit as a real panel click would.
                                let mtm = MainThreadMarker::new().unwrap();
                                let app = NSApplication::sharedApplication(mtm);
                                let native = app.windows().iter()
                                    .find(|w| w.isVisible() && (w.frame().size.width - 280.0).abs() < 1.0)
                                    .ok_or_else(|| anyhow::anyhow!("Native popup missing"))?;
                                let (x, y) = if step == 0 { (60.0, 68.0) } else { (244.0, 92.0) };
                                for kind in [objc2_app_kit::NSEventType::LeftMouseDown, objc2_app_kit::NSEventType::LeftMouseUp] {
                                    let event = objc2_app_kit::NSEvent::mouseEventWithType_location_modifierFlags_timestamp_windowNumber_context_eventNumber_clickCount_pressure(
                                        kind, objc2_foundation::NSPoint::new(x, f64::from(window.viewport_size().height) - y),
                                        objc2_app_kit::NSEventModifierFlags::empty(), (step + 1) as f64,
                                        native.windowNumber(), None, (step + 1) as isize, 1, 1.0,
                                    ).ok_or_else(|| anyhow::anyhow!("Mouse event allocation failed"))?;
                                    app.postEvent_atStart(&event, false);
                                }
                            }
                            Ok(())
                        })??;

                        Ok(())
                    })?;
                    if step == 0 {
                        cx.background_executor()
                            .timer(Duration::from_millis(250))
                            .await;
                        cx.update(|cx| -> anyhow::Result<()> {
                            let handle = cx.windows()[0]
                                .downcast::<Panel>()
                                .ok_or_else(|| anyhow::anyhow!("Update popup disappeared"))?;
                            handle.update(cx, |panel, _, _| -> anyhow::Result<()> {
                                anyhow::ensure!(
                                    panel.update_label == "检查中…",
                                    "Fast result hid the checking state before it could be seen"
                                );
                                Ok(())
                            })??;
                            Ok(())
                        })?;
                        cx.background_executor()
                            .timer(Duration::from_millis(1500))
                            .await;
                        cx.update(|cx| -> anyhow::Result<()> {
                            let handle = cx.windows()[0]
                                .downcast::<Panel>()
                                .ok_or_else(|| anyhow::anyhow!("Update popup disappeared"))?;
                            handle.update(cx, |panel, _, _| -> anyhow::Result<()> {
                                anyhow::ensure!(
                                    panel.update_label == ui::current_version_label(),
                                    "Fast update check did not finish"
                                );
                                Ok(())
                            })??;
                            unsafe {
                                test_button.performClick(None);
                            }
                            Ok(())
                        })?;
                    }
                    if step == 2 {
                        // Exercise the event sent by the native mouse monitor.
                        // macOS may deny activation to an automated background
                        // launch, so do not infer an outside click from keyWindow.
                        test_events.try_send(TrayEvent::Close)?;
                    }
                    if step == 3 {
                        // The click must itself request application quit. A timed
                        // cleanup is a failure and cannot mark this test passed.
                        cx.background_executor()
                            .timer(Duration::from_millis(400))
                            .await;
                        anyhow::bail!("Visible Quit button did not quit the application");
                    }
                    cx.background_executor()
                        .timer(Duration::from_millis(300))
                        .await;
                    cx.update(|cx| -> anyhow::Result<()> {
                        anyhow::ensure!(
                            cx.windows().len() == 1,
                            "Close path {step} destroyed the reusable popup"
                        );
                        let handle = cx.windows()[0]
                            .downcast::<Panel>()
                            .ok_or_else(|| anyhow::anyhow!("Wrong root"))?;
                        handle.update(cx, |panel, window, _| -> anyhow::Result<()> {
                            anyhow::ensure!(
                                !panel.is_shown() && !native_popup(window)?.isVisible(),
                                "Close path {step} left the popup or its click monitor open"
                            );
                            Ok(())
                        })??;
                        if step == 2 {
                            quit_expected.set(true);
                        }
                        unsafe {
                            test_button.performClick(None);
                        }
                        Ok(())
                    })?;
                }
                Ok::<(), anyhow::Error>(())
            }
            .await;
            quit_expected.set(false);
            if let Err(error) = result {
                eprintln!("UI smoke test failed: {error}");
                std::process::exit(1);
            }
            let _ = cx.update(|cx| {
                cx.quit();
                Ok::<(), anyhow::Error>(())
            });
        })
        .detach();
    }
    if std::env::var_os("MICLIP_DIAGNOSTICS").is_some() {
        println!("Mi Clip status item ready; no startup window.");
    }
    let status = StatusItem {
        item,
        _action: action,
        stop,
    };
    cx.spawn(async move |cx| {
        let _status = status;
        let clock = Instant::now();
        let mut feedback = Feedback::default();
        let mut file_icons = FileIcons::new();
        let mut animation_pending = false;
        let mut last_notification: Option<Instant> = None;
        let mut panel: Option<WindowHandle<Panel>> = None;
        let mut devices = crate::devices::Devices::default();
        let mut status_text = "尚未连接";
        let mut detail_text = "请先完成设备登录".to_owned();
        let mut update_text = if std::env::var_os("MICLIP_UPDATE_ROLLBACK").is_some() {
            "更新失败，重试".to_owned()
        } else {
            "检查更新".to_owned()
        };
        while let Ok(event) = receiver.recv().await {
            let (file_target, event) = if let TrayEvent::SessionFile(target, event) = event {
                (Some(target), TrayEvent::File(event))
            } else {
                (None, event)
            };
            let event = if let TrayEvent::SessionStatus(endpoint, status, detail) = event {
                if status == "未发现" {
                    devices.sessions.remove(&endpoint);
                } else {
                    devices.sessions.insert(endpoint, (status, detail.clone()));
                }
                if let Some(opened) = panel.as_ref() {
                    let _ = cx.update(|cx| {
                        opened.update(cx, |panel, _, cx| panel.set_devices(devices.clone(), cx))
                    });
                }
                let connected = devices
                    .sessions
                    .values()
                    .filter(|(state, _)| *state == "已连接")
                    .count();
                if connected > 0 {
                    TrayEvent::Status(
                        "已连接",
                        if status == "已连接" && !detail.is_empty() {
                            format!("{connected} 台设备已连接 · {detail}")
                        } else {
                            format!("{connected} 台设备已连接")
                        },
                    )
                } else {
                    if status == "未发现" {
                        TrayEvent::Status("等待设备", "设备上线后将自动连接".into())
                    } else {
                        TrayEvent::Status(status, detail)
                    }
                }
            } else {
                event
            };

            let result = cx.update(|cx| -> anyhow::Result<()> {
                let anchor = match event {
                    TrayEvent::Toggle(anchor) => anchor,
                    TrayEvent::Close => {
                        if let Some(opened) = panel.as_ref() {
                            opened.update(cx, |panel, window, _| hide_popup(panel, window))??;
                        }
                        return Ok(());
                    }
                    TrayEvent::Capture | TrayEvent::CaptureSettings => {
                        if let Some(opened) = panel.as_ref() {
                            opened.update(cx, |panel, window, _| hide_popup(panel, window))??;
                        }
                        if matches!(event, TrayEvent::Capture) {
                            crate::capture::open();
                        } else {
                            crate::capture::settings();
                        }
                        return Ok(());
                    }
                    TrayEvent::RefreshDevices => {
                        if let Some(request) = &discovery_request {
                            let _ = request.try_send(());
                        }
                        return Ok(());
                    }
                    TrayEvent::Discovering => {
                        devices.scanning = true;
                        if let Some(opened) = panel.as_ref() {
                            let _ = opened
                                .update(cx, |panel, _, cx| panel.set_devices(devices.clone(), cx));
                        }
                        return Ok(());
                    }
                    TrayEvent::Devices(result) => {
                        devices.scanning = false;
                        devices.failed = result.is_err();
                        if let Ok(entries) = result {
                            devices.entries = entries;
                        }
                        if let Some(opened) = panel.as_ref() {
                            let _ = opened
                                .update(cx, |panel, _, cx| panel.set_devices(devices.clone(), cx));
                        }
                        return Ok(());
                    }
                    TrayEvent::CheckUpdate => {
                        if std::env::var_os("MICLIP_UPDATE_DIAGNOSTICS").is_some() {
                            eprintln!("Mi Clip update: manual request received");
                        }
                        if runtime.update.lock().unwrap().is_some() {
                            runtime
                                .install_update
                                .store(true, std::sync::atomic::Ordering::Relaxed);
                            cx.quit();
                        } else if let Some(request) = &update_request {
                            update_text = match request.try_send(()) {
                                Ok(()) | Err(std::sync::mpsc::TrySendError::Full(())) => "检查中…",
                                Err(std::sync::mpsc::TrySendError::Disconnected(())) => {
                                    "检查服务已停止，请重启"
                                }
                            }
                            .into();
                        } else {
                            update_text = "检查服务未启动".into();
                        }
                        if let Some(opened) = panel.as_ref() {
                            let _ = opened
                                .update(cx, |panel, _, cx| panel.update_label(&update_text, cx));
                        }
                        return Ok(());
                    }
                    TrayEvent::Update(notice) => {
                        use crate::update_ui::Notice;
                        update_text = match notice {
                            Notice::Checking => "检查中…".into(),
                            Notice::Downloading(version) => format!("正在下载 v{version}…"),
                            Notice::Current => ui::current_version_label(),
                            Notice::Unpublished => "更新尚未发布".into(),
                            Notice::Failed => "检查失败 · 点击重试".into(),
                            Notice::Ready(update) => {
                                let text = format!("更新至 {}", update.version());
                                *runtime.update.lock().unwrap() = Some(update);
                                text
                            }
                        };
                        if std::env::var_os("MICLIP_UPDATE_DIAGNOSTICS").is_some() {
                            eprintln!("Mi Clip update state: {update_text}");
                        }
                        if let Some(opened) = panel.as_ref() {
                            let _ = opened
                                .update(cx, |panel, _, cx| panel.update_label(&update_text, cx));
                        }
                        return Ok(());
                    }
                    TrayEvent::SessionStatus(..) | TrayEvent::SessionFile(..) => unreachable!(),
                    TrayEvent::Status(status, detail) => {
                        if std::env::var_os("MICLIP_DIAGNOSTICS").is_some() {
                            println!("Mi Clip state: {status}; {detail}");
                        }
                        status_text = status;
                        detail_text = detail;
                        let detail = visible_detail(&feedback, &detail_text);
                        button.setToolTip(Some(&objc2_foundation::NSString::from_str(&format!(
                            "Mi Clip · {status}{}",
                            if detail.is_empty() {
                                String::new()
                            } else {
                                format!(" · {detail}")
                            }
                        ))));
                        if let Some(opened) = panel.as_ref() {
                            let _ = opened.update(cx, |panel, window, cx| {
                                panel.refresh(status_text, &detail, window, cx)
                            });
                        }
                        return Ok(());
                    }
                    TrayEvent::File(event) => {
                        let changed = feedback.apply_for(file_target, event, clock.elapsed());
                        if changed {
                            detail_text.clear();
                        }
                        if changed && std::env::var_os("MICLIP_DIAGNOSTICS").is_some() {
                            if let Some(detail) = feedback.detail() {
                                println!("Mi Clip file state: {detail}");
                            }
                        }
                        if changed && !testing {
                            match event {
                                FileEvent::Requested
                                    if last_notification.is_none_or(|last| {
                                        last.elapsed() >= Duration::from_secs(15)
                                    }) =>
                                {
                                    last_notification = Some(Instant::now());
                                    crate::notifications::deliver(false);
                                }
                                FileEvent::Saved { .. } => crate::notifications::deliver(true),
                                _ => (),
                            }
                        }
                        render_feedback(
                            &feedback,
                            &mut file_icons,
                            &button,
                            &image,
                            clock.elapsed(),
                            reduced_motion,
                            &detail_text,
                        )?;
                        if feedback.appearance(clock.elapsed()).2
                            && (!reduced_motion || feedback.active())
                            && !animation_pending
                        {
                            animation_pending = true;
                            schedule_frame(cx, sender.clone(), reduced_motion);
                        }
                        if let Some(detail) = feedback.detail() {
                            if let Some(opened) = panel.as_ref() {
                                let _ = opened.update(cx, |panel, window, cx| {
                                    panel.refresh(status_text, &detail, window, cx)
                                });
                            }
                        }
                        return Ok(());
                    }
                    TrayEvent::AnimationTick => {
                        animation_pending = false;
                        let changed = feedback.tick(clock.elapsed());
                        if changed {
                            if let (Some(detail), Some(opened)) =
                                (feedback.detail(), panel.as_ref())
                            {
                                let _ = opened.update(cx, |panel, window, cx| {
                                    panel.refresh(status_text, &detail, window, cx)
                                });
                            }
                        }
                        render_feedback(
                            &feedback,
                            &mut file_icons,
                            &button,
                            &image,
                            clock.elapsed(),
                            reduced_motion,
                            &detail_text,
                        )?;
                        if feedback.appearance(clock.elapsed()).2
                            && (!reduced_motion || feedback.active())
                        {
                            animation_pending = true;
                            schedule_frame(cx, sender.clone(), reduced_motion);
                        }
                        return Ok(());
                    }
                };
                if let Some(existing) = panel.as_ref() {
                    let hidden =
                        existing.update(cx, |panel, window, _| -> anyhow::Result<bool> {
                            if !panel.is_shown() {
                                return Ok(false);
                            }
                            hide_popup(panel, window)?;
                            Ok(true)
                        })??;
                    if hidden {
                        return Ok(());
                    }
                }
                if let Some(request) = &discovery_request {
                    let _ = request.try_send(());
                }
                let detail = visible_detail(&feedback, &detail_text);
                let opened = if let Some(existing) = panel {
                    existing.update(cx, |panel, window, cx| -> anyhow::Result<()> {
                        let native = native_popup(window)?;
                        let main_height = NSScreen::screens(MainThreadMarker::new().unwrap())
                            .firstObject()
                            .map_or(0., |screen| screen.frame().size.height);
                        native.setFrameTopLeftPoint(objc2_foundation::NSPoint::new(
                            f64::from(anchor.x),
                            main_height - f64::from(anchor.y),
                        ));
                        panel.refresh(status_text, &detail, window, cx)?;
                        Ok(())
                    })??;
                    existing
                } else {
                    // Allocate the native popup on the first click and reuse it thereafter.
                    let prepared = ui::prepare_status(status_text, &detail)?;
                    cx.open_window(
                        WindowOptions {
                            window_bounds: Some(WindowBounds::Windowed(Bounds::new(
                                point(px(anchor.x), px(anchor.y)),
                                ui::panel_size(&detail),
                            ))),
                            titlebar: None,
                            // A nonactivating AppKit panel accepts clicks while another
                            // app remains active; a Floating window can lose the click
                            // while macOS tries to activate this menu-bar-only app.
                            kind: WindowKind::PopUp,
                            is_resizable: false,
                            is_minimizable: false,
                            is_movable: false,
                            ..Default::default()
                        },
                        |window, cx| {
                            cx.new(|cx| {
                                Panel::new(
                                    prepared,
                                    sender.clone(),
                                    update_text.clone(),
                                    status_text,
                                    &detail,
                                    window,
                                    cx,
                                )
                            })
                        },
                    )?
                };
                panel = Some(opened);
                opened.update(cx, |panel, window, cx| {
                    panel.set_devices(devices.clone(), cx);
                    panel.start_monitoring();
                    window.activate_window();
                })?;
                // Acknowledge only once the user can actually see the panel.
                feedback.acknowledge();
                render_feedback(
                    &feedback,
                    &mut file_icons,
                    &button,
                    &image,
                    clock.elapsed(),
                    reduced_motion,
                    &detail_text,
                )?;
                Ok(())
            });
            if let Err(error) = result {
                eprintln!("Panel unavailable: {error}");
            }
        }
    })
    .detach();
    Ok(())
}
