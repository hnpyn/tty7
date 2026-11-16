//! One process, one window, one terminal.

use gpui::{
    AnyWindowHandle, App, AppContext as _, Bounds, Context, Entity, FocusHandle, Focusable, Global,
    InteractiveElement, IntoElement, ParentElement, PromptLevel, Render, Styled, Subscription,
    TitlebarOptions, WeakEntity, Window, WindowBounds, WindowDecorations, WindowOptions, div, px,
    size,
};
#[cfg(target_os = "macos")]
use gpui::{MouseButton, MouseDownEvent, MouseMoveEvent, WindowControlArea, point};
use gpui_component::{Root, WindowExt as _};

use crate::core::actions::{
    CloseActiveTab, CloseWindow, DecreaseFontSize, HideApp, HideOthers, IncreaseFontSize,
    MinimizeWindow, NewTab, NewWindow, Quit, ResetFontSize, ShowAll, ToggleFullscreen, ZoomWindow,
};
use crate::core::config::{Config, ConfirmClose, FONT_SIZE_MAX, FONT_SIZE_MIN, StartupMode};
use crate::terminal::view::{ChildExited, PaneBusy, TerminalView};
use crate::ui::i18n::{L10nKey, t, t_fmt};

pub(crate) struct SoloWindow {
    terminal: Entity<TerminalView>,
    /// Quit when the shell exits. Replaced before a retarget kills the old
    /// pane, so that exit does not close the window. The subscription is the
    /// value; nothing else reads it.
    _child_exit: Subscription,
    close_prompt_open: bool,
    _subs: Vec<Subscription>,
}

/// The one window this process owns. The global Quit binding routes through
/// it so a confirmation that is already up, or a minimized window that is not
/// the active one, still gets the question instead of an immediate exit.
#[derive(Clone)]
struct LiveSolo {
    window: AnyWindowHandle,
    solo: WeakEntity<SoloWindow>,
}

impl Global for LiveSolo {}

/// What ⌘Q should do, given whether a window is still around and whether its
/// confirmation is already on screen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum QuitRoute {
    /// Ask, or leave immediately when the close policy has nothing to ask.
    Confirm,
    /// A confirmation is already showing. Let it finish.
    LeavePrompt,
    /// Nothing left that can ask.
    ExitNow,
}

pub(crate) fn quit_route(window_live: bool, prompt_open: bool) -> QuitRoute {
    if !window_live {
        QuitRoute::ExitNow
    } else if prompt_open {
        QuitRoute::LeavePrompt
    } else {
        QuitRoute::Confirm
    }
}

pub(crate) fn route_new_window(cx: &mut App) {
    let cwd = cx.try_global::<LiveSolo>().cloned().and_then(|live| {
        live.window
            .update(cx, |_root, _window, cx| {
                live.solo
                    .upgrade()
                    .and_then(|solo| solo.read(cx).terminal.read(cx).local_cwd())
            })
            .ok()
            .flatten()
    });
    crate::spawn_new_instance(cwd.as_deref());
}

pub(crate) fn route_quit(cx: &mut App) {
    let Some(live) = cx.try_global::<LiveSolo>().cloned() else {
        crate::cleanup_and_quit(cx);
        return;
    };
    cx.activate(true);
    let outcome = live.window.update(cx, |_root, window, cx| {
        window.activate_window();
        let Some(solo) = live.solo.upgrade() else {
            return QuitRoute::ExitNow;
        };
        solo.update(cx, |solo, cx| {
            match quit_route(true, solo.close_prompt_open) {
                QuitRoute::LeavePrompt => QuitRoute::LeavePrompt,
                QuitRoute::Confirm => {
                    solo.request_close(window, cx);
                    QuitRoute::Confirm
                }
                QuitRoute::ExitNow => QuitRoute::ExitNow,
            }
        })
    });
    match outcome {
        Ok(QuitRoute::LeavePrompt | QuitRoute::Confirm) => {}
        _ => crate::cleanup_and_quit(cx),
    }
}

impl SoloWindow {
    fn open(
        parts: crate::terminal::view::ShellParts,
        launched_without_path: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        crate::ui::theme::apply_theme(Some(window), cx);
        let notice = parts.shell_notice.clone();
        let terminal = cx.new(|cx| TerminalView::from_shell_parts(parts, window, cx));
        if let Some(notice) = notice {
            window.push_notification(notice, cx);
        }
        let child_exit = Self::quit_when_the_shell_exits(&terminal, cx);
        let config_watch = cx.observe_global::<Config>(|this, cx| this.apply_config(cx));
        let appearance = window.observe_window_appearance(|window, cx| {
            crate::ui::theme::note_system_appearance(window, cx);
            if cx.global::<Config>().theme_follow_system {
                // `apply_theme` rebuilds the menu from the appearance it just
                // read off this window.
                crate::ui::theme::apply_theme(Some(window), cx);
            } else {
                crate::ui::theme::set_menus(cx);
            }
        });
        let weak = cx.weak_entity();
        cx.set_global(LiveSolo {
            window: window.window_handle(),
            solo: weak.clone(),
        });
        window.on_window_should_close(cx, move |window, cx| {
            let Some(solo) = weak.upgrade() else {
                return true;
            };
            solo.update(cx, |solo, cx| solo.request_close(window, cx));
            false
        });
        let mut this = Self {
            terminal,
            _child_exit: child_exit,
            close_prompt_open: false,
            _subs: vec![config_watch, appearance],
        };
        this.apply_config(cx);
        if launched_without_path {
            // `open -a ntty7 /dir` delivers the URL on the next turn of the
            // main loop, after this window already exists. Poll across the
            // launch grace and retarget in place instead of leaving $HOME up.
            cx.spawn_in(window, async move |this, cx| {
                loop {
                    if let Some(path) = crate::take_launch_retarget() {
                        let _ = this.update_in(cx, |solo, window, cx| {
                            solo.retarget(path, window, cx);
                        });
                        break;
                    }
                    if !crate::within_launch_grace() {
                        break;
                    }
                    cx.background_executor()
                        .timer(std::time::Duration::from_millis(20))
                        .await;
                }
            })
            .detach();
        }
        this
    }

    fn quit_when_the_shell_exits(
        terminal: &Entity<TerminalView>,
        cx: &mut Context<Self>,
    ) -> Subscription {
        cx.subscribe(terminal, |_this, _view, _: &ChildExited, cx| {
            // The shell is already gone. `exit` and Ctrl+D close the process
            // without a second question.
            crate::cleanup_and_quit(cx);
        })
    }

    /// Point the one window at `cwd`. A busy shell, or a shell that will not
    /// start, becomes another process so this one stays where it is.
    fn retarget(&mut self, cwd: std::path::PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        if self.terminal.read(cx).busy().is_some() {
            crate::spawn_new_instance(Some(&cwd));
            return;
        }
        let parts = match TerminalView::spawn_shell_terminal_in(
            None,
            Some(cwd.clone()),
            None,
            None,
            None,
        ) {
            Ok(parts) => parts,
            Err(error) => {
                log::error!("could not open {}: {error:#}", cwd.display());
                window.push_notification(
                    t_fmt(
                        L10nKey::ShellCouldNotStart,
                        &[("error", &format!("{error:#}"))],
                    ),
                    cx,
                );
                return;
            }
        };
        let notice = parts.shell_notice.clone();
        let old_pane = self.terminal.read(cx).pane_id;
        // The reader's teardown warning is for a link that died on its own.
        // Retiring this view is not that: mark it quit before the pane is
        // killed, or the close looks like a dropped connection.
        self.terminal
            .update(cx, |view, _cx| view.terminal.detach_link());
        let next = cx.new(|cx| TerminalView::from_shell_parts(parts, window, cx));
        // Drop the old quit subscription before the old pane is killed, or
        // its `ChildExited` closes the window we just retargeted.
        self._child_exit = Self::quit_when_the_shell_exits(&next, cx);
        self.terminal = next;
        self.apply_config(cx);
        TerminalView::kill_pane(old_pane);
        if let Some(notice) = notice {
            window.push_notification(notice, cx);
        }
        window.focus(&self.focus_handle(cx), cx);
        cx.notify();
    }

    fn apply_config(&mut self, cx: &mut Context<Self>) {
        let config = cx.global::<Config>().clone();
        let features = config
            .font_features
            .as_ref()
            .map(crate::core::config::gpui_font_features);
        self.terminal.update(cx, |view, cx| {
            view.font_size = px(config.font_size);
            view.line_height_mul = config.line_height;
            view.set_font_family(config.font_family.clone(), cx);
            view.set_font_family_bold(config.font_family_bold.clone(), cx);
            view.set_font_family_italic(config.font_family_italic.clone(), cx);
            view.reread_fallback_chain(cx);
            view.set_font_features(features, cx);
            view.report_mouse = config.mouse_reporting;
            view.set_prompt_editor(config.prompt_editor, cx);
            view.terminal.apply_user_config(&config);
            cx.notify();
        });
    }

    fn request_close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.close_prompt_open {
            return;
        }
        let policy = cx.global::<Config>().confirm_close;
        let busy = self.terminal.read(cx).busy();
        if !needs_confirm(policy, busy.is_some()) {
            crate::cleanup_and_quit(cx);
            return;
        }
        self.close_prompt_open = true;
        let title = quit_title(&cx.global::<Config>().gui_language);
        let body = quit_body(busy.as_ref());
        let answer = window.prompt(
            PromptLevel::Warning,
            title,
            Some(&body),
            &crate::ui::confirm_answers(t(L10nKey::Close), t(L10nKey::Keep)),
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            let close = matches!(answer.await, Ok(0));
            let _ = this.update(cx, |this, cx| {
                this.close_prompt_open = false;
                if close {
                    crate::cleanup_and_quit(cx);
                }
            });
        })
        .detach();
    }

    fn spawn_another(&mut self, cx: &mut Context<Self>) {
        let cwd = self.terminal.read(cx).local_cwd();
        crate::spawn_new_instance(cwd.as_deref());
    }

    fn set_font_size(&mut self, next: f32, cx: &mut Context<Self>) {
        let next = next.clamp(FONT_SIZE_MIN, FONT_SIZE_MAX);
        if (next - cx.global::<Config>().font_size).abs() < f32::EPSILON {
            return;
        }
        cx.global_mut::<Config>().font_size = next;
        let _ = cx.global::<Config>().try_save();
        self.apply_config(cx);
    }
}

impl Focusable for SoloWindow {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.terminal.read(cx).focus_handle.clone()
    }
}

impl Render for SoloWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .bg(crate::ui::theme::workspace_background(cx))
            .children(crate::ui::theme::window_background_image_layer(cx))
            .child(self.terminal.clone())
            .on_action(cx.listener(|this, _: &NewWindow, _, cx| this.spawn_another(cx)))
            .on_action(cx.listener(|this, _: &NewTab, _, cx| this.spawn_another(cx)))
            .on_action(
                cx.listener(|this, _: &CloseWindow, window, cx| this.request_close(window, cx)),
            )
            .on_action(
                cx.listener(|this, _: &CloseActiveTab, window, cx| this.request_close(window, cx)),
            )
            .on_action(cx.listener(|this, _: &Quit, window, cx| this.request_close(window, cx)))
            .on_action(cx.listener(|this, _: &IncreaseFontSize, _, cx| {
                let next = cx.global::<Config>().font_size + 1.0;
                this.set_font_size(next, cx);
            }))
            .on_action(cx.listener(|this, _: &DecreaseFontSize, _, cx| {
                let next = cx.global::<Config>().font_size - 1.0;
                this.set_font_size(next, cx);
            }))
            .on_action(cx.listener(|this, _: &ResetFontSize, _, cx| {
                let next = Config::default().font_size;
                this.set_font_size(next, cx);
            }))
            .on_action(cx.listener(|_, _: &ToggleFullscreen, window, _| {
                window.toggle_fullscreen();
            }))
            .on_action(cx.listener(|_, _: &HideApp, _, cx| cx.hide()))
            .on_action(cx.listener(|_, _: &HideOthers, _, cx| cx.hide_other_apps()))
            .on_action(cx.listener(|_, _: &ShowAll, _, cx| cx.unhide_other_apps()))
            .on_action(cx.listener(|_, _: &MinimizeWindow, window, _| window.minimize_window()))
            .on_action(cx.listener(|_, _: &ZoomWindow, window, _| window.zoom_window()))
            .children(macos_drag_edge(window, cx))
    }
}

/// The top padding of a borderless macOS window. `GRID_PAD_Y` is 4px, so a
/// taller strip would cover the first row of cells. Native traffic lights are
/// parked off-screen, and this gpui build ignores `WindowControlArea::Drag`,
/// so the strip drags the window itself.
#[cfg(target_os = "macos")]
fn drag_edge(window: &mut Window, cx: &mut Context<SoloWindow>) -> impl IntoElement {
    let moving = window.use_state(cx, |_, _| false);
    div()
        .id("ntty7-drag-edge")
        .absolute()
        .top_0()
        .left_0()
        .right_0()
        .h(px(4.))
        .window_control_area(WindowControlArea::Drag)
        .on_mouse_down_out(window.listener_for(&moving, |moving, _, _, _| {
            *moving = false;
        }))
        .on_mouse_down(
            MouseButton::Left,
            window.listener_for(&moving, |moving, event: &MouseDownEvent, window, cx| {
                cx.stop_propagation();
                if event.click_count >= 2 {
                    *moving = false;
                    window.titlebar_double_click();
                } else {
                    // `performWindowDragWithEvent` wants the mouse-down. A 4px
                    // strip loses the later move as soon as the pointer enters
                    // the terminal, so the drag starts here.
                    *moving = true;
                    window.start_window_move();
                }
            }),
        )
        .on_mouse_up(
            MouseButton::Left,
            window.listener_for(&moving, |moving, _, _, _| {
                *moving = false;
            }),
        )
        .on_mouse_move(window.listener_for(
            &moving,
            |moving, event: &MouseMoveEvent, window, cx| {
                if *moving && event.dragging() {
                    *moving = false;
                    cx.stop_propagation();
                    window.start_window_move();
                }
            },
        ))
}

fn macos_drag_edge(window: &mut Window, cx: &mut Context<SoloWindow>) -> Vec<gpui::AnyElement> {
    #[cfg(target_os = "macos")]
    {
        vec![drag_edge(window, cx).into_any_element()]
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (window, cx);
        Vec::new()
    }
}

pub(crate) fn open(cx: &mut App, cwd: Option<std::path::PathBuf>) {
    let cwd = cwd.or_else(crate::take_pending_open);
    crate::mark_window_live(cwd.as_deref());
    let launched_without_path = cwd.is_none();
    let cwd = cwd.or_else(crate::core::config::working_directory_base);
    match TerminalView::spawn_shell_terminal_in(None, cwd, None, None, None) {
        Ok(parts) => {
            let options = window_options(cx);
            let opened = cx.open_window(options, move |window, cx| {
                let solo = cx.new(|cx| SoloWindow::open(parts, launched_without_path, window, cx));
                cx.new(|cx| Root::new(solo, window, cx).bg(gpui::transparent_black()))
            });
            if let Err(error) = opened {
                log::error!("failed to open window: {error}");
                crate::cleanup_and_quit(cx);
            }
        }
        Err(error) => open_startup_failure(cx, &error),
    }
}

/// A cold start whose login shell could not open a PTY. The prompt is the
/// only thing the user sees; quitting is `cleanup_and_quit`, so Dock and
/// Spotlight still get a window instead of a silent exit.
fn open_startup_failure(cx: &mut App, error: &anyhow::Error) {
    log::error!("terminal failed to start: {error:#}");
    let detail = t_fmt(
        L10nKey::ShellCouldNotStart,
        &[("error", &format!("{error:#}"))],
    );
    let options = window_options(cx);
    let opened = cx.open_window(options, move |window, cx| {
        let failure = cx.new(|cx| StartupFailure::new(detail, window, cx));
        cx.new(|cx| Root::new(failure, window, cx).bg(gpui::transparent_black()))
    });
    if let Err(error) = opened {
        log::error!("failed to open window: {error}");
        crate::cleanup_and_quit(cx);
    }
}

struct StartupFailure;

impl StartupFailure {
    fn new(detail: String, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let answer = window.prompt(
            PromptLevel::Critical,
            t(L10nKey::ShellCouldNotStartTitle),
            Some(&detail),
            &[gpui::PromptButton::ok(t(L10nKey::AppMenuQuit))],
            cx,
        );
        window.on_window_should_close(cx, |_window, cx| {
            crate::cleanup_and_quit(cx);
            true
        });
        cx.spawn_in(window, async move |this, cx| {
            let _ = answer.await;
            this.update(cx, |_this, cx| crate::cleanup_and_quit(cx))
                .ok();
        })
        .detach();
        Self
    }
}

impl Render for StartupFailure {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().size_full()
    }
}

fn window_options(cx: &mut App) -> WindowOptions {
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    static APP_ICON: std::sync::LazyLock<Option<std::sync::Arc<image::RgbaImage>>> =
        std::sync::LazyLock::new(|| {
            image::load_from_memory(include_bytes!("../../assets/app-icon.png"))
                .ok()
                .map(|image| std::sync::Arc::new(image.thumbnail(256, 256).into_rgba8()))
        });

    let display_id = focused_display_id(cx);
    let bounds = Bounds::centered(display_id, size(px(800.), px(600.)), cx);
    let window_bounds = match cx.global::<Config>().startup_mode {
        StartupMode::Normal => WindowBounds::Windowed(bounds),
        StartupMode::Maximized => WindowBounds::Maximized(bounds),
        StartupMode::Fullscreen => WindowBounds::Fullscreen(bounds),
    };
    WindowOptions {
        window_bounds: Some(window_bounds),
        display_id,
        app_id: Some("ntty7".to_owned()),
        #[cfg(any(target_os = "linux", target_os = "freebsd"))]
        icon: APP_ICON.as_ref().cloned(),
        titlebar: Some(TitlebarOptions {
            title: Some("ntty7".into()),
            appears_transparent: cfg!(target_os = "macos"),
            traffic_light_position: macos_traffic_lights(),
        }),
        window_decorations: Some(if cfg!(target_os = "macos") {
            WindowDecorations::Client
        } else {
            WindowDecorations::Server
        }),
        window_background: crate::ui::theme::background_appearance(cx),
        window_min_size: Some(size(px(320.), px(200.))),
        ..Default::default()
    }
}

fn focused_display_id(cx: &mut App) -> Option<gpui::DisplayId> {
    #[cfg(target_os = "macos")]
    {
        #[repr(C)]
        #[derive(Clone, Copy)]
        struct CGPoint {
            x: f64,
            y: f64,
        }
        #[link(name = "CoreGraphics", kind = "framework")]
        unsafe extern "C" {
            fn CGEventCreate(source: *const std::ffi::c_void) -> *mut std::ffi::c_void;
            fn CGEventGetLocation(event: *mut std::ffi::c_void) -> CGPoint;
            fn CGGetDisplaysWithPoint(
                point: CGPoint,
                max_displays: u32,
                displays: *mut u32,
                matching_display_count: *mut u32,
            ) -> i32;
        }
        unsafe {
            let ev = CGEventCreate(std::ptr::null());
            if !ev.is_null() {
                let pt = CGEventGetLocation(ev);
                core_foundation::base::CFRelease(ev as _);
                let mut id: u32 = 0;
                let mut count: u32 = 0;
                if CGGetDisplaysWithPoint(pt, 1, &mut id, &mut count) == 0 && count > 0 {
                    let candidate = gpui::DisplayId::new(id as u64);
                    if cx.find_display(candidate).is_some() {
                        return Some(candidate);
                    }
                }
            }
        }
    }
    cx.active_window()
        .and_then(|w| {
            w.update(cx, |_, window, cx| window.display(cx).map(|d| d.id()))
                .ok()
        })
        .flatten()
}

#[cfg(target_os = "macos")]
fn macos_traffic_lights() -> Option<gpui::Point<gpui::Pixels>> {
    Some(point(px(-200.), px(-200.)))
}

#[cfg(not(target_os = "macos"))]
fn macos_traffic_lights() -> Option<gpui::Point<gpui::Pixels>> {
    None
}

pub(crate) fn needs_confirm(policy: ConfirmClose, busy: bool) -> bool {
    match policy {
        ConfirmClose::Never => false,
        ConfirmClose::WhenBusy => busy,
        ConfirmClose::Always => true,
    }
}

pub(crate) fn quit_title(language: &str) -> &'static str {
    if language.starts_with("zh") {
        "退出 ntty7？"
    } else if language.starts_with("ja") {
        "ntty7 を終了しますか？"
    } else if language.starts_with("ru") {
        "Выйти из ntty7?"
    } else {
        "Quit ntty7?"
    }
}

fn quit_body(busy: Option<&PaneBusy>) -> String {
    match busy {
        Some(PaneBusy::Command(what)) => t_fmt(L10nKey::CloseBusyCommandBody, &[("what", what)]),
        Some(PaneBusy::Agent(agent)) => t_fmt(L10nKey::CloseBusyAgentBody, &[("agent", agent)]),
        None => t(L10nKey::CloseIdleBody).to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::{QuitRoute, needs_confirm, quit_route, quit_title};
    use crate::core::config::ConfirmClose;

    #[test]
    fn confirm_follows_the_policy_and_whether_the_shell_is_busy() {
        assert!(!needs_confirm(ConfirmClose::Never, true));
        assert!(!needs_confirm(ConfirmClose::Never, false));
        assert!(needs_confirm(ConfirmClose::WhenBusy, true));
        assert!(!needs_confirm(ConfirmClose::WhenBusy, false));
        assert!(needs_confirm(ConfirmClose::Always, true));
        assert!(needs_confirm(ConfirmClose::Always, false));
    }

    #[test]
    fn quit_asks_the_live_window_and_leaves_an_open_prompt_alone() {
        assert_eq!(quit_route(false, false), QuitRoute::ExitNow);
        assert_eq!(quit_route(false, true), QuitRoute::ExitNow);
        assert_eq!(quit_route(true, true), QuitRoute::LeavePrompt);
        assert_eq!(quit_route(true, false), QuitRoute::Confirm);
    }

    #[test]
    fn the_quit_question_follows_the_gui_language() {
        assert_eq!(quit_title("en"), "Quit ntty7?");
        assert_eq!(quit_title("zh-CN"), "退出 ntty7？");
        assert_eq!(quit_title("ja"), "ntty7 を終了しますか？");
        assert_eq!(quit_title("ru"), "Выйти из ntty7?");
        assert_eq!(quit_title(""), "Quit ntty7?");
    }
}
