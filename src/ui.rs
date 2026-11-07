use gpui_rhai::gpui::{
    Context, InteractiveElement, IntoElement, ParentElement, Render, StatefulInteractiveElement,
    Styled, Subscription, Window, div, px, rgba,
};
use gpui_rhai::{
    EmbeddedScriptSource, EmbeddedScriptView, ModuleId, PreparedScriptView, ScriptViewConfig,
    ScriptViewHandle, ScriptViewHost,
};
use std::collections::BTreeMap;

pub fn current_version_label() -> String {
    format!("已是最新 · v{} · 检查更新", env!("CARGO_PKG_VERSION"))
}

pub fn prepare() -> anyhow::Result<PreparedScriptView> {
    prepare_status("尚未连接", "请先完成设备登录")
}
pub fn prepare_status(status: &str, detail: &str) -> anyhow::Result<PreparedScriptView> {
    let id = ModuleId::parse("panel")?;
    let online = matches!(status, "已连接" | "已联机" | "同步中");
    Ok(EmbeddedScriptView::new(
        id.clone(),
        EmbeddedScriptSource::new(BTreeMap::from([(
            id,
            include_str!("../ui/panel.rhai")
                .replace(
                    "\"Mi Clip\"",
                    &serde_json::to_string(&format!("Mi Clip · v{}", env!("CARGO_PKG_VERSION")))?,
                )
                .replace(
                    "\"尚未连接\"",
                    &serde_json::to_string(if online { "✓" } else { status })?,
                )
                .replace(
                    "\"status_color\"",
                    if online {
                        "\"success\""
                    } else {
                        "\"text_muted\""
                    },
                )
                .replace("\"等待设备认证\"", &serde_json::to_string(detail)?),
        )])),
        include_str!("../ui/theme.rhai"),
    )
    .prepare()
    .map_err(|error| anyhow::anyhow!(error.to_string()))?)
}

fn content_height(detail: &str) -> f32 {
    if detail.is_empty() {
        return 48.0;
    }
    // Reserve space for wrapped status details.
    let lines: usize = detail
        .lines()
        .map(|line| {
            let units: usize = line.chars().map(|c| if c.is_ascii() { 1 } else { 2 }).sum();
            units.div_ceil(36).max(1)
        })
        .sum();
    54.0 + lines as f32 * 18.0
}
pub const PANEL_WIDTH: f32 = 280.0;
pub fn panel_size(detail: &str) -> gpui_rhai::gpui::Size<gpui_rhai::gpui::Pixels> {
    gpui_rhai::gpui::size(
        px(PANEL_WIDTH),
        px(content_height(detail) + 40.0 + 28.0 + 32.0 + 232.0),
    )
}

pub struct Panel {
    host: ScriptViewHost,
    view: Result<ScriptViewHandle, String>,
    _blur: Subscription,
    _outside_click: OutsideClickMonitor,
    events: async_channel::Sender<crate::tray::TrayEvent>,
    pub(crate) update_label: String,
    content_height: f32,
    devices: crate::devices::Devices,
    status: String,
}
impl Panel {
    pub(crate) fn device_count(&self) -> usize {
        self.devices.entries.len()
    }
    pub(crate) fn set_devices(&mut self, devices: crate::devices::Devices, cx: &mut Context<Self>) {
        self.devices = devices;
        cx.notify();
    }
    pub fn update_label(&mut self, label: &str, cx: &mut Context<Self>) {
        self.update_label = label.to_owned();
        cx.notify();
    }
    pub fn refresh(
        &mut self,
        status: &str,
        detail: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> anyhow::Result<()> {
        self.status = status.to_owned();
        let prepared = prepare_status(status, detail)?;
        self.content_height = content_height(detail);
        window.resize(panel_size(detail));
        if let Ok(view) = &self.view {
            let _ = view.dispose(cx);
        }
        self.view = prepared
            .mount(
                ScriptViewConfig::new("status"),
                self.host.clone(),
                window,
                cx,
            )
            .map_err(|error| error.to_string());
        if let Ok(view) = &self.view {
            let _ = view.focus(window, cx);
        }
        cx.notify();
        Ok(())
    }
    pub fn validate(&self, cx: &gpui_rhai::gpui::App) -> anyhow::Result<()> {
        anyhow::ensure!(
            self._outside_click.0.is_some(),
            "Outside click monitor unavailable"
        );
        let view = self
            .view
            .as_ref()
            .map_err(|error| anyhow::anyhow!(error.clone()))?;
        anyhow::ensure!(
            view.root(cx)
                .map_err(|e| anyhow::anyhow!(e.to_string()))?
                .is_some(),
            "No committed Rhai tree"
        );
        if let Some(error) = view
            .last_error(cx)
            .map_err(|e| anyhow::anyhow!(e.to_string()))?
        {
            anyhow::bail!(error);
        }
        Ok(())
    }
    pub fn new(
        prepared: PreparedScriptView,
        events: async_channel::Sender<crate::tray::TrayEvent>,
        update_label: String,
        status: &str,
        detail: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let host = ScriptViewHost::new("status-panel", cx).expect("static host identifier");
        let view = prepared
            .mount(ScriptViewConfig::new("status"), host.clone(), window, cx)
            .map_err(|error| error.to_string());
        if let Ok(view) = &view {
            let _ = view.focus(window, cx);
        }
        let outside_click = OutsideClickMonitor::new(events.clone());
        let blur = cx.observe_window_activation(window, |_, window, _| {
            if !window.is_window_active() {
                window.remove_window();
            }
        });
        Self {
            host,
            view,
            _blur: blur,
            _outside_click: outside_click,
            events,
            update_label,
            content_height: content_height(detail),
            devices: Default::default(),
            status: status.into(),
        }
    }
}
// AppKit popup panels can remain nonactivating, so a key-window change alone
// does not reliably detect clicks in other apps. Monitor mouse-down only while
// this popup exists; no polling, keyboard capture or accessibility permission.
struct OutsideClickMonitor(Option<objc2::rc::Retained<objc2::runtime::AnyObject>>);
impl OutsideClickMonitor {
    fn new(events: async_channel::Sender<crate::tray::TrayEvent>) -> Self {
        use objc2_app_kit::{NSEvent, NSEventMask};
        let handler = block2::RcBlock::new(move |_: std::ptr::NonNull<NSEvent>| {
            let _ = events.try_send(crate::tray::TrayEvent::Close);
        });
        Self(NSEvent::addGlobalMonitorForEventsMatchingMask_handler(
            NSEventMask::LeftMouseDown | NSEventMask::RightMouseDown | NSEventMask::OtherMouseDown,
            &handler,
        ))
    }
}
impl Drop for OutsideClickMonitor {
    fn drop(&mut self) {
        if let Some(monitor) = self.0.take() {
            // SAFETY: This token came from NSEvent's monitor API above and is
            // removed exactly once on the application's main thread.
            unsafe {
                objc2_app_kit::NSEvent::removeMonitor(&monitor);
            }
        }
    }
}
impl Render for Panel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let checking = self.update_label == "检查中…" || self.update_label.starts_with("正在下载 ");
        let content = match &self.view {
            Ok(view) => view
                .element()
                .expect("mounted status panel")
                .into_any_element(),
            Err(error) => div()
                .p(px(16.0))
                .child(format!("界面加载失败：{error}"))
                .into_any_element(),
        };
        let rows = self.devices.rows(&self.status);
        let empty = rows.is_empty();
        let mut list = div()
            .id("device-list")
            .flex_1()
            .min_h(px(0.0))
            .overflow_y_scroll();
        for (name, detail) in rows {
            list = list.child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(3.0))
                    .py(px(7.0))
                    .child(div().text_ellipsis().child(name))
                    .child(
                        div()
                            .text_size(px(10.0))
                            .text_color(rgba(0xa4acbaff))
                            .text_ellipsis()
                            .child(detail),
                    ),
            );
        }
        if empty {
            list = list.child(div().py(px(12.0)).text_color(rgba(0xa4acbaff)).child(
                if self.devices.scanning {
                    "正在发现局域网设备…"
                } else if self.devices.failed {
                    "暂未发现设备"
                } else {
                    "未发现设备"
                },
            ));
        }
        let has_device = !self.devices.entries.is_empty();
        let discovery_note = if self.devices.failed && has_device {
            "暂时无法刷新 · 显示上次发现的设备"
        } else if self.status == "已连接" {
            "双方认证通过 · 连接已建立"
        } else if !has_device {
            "未发现设备 · 请解锁并开启互联"
        } else {
            "发现后自动连接同账号设备"
        };
        self.host.container(
            div()
                .size_full()
                .flex()
                .flex_col()
                .bg(rgba(0x191b20ff))
                .text_color(rgba(0xf0f2f5ff))
                .text_size(px(12.0))
                .on_action(|_: &crate::ClosePanel, window, _| window.remove_window())
                .on_key_down(|event, window, _| {
                    if event.keystroke.key == "escape" {
                        window.remove_window();
                    }
                })
                // ScriptViewHandle's host has size_full(), so constrain it to
                // the content area instead of letting it push buttons offscreen.
                .child(
                    div()
                        .h(px(self.content_height))
                        .flex_shrink_0()
                        .overflow_hidden()
                        .child(content),
                )
                .child(
                    div()
                        .h(px(40.0))
                        .flex_shrink_0()
                        .flex()
                        .items_center()
                        .justify_between()
                        .px(px(12.0))
                        .child(
                            div()
                                .id("update")
                                .px(px(8.0))
                                .py(px(6.0))
                                .text_size(px(12.0))
                                .rounded(px(6.0))
                                .cursor_pointer()
                                .hover(|style| style.bg(rgba(0x2b3038ff)))
                                .on_click(cx.listener(move |panel, _, _, cx| {
                                    if !checking {
                                        if std::env::var_os("MICLIP_UPDATE_DIAGNOSTICS").is_some() {
                                            eprintln!("Mi Clip update: button clicked");
                                        }
                                        let label = if panel
                                            .events
                                            .try_send(crate::tray::TrayEvent::CheckUpdate)
                                            .is_ok()
                                        {
                                            if panel.update_label.starts_with("更新至 ") {
                                                "正在安装…"
                                            } else {
                                                "检查中…"
                                            }
                                        } else {
                                            "请求失败，点击重试"
                                        };
                                        panel.update_label(label, cx);
                                    }
                                }))
                                .child(self.update_label.clone()),
                        )
                        .child(
                            div()
                                .id("quit")
                                .px(px(8.0))
                                .py(px(6.0))
                                .text_size(px(12.0))
                                .rounded(px(6.0))
                                .cursor_pointer()
                                .hover(|style| style.bg(rgba(0x2b3038ff)))
                                .on_click(|_, _, cx| cx.quit())
                                .child("退出"),
                        ),
                )
                .child(
                    div()
                        .h(px(32.0))
                        .flex_shrink_0()
                        .flex()
                        .items_center()
                        .justify_between()
                        .px(px(14.0))
                        .child(
                            div()
                                .id("capture")
                                .cursor_pointer()
                                .on_click(cx.listener(|panel, _, _, _| {
                                    let _ = panel.events.try_send(crate::tray::TrayEvent::Capture);
                                }))
                                .child("截图"),
                        )
                        .child(
                            div()
                                .id("capture-settings")
                                .cursor_pointer()
                                .text_size(px(10.0))
                                .on_click(cx.listener(|panel, _, _, _| {
                                    let _ = panel
                                        .events
                                        .try_send(crate::tray::TrayEvent::CaptureSettings);
                                }))
                                .child("截图快捷键设置"),
                        ),
                )
                .child(
                    div()
                        .id("share-setup")
                        .h(px(28.0))
                        .flex_shrink_0()
                        .px(px(14.0))
                        .text_size(px(10.0))
                        .text_color(rgba(0xa4acbaff))
                        .cursor_pointer()
                        .on_click(|_, _, _| crate::sharing_setup::open_help())
                        .child(crate::sharing_setup::hint()),
                )
                .child(
                    div()
                        .h(px(232.0))
                        .flex_shrink_0()
                        .flex()
                        .flex_col()
                        .px(px(14.0))
                        .pb(px(10.0))
                        .border_t_1()
                        .border_color(rgba(0x30343cff))
                        .child(
                            div()
                                .h(px(36.0))
                                .flex_shrink_0()
                                .flex()
                                .items_center()
                                .justify_between()
                                .child("局域网设备")
                                .child(
                                    div()
                                        .id("refresh-devices")
                                        .cursor_pointer()
                                        .px(px(6.0))
                                        .py(px(5.0))
                                        .rounded(px(4.0))
                                        .hover(|s| s.bg(rgba(0x2b3038ff)))
                                        .on_click(cx.listener(|panel, _, _, cx| {
                                            if !panel.devices.scanning {
                                                if panel
                                                    .events
                                                    .try_send(
                                                        crate::tray::TrayEvent::RefreshDevices,
                                                    )
                                                    .is_ok()
                                                {
                                                    panel.devices.scanning = true;
                                                    cx.notify();
                                                }
                                            }
                                        }))
                                        .child(if self.devices.scanning {
                                            "发现中…"
                                        } else {
                                            "刷新"
                                        }),
                                ),
                        )
                        .child(list)
                        .child(
                            div()
                                .flex_shrink_0()
                                .pt(px(6.0))
                                .text_size(px(10.0))
                                .text_color(rgba(0xa4acbaff))
                                .child(discovery_note),
                        ),
                ),
        )
    }
}
