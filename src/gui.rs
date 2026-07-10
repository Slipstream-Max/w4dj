use std::borrow::Cow;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::Duration;

use anyhow::Result;
use gpui::{
    App, Application, Bounds, ClickEvent, Context, ExternalPaths, FontWeight, Hsla, IntoElement,
    MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, ParentElement, PathPromptOptions,
    Pixels, Render, ScrollHandle, SharedString, StatefulInteractiveElement, Styled, Task,
    TitlebarOptions, Window, WindowAppearance, WindowBackgroundAppearance, WindowBounds,
    WindowControlArea, WindowOptions, canvas, div, point, prelude::*, px, relative, rgb, size,
};
use lucide_icons::{Icon, LUCIDE_FONT_BYTES};

use crate::config::{
    EditableConfig, GuiTheme, MAX_WINDOW_OPACITY, MIN_WINDOW_OPACITY, Mode,
    normalize_window_opacity,
};
use crate::sync::{self, SyncEvent, SyncSummary};

const ACCENT: u32 = 0x3a80db;
const ACCENT_HOVER: u32 = 0x2f6fbe;
const DANGER: u32 = 0xb42318;
const DANGER_HOVER: u32 = 0x912018;
const SOURCE_ROW_HEIGHT: f32 = 48.0;
const SCROLLBAR_BUTTON_HEIGHT: f32 = 18.0;
const SCROLLBAR_MIN_THUMB_HEIGHT: f32 = 28.0;
const OPACITY_STEP: f32 = 0.05;
const UI_TEXT_SIZE: f32 = 13.0;
const UI_MEDIUM_SIZE: f32 = 14.0;
const UI_LARGE_SIZE: f32 = 16.0;

#[derive(Clone, Copy)]
struct Palette {
    dark_mode: bool,
    background: u32,
    surface: u32,
    surface_strong: u32,
    ink: u32,
    muted: u32,
    border: u32,
    hover: u32,
    accent_soft: u32,
    danger_soft: u32,
    danger_border: u32,
    segment: u32,
    disabled: u32,
    disabled_text: u32,
    progress_track: u32,
    scrollbar: u32,
    scrollbar_track: u32,
    scrollbar_thumb: u32,
    scrollbar_thumb_hover: u32,
    output_border_hover: u32,
}

impl Palette {
    fn for_dark_mode(dark_mode: bool) -> Self {
        if dark_mode {
            Self {
                dark_mode: true,
                background: 0x151617,
                surface: 0x222426,
                surface_strong: 0x292b2e,
                ink: 0xf3f4f6,
                muted: 0xa8adb5,
                border: 0x44484f,
                hover: 0x303338,
                accent_soft: 0x1d3150,
                danger_soft: 0x3f2224,
                danger_border: 0x794042,
                segment: 0x191a1c,
                disabled: 0x35383c,
                disabled_text: 0x8c929a,
                progress_track: 0x383b40,
                scrollbar: 0x202224,
                scrollbar_track: 0x3b3e43,
                scrollbar_thumb: 0x747c86,
                scrollbar_thumb_hover: 0x949ba4,
                output_border_hover: 0x6b7078,
            }
        } else {
            Self {
                dark_mode: false,
                background: 0xf4f5f7,
                surface: 0xffffff,
                surface_strong: 0xffffff,
                ink: 0x17191d,
                muted: 0x6b717b,
                border: 0xd9dde3,
                hover: 0xf0f1f3,
                accent_soft: 0xeaf3fb,
                danger_soft: 0xfef0ee,
                danger_border: 0xf4b8b2,
                segment: 0xe7e9ed,
                disabled: 0xd8dce1,
                disabled_text: 0x8a9099,
                progress_track: 0xdfe3e8,
                scrollbar: 0xf5f6f8,
                scrollbar_track: 0xe2e6eb,
                scrollbar_thumb: 0x9da6b2,
                scrollbar_thumb_hover: 0x7d8896,
                output_border_hover: 0xa9afb8,
            }
        }
    }
}

fn glass_color(hex: u32, alpha: f32) -> Hsla {
    Hsla::from(rgb(hex)).alpha(alpha)
}

#[cfg(windows)]
fn set_native_titlebar_dark(dark_mode: bool) {
    use std::ffi::c_void;

    #[link(name = "user32")]
    unsafe extern "system" {
        fn GetActiveWindow() -> *mut c_void;
    }
    #[link(name = "dwmapi")]
    unsafe extern "system" {
        fn DwmSetWindowAttribute(
            window: *mut c_void,
            attribute: u32,
            value: *const c_void,
            value_size: u32,
        ) -> i32;
    }

    let value = i32::from(dark_mode);
    let window = unsafe { GetActiveWindow() };
    if window.is_null() {
        return;
    }
    let value_pointer = (&value as *const i32).cast::<c_void>();
    let result = unsafe {
        DwmSetWindowAttribute(
            window,
            20,
            value_pointer,
            std::mem::size_of_val(&value) as u32,
        )
    };
    if result < 0 {
        unsafe {
            DwmSetWindowAttribute(
                window,
                19,
                value_pointer,
                std::mem::size_of_val(&value) as u32,
            );
        }
    }
}

#[cfg(not(windows))]
fn set_native_titlebar_dark(_: bool) {}

const ASCII_LOGO: [&str; 6] = [
    "██╗    ██╗██╗  ██╗██████╗      ██╗     ██████╗ ██╗   ██╗██╗",
    "██║    ██║██║  ██║██╔══██╗     ██║    ██╔════╝ ██║   ██║██║",
    "██║ █╗ ██║███████║██║  ██║     ██║    ██║  ███╗██║   ██║██║",
    "██║███╗██║╚════██║██║  ██║██   ██║    ██║   ██║██║   ██║██║",
    "╚███╔███╔╝     ██║██████╔╝╚█████╔╝    ╚██████╔╝╚██████╔╝██║",
    " ╚══╝╚══╝      ╚═╝╚═════╝  ╚════╝      ╚═════╝  ╚═════╝ ╚═╝",
];

enum WorkerMessage {
    Event(SyncEvent),
    Done(Result<(), String>),
}

pub fn run() -> Result<()> {
    let (config, startup_error) = match EditableConfig::load_default() {
        Ok(config) => (config, None),
        Err(error) => (
            EditableConfig::empty_default()?,
            Some(format!("Could not load configuration: {error:#}")),
        ),
    };

    Application::new().run(move |cx: &mut App| {
        if let Err(error) = cx
            .text_system()
            .add_fonts(vec![Cow::Borrowed(LUCIDE_FONT_BYTES)])
        {
            eprintln!("w4dj: failed to load Lucide icons: {error:#}");
        }

        let bounds = Bounds::centered(None, size(px(960.0), px(620.0)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some("w4dj-gui".into()),
                    appears_transparent: cfg!(any(windows, target_os = "macos")),
                    traffic_light_position: if cfg!(target_os = "macos") {
                        Some(point(px(14.0), px(11.0)))
                    } else {
                        None
                    },
                }),
                window_min_size: Some(size(px(720.0), px(500.0))),
                window_background: WindowBackgroundAppearance::Blurred,
                app_id: Some("com.slipstream.w4dj".to_string()),
                ..Default::default()
            },
            |_, cx| cx.new(|_| W4djGui::new(config.clone(), startup_error.clone())),
        )
        .expect("failed to open the W4DJ window");
        cx.activate(true);
    });
    Ok(())
}

struct W4djGui {
    config: EditableConfig,
    session_inputs: Vec<PathBuf>,
    source_scroll: ScrollHandle,
    settings_open: bool,
    opacity_dragging: bool,
    syncing: bool,
    cancel_requested: bool,
    cancel_token: Option<Arc<AtomicBool>>,
    completed: usize,
    total: usize,
    summary: Option<SyncSummary>,
    error: Option<String>,
    sync_task: Option<Task<()>>,
}

impl W4djGui {
    fn new(config: EditableConfig, startup_error: Option<String>) -> Self {
        Self {
            config,
            session_inputs: Vec::new(),
            source_scroll: ScrollHandle::new(),
            settings_open: false,
            opacity_dragging: false,
            syncing: false,
            cancel_requested: false,
            cancel_token: None,
            completed: 0,
            total: 0,
            summary: None,
            error: startup_error,
            sync_task: None,
        }
    }

    fn source_count(&self) -> usize {
        self.config.inputs.len() + self.session_inputs.len()
    }

    fn progress(&self) -> f32 {
        if self.total == 0 {
            if self.summary.is_some() { 1.0 } else { 0.0 }
        } else {
            (self.completed as f32 / self.total as f32).clamp(0.0, 1.0)
        }
    }

    fn save_config(&mut self) {
        if let Err(error) = self.config.save() {
            self.error = Some(format!("Could not save configuration: {error:#}"));
        } else {
            self.error = None;
        }
    }

    fn palette(&self, window: &Window) -> Palette {
        let dark_mode = match self.config.theme {
            GuiTheme::Light => false,
            GuiTheme::Dark => true,
            GuiTheme::System => matches!(
                window.appearance(),
                WindowAppearance::Dark | WindowAppearance::VibrantDark
            ),
        };
        Palette::for_dark_mode(dark_mode)
    }

    fn toggle_settings(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.settings_open = !self.settings_open;
        cx.notify();
    }

    fn close_settings(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.settings_open = false;
        cx.notify();
    }

    fn select_theme(&mut self, theme: GuiTheme, cx: &mut Context<Self>) {
        if self.config.theme == theme {
            return;
        }
        self.config.theme = theme;
        self.save_config();
        cx.notify();
    }

    fn adjust_opacity(&mut self, delta: f32, cx: &mut Context<Self>) {
        self.config.window_opacity = normalize_window_opacity(self.config.window_opacity + delta);
        self.save_config();
        cx.notify();
    }

    fn update_opacity_from_slider(
        &mut self,
        pointer_x: Pixels,
        bounds: Bounds<Pixels>,
        cx: &mut Context<Self>,
    ) {
        if bounds.size.width <= px(0.0) {
            return;
        }
        let percentage = ((pointer_x - bounds.origin.x) / bounds.size.width).clamp(0.0, 1.0);
        self.config.window_opacity = normalize_window_opacity(
            MIN_WINDOW_OPACITY + percentage * (MAX_WINDOW_OPACITY - MIN_WINDOW_OPACITY),
        );
        cx.notify();
    }

    fn opacity_slider(&mut self, palette: Palette, cx: &mut Context<Self>) -> gpui::AnyElement {
        let percentage = ((self.config.window_opacity - MIN_WINDOW_OPACITY)
            / (MAX_WINDOW_OPACITY - MIN_WINDOW_OPACITY))
            .clamp(0.0, 1.0);
        let entity = cx.entity();

        div()
            .id("opacity-slider")
            .relative()
            .h_6()
            .min_w(px(140.0))
            .flex_1()
            .cursor_pointer()
            .child(
                div()
                    .absolute()
                    .left_0()
                    .right_0()
                    .top(px(10.0))
                    .h(px(4.0))
                    .rounded(px(2.0))
                    .bg(rgb(palette.progress_track))
                    .child(
                        div()
                            .h_full()
                            .w(relative(percentage))
                            .rounded(px(2.0))
                            .bg(rgb(ACCENT)),
                    ),
            )
            .child(
                div()
                    .absolute()
                    .top(px(5.0))
                    .left(relative(percentage))
                    .ml(px(-7.0))
                    .size(px(14.0))
                    .rounded(px(7.0))
                    .border_2()
                    .border_color(rgb(palette.surface_strong))
                    .bg(rgb(ACCENT)),
            )
            .child(
                canvas(
                    |_, _, _| (),
                    move |slider_bounds, _, window, _| {
                        window.on_mouse_event({
                            let entity = entity.clone();
                            move |event: &MouseDownEvent, _, _, cx| {
                                if event.button != MouseButton::Left
                                    || !slider_bounds.contains(&event.position)
                                {
                                    return;
                                }
                                entity.update(cx, |this, cx| {
                                    this.opacity_dragging = true;
                                    this.update_opacity_from_slider(
                                        event.position.x,
                                        slider_bounds,
                                        cx,
                                    );
                                });
                            }
                        });
                        window.on_mouse_event({
                            let entity = entity.clone();
                            move |event: &MouseMoveEvent, _, _, cx| {
                                if !event.dragging() || !entity.read(cx).opacity_dragging {
                                    return;
                                }
                                entity.update(cx, |this, cx| {
                                    this.update_opacity_from_slider(
                                        event.position.x,
                                        slider_bounds,
                                        cx,
                                    );
                                });
                            }
                        });
                        window.on_mouse_event(move |event: &MouseUpEvent, _, _, cx| {
                            if event.button != MouseButton::Left
                                || !entity.read(cx).opacity_dragging
                            {
                                return;
                            }
                            entity.update(cx, |this, cx| {
                                this.opacity_dragging = false;
                                this.save_config();
                                cx.notify();
                            });
                        });
                    },
                )
                .absolute()
                .top_0()
                .right_0()
                .bottom_0()
                .left_0(),
            )
            .into_any_element()
    }

    fn custom_titlebar(&self, palette: Palette) -> gpui::AnyElement {
        div()
            .id("custom-titlebar")
            .h(px(38.0))
            .w_full()
            .flex_none()
            .flex()
            .items_center()
            .text_color(rgb(palette.ink))
            .child(
                div()
                    .id("titlebar-drag-area")
                    .h_full()
                    .min_w_0()
                    .flex_1()
                    .px_3()
                    .flex()
                    .items_center()
                    .gap_2()
                    .when(cfg!(target_os = "macos"), |this| this.pl(px(76.0)))
                    .window_control_area(WindowControlArea::Drag)
                    .on_mouse_down(MouseButton::Left, |event, window, _| {
                        if event.click_count == 2 {
                            window.titlebar_double_click();
                        } else {
                            window.start_window_move();
                        }
                    })
                    .on_click(|event, window, _| {
                        if event.is_right_click() {
                            window.show_window_menu(event.position());
                        }
                    })
                    .child(
                        icon(Icon::AudioLines)
                            .text_color(rgb(ACCENT))
                            .text_size(px(UI_MEDIUM_SIZE)),
                    )
                    .child(
                        div()
                            .text_size(px(UI_TEXT_SIZE))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("w4dj-gui"),
                    ),
            )
            .when(cfg!(windows), |this| {
                this.child(
                    div()
                        .id("titlebar-minimize")
                        .h_full()
                        .w(px(46.0))
                        .flex_none()
                        .flex()
                        .items_center()
                        .justify_center()
                        .cursor_pointer()
                        .window_control_area(WindowControlArea::Min)
                        .hover(|this| this.bg(rgb(palette.hover)))
                        .on_click(|_, window, _| window.minimize_window())
                        .child(icon(Icon::Minus).text_size(px(UI_TEXT_SIZE))),
                )
                .child(
                    div()
                        .id("titlebar-maximize")
                        .h_full()
                        .w(px(46.0))
                        .flex_none()
                        .flex()
                        .items_center()
                        .justify_center()
                        .cursor_pointer()
                        .window_control_area(WindowControlArea::Max)
                        .hover(|this| this.bg(rgb(palette.hover)))
                        .on_click(|_, window, _| window.zoom_window())
                        .child(icon(Icon::Square).text_xs()),
                )
                .child(
                    div()
                        .id("titlebar-close")
                        .h_full()
                        .w(px(46.0))
                        .flex_none()
                        .flex()
                        .items_center()
                        .justify_center()
                        .cursor_pointer()
                        .window_control_area(WindowControlArea::Close)
                        .hover(|this| this.bg(rgb(0xc42b1c)).text_color(rgb(0xffffff)))
                        .on_click(|_, window, _| window.remove_window())
                        .child(icon(Icon::X).text_size(px(UI_TEXT_SIZE))),
                )
            })
            .into_any_element()
    }

    fn settings_overlay(&mut self, palette: Palette, cx: &mut Context<Self>) -> gpui::AnyElement {
        let opacity = self.config.window_opacity;
        let opacity_label = format!("{}%", (opacity * 100.0).round() as u32);
        let slider = self.opacity_slider(palette, cx);

        div()
            .id("settings-overlay")
            .absolute()
            .top_0()
            .right_0()
            .bottom_0()
            .left_0()
            .p_8()
            .flex()
            .items_center()
            .justify_center()
            .bg(glass_color(
                0x000000,
                if !palette.dark_mode { 0.24 } else { 0.44 },
            ))
            .child(
                div()
                    .id("settings-panel")
                    .w(px(440.0))
                    .max_w_full()
                    .p_5()
                    .rounded(px(8.0))
                    .border_1()
                    .border_color(rgb(palette.border))
                    .bg(rgb(palette.surface_strong))
                    .shadow_lg()
                    .text_color(rgb(palette.ink))
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .h_9()
                            .flex()
                            .items_center()
                            .justify_between()
                            .child(
                                div()
                                    .text_size(px(UI_MEDIUM_SIZE))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child("Appearance"),
                            )
                            .child(
                                div()
                                    .id("close-settings")
                                    .size_8()
                                    .rounded(px(4.0))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .cursor_pointer()
                                    .text_color(rgb(palette.muted))
                                    .hover(|this| {
                                        this.bg(rgb(palette.hover)).text_color(rgb(palette.ink))
                                    })
                                    .on_click(cx.listener(Self::close_settings))
                                    .child(icon(Icon::X).text_size(px(UI_MEDIUM_SIZE))),
                            ),
                    )
                    .child(
                        div()
                            .py_4()
                            .border_b_1()
                            .border_color(rgb(palette.border))
                            .flex()
                            .items_center()
                            .justify_between()
                            .gap_4()
                            .child(
                                div()
                                    .text_size(px(UI_TEXT_SIZE))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child("Theme"),
                            )
                            .child(
                                div()
                                    .p_1()
                                    .rounded(px(6.0))
                                    .bg(rgb(palette.segment))
                                    .flex()
                                    .children(
                                        [GuiTheme::Light, GuiTheme::Dark, GuiTheme::System]
                                            .into_iter()
                                            .map(|theme| {
                                                let selected = self.config.theme == theme;
                                                div()
                                                    .id(SharedString::from(format!(
                                                        "theme-{}",
                                                        theme_label(theme)
                                                    )))
                                                    .h_8()
                                                    .w(px(92.0))
                                                    .rounded(px(4.0))
                                                    .flex()
                                                    .items_center()
                                                    .justify_center()
                                                    .gap_2()
                                                    .text_size(px(UI_TEXT_SIZE))
                                                    .text_color(rgb(if selected {
                                                        palette.ink
                                                    } else {
                                                        palette.muted
                                                    }))
                                                    .when(selected, |this| {
                                                        this.bg(glass_color(
                                                            palette.surface_strong,
                                                            0.96,
                                                        ))
                                                        .shadow_sm()
                                                    })
                                                    .cursor_pointer()
                                                    .hover(|this| this.text_color(rgb(palette.ink)))
                                                    .on_click(cx.listener(
                                                        move |this, _: &ClickEvent, _, cx| {
                                                            this.select_theme(theme, cx)
                                                        },
                                                    ))
                                                    .child(
                                                        icon(theme_icon(theme))
                                                            .text_size(px(UI_TEXT_SIZE)),
                                                    )
                                                    .child(theme_label(theme))
                                            }),
                                    ),
                            ),
                    )
                    .child(
                        div()
                            .py_4()
                            .border_b_1()
                            .border_color(rgb(palette.border))
                            .flex()
                            .flex_col()
                            .gap_3()
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .justify_between()
                                    .child(
                                        div()
                                            .text_size(px(UI_TEXT_SIZE))
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .child("Glass opacity"),
                                    )
                                    .child(
                                        div()
                                            .text_size(px(UI_TEXT_SIZE))
                                            .text_color(rgb(palette.muted))
                                            .child(opacity_label),
                                    ),
                            )
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_3()
                                    .child(
                                        div()
                                            .id("decrease-opacity")
                                            .size_8()
                                            .rounded(px(4.0))
                                            .border_1()
                                            .border_color(rgb(palette.border))
                                            .flex()
                                            .items_center()
                                            .justify_center()
                                            .cursor_pointer()
                                            .text_color(rgb(palette.muted))
                                            .hover(|this| {
                                                this.bg(rgb(palette.hover))
                                                    .text_color(rgb(palette.ink))
                                            })
                                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                                this.adjust_opacity(-OPACITY_STEP, cx)
                                            }))
                                            .child(icon(Icon::Minus).text_size(px(UI_TEXT_SIZE))),
                                    )
                                    .child(slider)
                                    .child(
                                        div()
                                            .id("increase-opacity")
                                            .size_8()
                                            .rounded(px(4.0))
                                            .border_1()
                                            .border_color(rgb(palette.border))
                                            .flex()
                                            .items_center()
                                            .justify_center()
                                            .cursor_pointer()
                                            .text_color(rgb(palette.muted))
                                            .hover(|this| {
                                                this.bg(rgb(palette.hover))
                                                    .text_color(rgb(palette.ink))
                                            })
                                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                                this.adjust_opacity(OPACITY_STEP, cx)
                                            }))
                                            .child(icon(Icon::Plus).text_size(px(UI_TEXT_SIZE))),
                                    ),
                            ),
                    )
                    .child(
                        div().pt_4().flex().justify_end().child(
                            div()
                                .id("open-config")
                                .h_9()
                                .px_3()
                                .rounded(px(5.0))
                                .border_1()
                                .border_color(rgb(palette.border))
                                .flex()
                                .items_center()
                                .gap_2()
                                .cursor_pointer()
                                .text_size(px(UI_TEXT_SIZE))
                                .text_color(rgb(palette.ink))
                                .hover(|this| this.bg(rgb(palette.hover)))
                                .on_click(cx.listener(Self::open_config))
                                .child(icon(Icon::FileCog).text_size(px(UI_MEDIUM_SIZE)))
                                .child("Open config"),
                        ),
                    ),
            )
            .into_any_element()
    }

    fn choose_folders(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.syncing {
            return;
        }
        let receiver = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: true,
            prompt: Some("Add folders".into()),
        });
        cx.spawn(async move |this, cx| {
            let selection = receiver.await;
            let _ = this.update(cx, |this, cx| {
                match selection {
                    Ok(Ok(Some(paths))) => {
                        for path in paths {
                            push_unique(&mut this.config.inputs, path);
                        }
                        this.save_config();
                    }
                    Ok(Err(error)) => {
                        this.error = Some(format!("Could not open folder picker: {error:#}"));
                    }
                    Err(error) => {
                        this.error = Some(format!("Folder picker closed unexpectedly: {error}"));
                    }
                    Ok(Ok(None)) => {}
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn choose_output(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.syncing {
            return;
        }
        let receiver = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Choose output folder".into()),
        });
        cx.spawn(async move |this, cx| {
            let selection = receiver.await;
            let _ = this.update(cx, |this, cx| {
                match selection {
                    Ok(Ok(Some(paths))) => {
                        if let Some(path) = paths.into_iter().next() {
                            this.config.output = Some(path);
                            this.save_config();
                        }
                    }
                    Ok(Err(error)) => {
                        this.error = Some(format!("Could not open folder picker: {error:#}"));
                    }
                    Err(error) => {
                        this.error = Some(format!("Folder picker closed unexpectedly: {error}"));
                    }
                    Ok(Ok(None)) => {}
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn open_config(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.settings_open = false;
        if !self.config.path.exists() {
            self.save_config();
        }
        if self.config.path.exists()
            && let Err(error) = open::that(&self.config.path)
        {
            self.error = Some(format!("Could not open configuration: {error}"));
        }
        cx.notify();
    }

    fn remove_configured(&mut self, index: usize, cx: &mut Context<Self>) {
        if self.syncing || index >= self.config.inputs.len() {
            return;
        }
        self.config.inputs.remove(index);
        self.save_config();
        cx.notify();
    }

    fn remove_session(&mut self, index: usize, cx: &mut Context<Self>) {
        if self.syncing || index >= self.session_inputs.len() {
            return;
        }
        self.session_inputs.remove(index);
        cx.notify();
    }

    fn add_dropped_paths(&mut self, paths: &[PathBuf], cx: &mut Context<Self>) {
        if self.syncing {
            return;
        }
        for path in paths {
            push_unique(&mut self.session_inputs, path.clone());
        }
        self.error = None;
        cx.notify();
    }

    fn select_mode(&mut self, mode: Mode, cx: &mut Context<Self>) {
        if self.syncing || self.config.mode == mode {
            return;
        }
        self.config.mode = mode;
        self.save_config();
        cx.notify();
    }

    fn scroll_sources_by(&mut self, amount: f32, cx: &mut Context<Self>) {
        let max_offset = self.source_scroll.max_offset().height;
        let current = self.source_scroll.offset();
        let next_y = (current.y + px(amount)).clamp(-max_offset, px(0.0));
        self.source_scroll.set_offset(point(px(0.0), next_y));
        cx.notify();
    }

    fn jump_sources(&mut self, event: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        let bounds = self.source_scroll.bounds();
        let max_offset = self.source_scroll.max_offset().height;
        let track_height = bounds.size.height - px(SCROLLBAR_BUTTON_HEIGHT * 2.0);
        if max_offset <= px(0.0) || track_height <= px(0.0) {
            return;
        }

        let track_y = bounds.origin.y + px(SCROLLBAR_BUTTON_HEIGHT);
        let percentage = ((event.position().y - track_y) / track_height).clamp(0.0, 1.0);
        self.source_scroll
            .set_offset(point(px(0.0), -max_offset * percentage));
        cx.notify();
    }

    fn source_scrollbar(&mut self, palette: Palette, cx: &mut Context<Self>) -> gpui::AnyElement {
        let bounds = self.source_scroll.bounds();
        let viewport_height = bounds.size.height;
        let max_offset = self.source_scroll.max_offset().height;
        let track_height = (viewport_height - px(SCROLLBAR_BUTTON_HEIGHT * 2.0)).max(px(1.0));
        let content_height = viewport_height + max_offset;
        let thumb_height = if viewport_height > px(0.0) && content_height > px(0.0) {
            (track_height * (viewport_height / content_height))
                .max(px(SCROLLBAR_MIN_THUMB_HEIGHT))
                .min(track_height)
        } else {
            px(SCROLLBAR_MIN_THUMB_HEIGHT).min(track_height)
        };
        let scroll_percentage = if max_offset > px(0.0) {
            (-self.source_scroll.offset().y / max_offset).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let thumb_top = (track_height - thumb_height) * scroll_percentage;

        div()
            .id("source-scrollbar")
            .absolute()
            .top_0()
            .right_0()
            .bottom_0()
            .w(px(14.0))
            .border_l_1()
            .border_color(rgb(palette.border))
            .bg(rgb(palette.scrollbar))
            .flex()
            .flex_col()
            .child(
                div()
                    .id("source-scroll-up")
                    .h(px(SCROLLBAR_BUTTON_HEIGHT))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .cursor_pointer()
                    .text_color(rgb(palette.muted))
                    .hover(|this| this.bg(rgb(palette.hover)).text_color(rgb(palette.ink)))
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                        this.scroll_sources_by(SOURCE_ROW_HEIGHT, cx)
                    }))
                    .child(icon(Icon::ChevronUp).text_xs()),
            )
            .child(
                div()
                    .id("source-scroll-track")
                    .relative()
                    .min_h_0()
                    .flex_1()
                    .mx(px(3.0))
                    .rounded(px(4.0))
                    .bg(rgb(palette.scrollbar_track))
                    .cursor_pointer()
                    .on_click(cx.listener(Self::jump_sources))
                    .child(
                        div()
                            .absolute()
                            .top(thumb_top)
                            .left_0()
                            .right_0()
                            .h(thumb_height)
                            .rounded(px(4.0))
                            .bg(rgb(palette.scrollbar_thumb))
                            .hover(|this| this.bg(rgb(palette.scrollbar_thumb_hover))),
                    ),
            )
            .child(
                div()
                    .id("source-scroll-down")
                    .h(px(SCROLLBAR_BUTTON_HEIGHT))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .cursor_pointer()
                    .text_color(rgb(palette.muted))
                    .hover(|this| this.bg(rgb(palette.hover)).text_color(rgb(palette.ink)))
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                        this.scroll_sources_by(-SOURCE_ROW_HEIGHT, cx)
                    }))
                    .child(icon(Icon::ChevronDown).text_xs()),
            )
            .into_any_element()
    }

    fn start_sync(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.syncing {
            return;
        }
        let config = match self.config.runtime_config(&self.session_inputs) {
            Ok(config) => config,
            Err(error) => {
                self.error = Some(format!("Cannot start sync: {error:#}"));
                cx.notify();
                return;
            }
        };

        self.syncing = true;
        self.cancel_requested = false;
        self.completed = 0;
        self.total = 0;
        self.summary = None;
        self.error = None;

        let (sender, receiver) = mpsc::channel();
        let cancel_token = Arc::new(AtomicBool::new(false));
        self.cancel_token = Some(Arc::clone(&cancel_token));
        let worker_sender = sender.clone();
        let worker = std::thread::Builder::new()
            .name("w4dj-sync".to_string())
            .spawn(move || {
                let event_sender = worker_sender.clone();
                let result =
                    sync::run_with_progress_cancellable(&config, &cancel_token, move |event| {
                        let _ = event_sender.send(WorkerMessage::Event(event));
                    })
                    .map(|_| ())
                    .map_err(|error| format!("{error:#}"));
                let _ = worker_sender.send(WorkerMessage::Done(result));
            });

        if let Err(error) = worker {
            self.syncing = false;
            self.cancel_token = None;
            self.error = Some(format!("Could not start sync worker: {error}"));
            cx.notify();
            return;
        }

        self.sync_task = Some(cx.spawn(async move |this, cx| {
            loop {
                let mut done = false;
                while let Ok(message) = receiver.try_recv() {
                    if matches!(message, WorkerMessage::Done(_)) {
                        done = true;
                    }
                    if this
                        .update(cx, |this, cx| {
                            this.apply_worker_message(message);
                            cx.notify();
                        })
                        .is_err()
                    {
                        return;
                    }
                }
                if done {
                    return;
                }
                cx.background_executor()
                    .timer(Duration::from_millis(60))
                    .await;
            }
        }));
        cx.notify();
    }

    fn cancel_sync(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        if !self.syncing || self.cancel_requested {
            return;
        }
        if let Some(cancel_token) = &self.cancel_token {
            cancel_token.store(true, Ordering::Relaxed);
            self.cancel_requested = true;
            self.error = None;
            cx.notify();
        }
    }

    fn apply_worker_message(&mut self, message: WorkerMessage) {
        match message {
            WorkerMessage::Event(SyncEvent::Status(_)) => {}
            WorkerMessage::Event(SyncEvent::Progress {
                completed,
                total,
                current: _,
            }) => {
                if completed >= self.completed {
                    self.completed = completed;
                    self.total = total;
                }
            }
            WorkerMessage::Event(SyncEvent::Finished(summary)) => {
                self.error = summary.errors.first().cloned();
                self.summary = Some(summary);
            }
            WorkerMessage::Event(SyncEvent::Cancelled(summary)) => {
                self.error = None;
                self.summary = Some(summary);
            }
            WorkerMessage::Done(result) => {
                self.syncing = false;
                self.cancel_requested = false;
                self.cancel_token = None;
                if let Err(error) = result
                    && self.error.is_none()
                {
                    self.error = Some(error);
                }
            }
        }
    }

    fn source_row(
        &self,
        path: PathBuf,
        configured: bool,
        index: usize,
        palette: Palette,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let is_directory = path.is_dir() || path.extension().is_none();
        let label = path.display().to_string();
        let remove = cx.listener(move |this, _: &ClickEvent, _, cx| {
            if configured {
                this.remove_configured(index, cx);
            } else {
                this.remove_session(index, cx);
            }
        });

        div()
            .h(px(48.0))
            .px_4()
            .flex()
            .items_center()
            .gap_3()
            .border_b_1()
            .border_color(rgb(palette.border))
            .child(
                icon(if is_directory {
                    Icon::Folder
                } else {
                    Icon::FileAudio
                })
                .text_color(rgb(ACCENT))
                .text_size(px(UI_LARGE_SIZE)),
            )
            .child(
                div()
                    .min_w_0()
                    .flex_1()
                    .overflow_hidden()
                    .text_ellipsis()
                    .whitespace_nowrap()
                    .text_size(px(UI_TEXT_SIZE))
                    .child(label),
            )
            .child(
                div()
                    .px_2()
                    .py_1()
                    .rounded(px(4.0))
                    .bg(rgb(palette.accent_soft))
                    .text_color(rgb(ACCENT))
                    .text_xs()
                    .child(if configured { "CONFIG" } else { "SESSION" }),
            )
            .child(
                div()
                    .id(if configured {
                        SharedString::from(format!("remove-configured-{index}"))
                    } else {
                        SharedString::from(format!("remove-session-{index}"))
                    })
                    .size_7()
                    .rounded(px(4.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .cursor_pointer()
                    .text_color(rgb(palette.muted))
                    .hover(|this| this.bg(rgb(palette.hover)).text_color(rgb(palette.ink)))
                    .on_click(remove)
                    .child(icon(Icon::X).text_size(px(UI_TEXT_SIZE))),
            )
            .into_any_element()
    }
}

impl Render for W4djGui {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        window.set_background_appearance(WindowBackgroundAppearance::Blurred);
        let palette = self.palette(window);
        set_native_titlebar_dark(palette.dark_mode);
        let window_opacity = normalize_window_opacity(self.config.window_opacity);
        let configured_inputs = self.config.resolved_inputs();
        let session_inputs = self.session_inputs.clone();
        let output = self.config.resolved_output().display().to_string();
        let can_start_sync = !self.syncing && self.source_count() > 0;
        let progress = self.progress();
        let source_viewport_height = self.source_scroll.bounds().size.height;
        let source_content_height = px(self.source_count() as f32 * SOURCE_ROW_HEIGHT);
        let show_source_scrollbar = self.source_scroll.max_offset().height > px(0.0)
            || source_content_height > source_viewport_height;

        if self.source_count() > 0 && source_viewport_height == px(0.0) {
            let entity = cx.entity();
            window.on_next_frame(move |_, cx| cx.notify(entity.entity_id()));
        }

        div()
            .relative()
            .size_full()
            .bg(glass_color(palette.background, window_opacity))
            .text_color(rgb(palette.ink))
            .text_size(px(UI_TEXT_SIZE))
            .font_family(".SystemUIFont")
            .flex()
            .flex_col()
            .when(cfg!(any(windows, target_os = "macos")), |this| {
                this.child(self.custom_titlebar(palette))
            })
            .child(
                div()
                    .relative()
                    .min_h_0()
                    .flex_1()
                    .p_8()
                    .flex()
                    .flex_col()
                    .gap_5()
                    .child(
                        div()
                            .relative()
                            .h(px(54.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(
                                div()
                                    .id("open-settings")
                                    .absolute()
                                    .left_0()
                                    .bottom_0()
                                    .size_10()
                                    .rounded(px(6.0))
                                    .border_1()
                                    .border_color(rgb(palette.border))
                                    .bg(rgb(palette.surface))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .cursor_pointer()
                                    .text_color(rgb(palette.ink))
                                    .hover(|this| this.bg(rgb(palette.hover)))
                                    .when(self.settings_open, |this| {
                                        this.border_color(rgb(ACCENT))
                                            .bg(rgb(palette.accent_soft))
                                            .text_color(rgb(ACCENT))
                                    })
                                    .on_click(cx.listener(Self::toggle_settings))
                                    .child(icon(Icon::Settings).text_size(px(UI_LARGE_SIZE))),
                            )
                            .child(
                                div()
                                    .font_family("Consolas")
                                    .flex()
                                    .flex_col()
                                    .text_size(px(9.0))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(rgb(ACCENT))
                                    .children(ASCII_LOGO.into_iter().enumerate().map(
                                        |(index, line)| {
                                            div()
                                                .id(SharedString::from(format!(
                                                    "ascii-logo-{index}"
                                                )))
                                                .h(px(9.0))
                                                .flex_none()
                                                .whitespace_nowrap()
                                                .line_height(px(9.0))
                                                .child(line)
                                        },
                                    )),
                            )
                            .child(
                                div()
                                    .id("add-folder")
                                    .absolute()
                                    .right_0()
                                    .bottom_0()
                                    .h_9()
                                    .px_4()
                                    .rounded(px(6.0))
                                    .bg(rgb(ACCENT))
                                    .text_color(rgb(0xffffff))
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .cursor_pointer()
                                    .hover(|this| this.bg(rgb(ACCENT_HOVER)))
                                    .on_click(cx.listener(Self::choose_folders))
                                    .child(icon(Icon::FolderPlus).text_size(px(UI_MEDIUM_SIZE)))
                                    .child("Add folder"),
                            ),
                    )
                    .when_some(self.error.clone(), |this, error| {
                        this.child(
                            div()
                                .px_4()
                                .py_3()
                                .rounded(px(6.0))
                                .border_1()
                                .border_color(rgb(palette.danger_border))
                                .bg(rgb(palette.danger_soft))
                                .text_color(rgb(DANGER))
                                .text_size(px(UI_TEXT_SIZE))
                                .child(error),
                        )
                    })
                    .child(
                        div()
                            .id("sources")
                            .min_h_0()
                            .flex_1()
                            .rounded(px(6.0))
                            .border_1()
                            .border_color(rgb(palette.border))
                            .bg(rgb(palette.surface))
                            .overflow_hidden()
                            .flex()
                            .flex_col()
                            .drag_over::<ExternalPaths>(move |style, _, _, _| {
                                style.border_color(rgb(ACCENT)).bg(rgb(palette.accent_soft))
                            })
                            .on_drop(cx.listener(|this, paths: &ExternalPaths, _, cx| {
                                this.add_dropped_paths(paths.paths(), cx)
                            }))
                            .child(
                                div()
                                    .h(px(44.0))
                                    .px_4()
                                    .flex()
                                    .items_center()
                                    .justify_between()
                                    .border_b_1()
                                    .border_color(rgb(palette.border))
                                    .child(
                                        div()
                                            .text_size(px(UI_TEXT_SIZE))
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .child("Sources"),
                                    )
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(rgb(palette.muted))
                                            .child(format!("{} ITEM(S)", self.source_count())),
                                    ),
                            )
                            .child(
                                div()
                                    .relative()
                                    .min_h_0()
                                    .flex_1()
                                    .child(
                                        div()
                                            .id("source-scroll")
                                            .size_full()
                                            .overflow_y_scroll()
                                            .scrollbar_width(px(14.0))
                                            .track_scroll(&self.source_scroll)
                                            .children(
                                                configured_inputs.into_iter().enumerate().map(
                                                    |(index, path)| {
                                                        self.source_row(
                                                            path, true, index, palette, cx,
                                                        )
                                                    },
                                                ),
                                            )
                                            .children(session_inputs.into_iter().enumerate().map(
                                                |(index, path)| {
                                                    self.source_row(path, false, index, palette, cx)
                                                },
                                            ))
                                            .when(self.source_count() == 0, |this| {
                                                this.child(
                                                    div()
                                                        .h_full()
                                                        .flex()
                                                        .items_center()
                                                        .justify_center()
                                                        .gap_2()
                                                        .text_color(rgb(palette.muted))
                                                        .child(
                                                            icon(Icon::FileAudio)
                                                                .text_size(px(UI_LARGE_SIZE)),
                                                        )
                                                        .child("Drop files or folders"),
                                                )
                                            }),
                                    )
                                    .when(show_source_scrollbar, |this| {
                                        this.child(self.source_scrollbar(palette, cx))
                                    }),
                            ),
                    )
                    .child(
                        div()
                            .id("output-folder")
                            .relative()
                            .h(px(56.0))
                            .px_4()
                            .rounded(px(6.0))
                            .border_1()
                            .border_color(rgb(palette.border))
                            .bg(rgb(palette.surface))
                            .flex()
                            .items_center()
                            .gap_3()
                            .cursor_pointer()
                            .hover(|this| this.border_color(rgb(palette.output_border_hover)))
                            .on_click(cx.listener(Self::choose_output))
                            .child(
                                icon(Icon::HardDrive)
                                    .text_color(rgb(ACCENT))
                                    .text_size(px(UI_LARGE_SIZE)),
                            )
                            .child(
                                div()
                                    .relative()
                                    .bottom(px(3.0))
                                    .flex()
                                    .flex_col()
                                    .min_w_0()
                                    .flex_1()
                                    .gap_1()
                                    .child(
                                        div()
                                            .text_xs()
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .text_color(rgb(palette.muted))
                                            .child("OUTPUT"),
                                    )
                                    .child(
                                        div()
                                            .overflow_hidden()
                                            .text_ellipsis()
                                            .whitespace_nowrap()
                                            .text_size(px(UI_TEXT_SIZE))
                                            .child(output),
                                    ),
                            )
                            .child(
                                div()
                                    .flex_none()
                                    .text_xs()
                                    .text_color(rgb(palette.muted))
                                    .child(if self.total > 0 {
                                        format!("{} / {}", self.completed, self.total)
                                    } else {
                                        "0 / 0".to_string()
                                    }),
                            )
                            .child(
                                div()
                                    .absolute()
                                    .left_0()
                                    .right_0()
                                    .bottom_0()
                                    .h(px(5.0))
                                    .overflow_hidden()
                                    .bg(rgb(palette.progress_track))
                                    .child(div().h_full().w(relative(progress)).bg(rgb(
                                        if self.error.is_some() { DANGER } else { ACCENT },
                                    ))),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .gap_4()
                            .child(
                                div()
                                    .p_1()
                                    .rounded(px(6.0))
                                    .bg(rgb(palette.segment))
                                    .flex()
                                    .items_center()
                                    .children(
                                        [Mode::Mp3, Mode::Wav, Mode::Original].into_iter().map(
                                            |mode| {
                                                let selected = self.config.mode == mode;
                                                div()
                                                    .id(SharedString::from(format!(
                                                        "mode-{}",
                                                        mode_label(mode)
                                                    )))
                                                    .h_8()
                                                    .w(px(82.0))
                                                    .rounded(px(4.0))
                                                    .flex()
                                                    .items_center()
                                                    .justify_center()
                                                    .text_size(px(UI_TEXT_SIZE))
                                                    .font_weight(if selected {
                                                        FontWeight::SEMIBOLD
                                                    } else {
                                                        FontWeight::NORMAL
                                                    })
                                                    .text_color(rgb(if selected {
                                                        palette.ink
                                                    } else {
                                                        palette.muted
                                                    }))
                                                    .when(selected, |this| {
                                                        this.bg(rgb(palette.surface_strong))
                                                            .shadow_sm()
                                                    })
                                                    .when(!self.syncing, |this| {
                                                        this.cursor_pointer()
                                                .hover(|this| this.text_color(rgb(palette.ink)))
                                                .on_click(cx.listener(
                                                    move |this, _: &ClickEvent, _, cx| {
                                                        this.select_mode(mode, cx)
                                                    },
                                                ))
                                                    })
                                                    .child(mode_label(mode))
                                            },
                                        ),
                                    ),
                            )
                            .child(
                                div()
                                    .id("sync")
                                    .h_10()
                                    .w(px(190.0))
                                    .rounded(px(6.0))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .gap_2()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .when(can_start_sync, |this| {
                                        this.bg(rgb(ACCENT))
                                            .text_color(rgb(0xffffff))
                                            .cursor_pointer()
                                            .hover(|this| this.bg(rgb(ACCENT_HOVER)))
                                            .on_click(cx.listener(Self::start_sync))
                                    })
                                    .when(self.syncing && !self.cancel_requested, |this| {
                                        this.bg(rgb(DANGER))
                                            .text_color(rgb(0xffffff))
                                            .cursor_pointer()
                                            .hover(|this| this.bg(rgb(DANGER_HOVER)))
                                            .on_click(cx.listener(Self::cancel_sync))
                                    })
                                    .when(self.cancel_requested, |this| {
                                        this.bg(rgb(palette.disabled))
                                            .text_color(rgb(palette.disabled_text))
                                    })
                                    .when(!can_start_sync && !self.syncing, |this| {
                                        this.bg(rgb(palette.disabled))
                                            .text_color(rgb(palette.disabled_text))
                                    })
                                    .child(icon(if self.cancel_requested {
                                        Icon::RotateCw
                                    } else if self.syncing {
                                        Icon::Square
                                    } else {
                                        Icon::Play
                                    }))
                                    .child(if self.cancel_requested {
                                        "Cancelling"
                                    } else if self.syncing {
                                        "Cancel"
                                    } else {
                                        "Sync"
                                    }),
                            ),
                    )
                    .when(self.settings_open, |this| {
                        this.child(self.settings_overlay(palette, cx))
                    }),
            )
    }
}

fn icon(icon: Icon) -> gpui::Div {
    div()
        .font_family("lucide")
        .line_height(relative(1.0))
        .child(icon.unicode().to_string())
}

fn mode_label(mode: Mode) -> &'static str {
    match mode {
        Mode::Original => "Ori",
        Mode::Mp3 => "MP3",
        Mode::Wav => "WAV",
    }
}

fn theme_label(theme: GuiTheme) -> &'static str {
    match theme {
        GuiTheme::Light => "Light",
        GuiTheme::Dark => "Dark",
        GuiTheme::System => "System",
    }
}

fn theme_icon(theme: GuiTheme) -> Icon {
    match theme {
        GuiTheme::Light => Icon::Sun,
        GuiTheme::Dark => Icon::Moon,
        GuiTheme::System => Icon::Monitor,
    }
}

fn push_unique(paths: &mut Vec<PathBuf>, path: PathBuf) {
    let key = path_key(&path);
    if !paths.iter().any(|existing| path_key(existing) == key) {
        paths.push(path);
    }
}

fn path_key(path: &Path) -> String {
    let value = path.to_string_lossy();
    if cfg!(windows) {
        value.to_lowercase()
    } else {
        value.into_owned()
    }
}
