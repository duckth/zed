#[cfg(test)]
mod session_switcher_tests;

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use collections::HashSet;
use fuzzy_nucleo::StringMatchCandidate;
use gpui::{
    Action, AnyElement, AnyWindowHandle, App, AppContext as _, Context, DismissEvent, Entity,
    EntityId, EventEmitter, FocusHandle, Focusable, Global, Render, Subscription, Task,
    TaskExt as _, WeakEntity, Window, WindowHandle, actions, rems,
};
use picker::{Picker, PickerDelegate};
use schemars::JsonSchema;
use serde::Deserialize;
use settings::{RegisterSetting, Settings, SettingsContent, SettingsStore};
use terminal::{ProgramState, TaskStatus};
use terminal_view::{TerminalView, terminal_panel::TerminalPanel};
use ui::{ListItem, ListItemSpacing, Tooltip, prelude::*};
use util::{ResultExt as _, paths::PathExt as _};
use workspace::{
    HideStatusItem, ItemHandle, ModalView, MultiWorkspace, OpenMode, Pane, ProjectGroupKey,
    RecentWorkspace, SerializedWorkspaceLocation, StatusItemView, Workspace, WorkspaceDb, ZoomIn,
    dock::Panel as _, item::Item as _,
};

const MODAL_WIDTH_REMS: f32 = 34.;

#[derive(Clone, Copy, Debug, RegisterSetting)]
pub struct SessionSwitcherSettings {
    pub show_attention_count: bool,
}

impl Settings for SessionSwitcherSettings {
    fn from_settings(content: &SettingsContent) -> Self {
        Self {
            show_attention_count: content
                .session_switcher
                .as_ref()
                .and_then(|session_switcher| session_switcher.show_attention_count)
                .unwrap_or(true),
        }
    }
}

/// Opens the session switcher, listing open sessions with their editor and terminal views,
/// followed by recent projects that are not open.
#[derive(PartialEq, Clone, Deserialize, JsonSchema, Default, Action)]
#[action(namespace = session_switcher)]
#[serde(deny_unknown_fields)]
pub struct Toggle {
    /// List only sessions, without their editor and terminal views.
    #[serde(default)]
    pub sessions_only: bool,
    /// List only the editor and terminal views of the current session.
    #[serde(default)]
    pub current_session_only: bool,
}

/// Activates a view of the current session by index: 0 is the editor, and 1 and up are its
/// terminals in terminal panel order.
#[derive(PartialEq, Clone, Debug, Deserialize, JsonSchema, Default, Action)]
#[action(namespace = session_switcher)]
pub struct ActivateView(pub usize);

actions!(
    session_switcher,
    [
        /// Switches to the most recently used other session.
        ToggleLastSession,
        /// Toggles between the editor and the last used terminal of the current session.
        ToggleLastView,
        /// Activates the next view (editor or terminal) of the current session.
        NextView,
        /// Activates the previous view (editor or terminal) of the current session.
        PreviousView,
        /// Activates the next terminal, in any session, whose program needs attention: it is
        /// waiting on you, failed, or finished while you were elsewhere.
        JumpToNextAttention,
    ]
);

pub fn init(cx: &mut App) {
    let attention_tracker = cx.new(|_| AttentionTracker::default());
    cx.set_global(GlobalAttentionTracker(attention_tracker));
    cx.observe_new(register_actions).detach();
    cx.observe_new(track_terminal_attention).detach();
}

fn register_actions(
    workspace: &mut Workspace,
    window: Option<&mut Window>,
    cx: &mut Context<Workspace>,
) {
    if let Some(window) = window {
        let attention_indicator = cx.new(AttentionIndicator::new);
        workspace.status_bar().update(cx, |status_bar, cx| {
            status_bar.add_right_item(attention_indicator, window, cx);
        });
    }
    workspace.register_action(|workspace, action: &Toggle, window, cx| {
        let mode = if action.current_session_only {
            Mode::CurrentSession
        } else if action.sessions_only {
            Mode::SessionsOnly
        } else {
            Mode::All
        };
        SessionSwitcher::toggle(workspace, mode, window, cx);
    });
    workspace.register_action(|_workspace, _: &ToggleLastSession, window, cx| {
        // Sessions are read from every workspace, including this one, which stays
        // borrowed for as long as its action handler runs.
        let current_window = window.window_handle();
        cx.defer(move |cx| toggle_last_session(current_window, cx));
    });
    workspace.register_action(|workspace, _: &ToggleLastView, window, cx| {
        toggle_last_view(workspace, window, cx);
    });
    workspace.register_action(|workspace, _: &NextView, window, cx| {
        cycle_view(workspace, true, window, cx);
    });
    workspace.register_action(|workspace, _: &PreviousView, window, cx| {
        cycle_view(workspace, false, window, cx);
    });
    workspace.register_action(|workspace, action: &ActivateView, window, cx| {
        let views = session_views(workspace, cx);
        if let Some(view) = views.get(action.0) {
            activate_view(workspace, view, window, cx);
        }
    });
    workspace.register_action(|workspace, _: &JumpToNextAttention, window, cx| {
        let views = session_views(workspace, cx);
        let focused_terminal = match views.get(focused_view_index(&views, window, cx)) {
            Some(SessionView::Terminal { terminal_view, .. }) => Some(terminal_view.entity_id()),
            Some(SessionView::Editor) | None => None,
        };
        // Like `ToggleLastSession`, this reads every workspace, including this borrowed one.
        cx.defer(move |cx| jump_to_next_attention(focused_terminal, cx));
    });
}

/// Remembers terminals whose program reported `done` while they were not focused, so they keep
/// needing attention until they are looked at. Notifies whenever any terminal's attention may
/// have changed.
#[derive(Default)]
struct AttentionTracker {
    unseen_done: HashSet<EntityId>,
}

struct GlobalAttentionTracker(Entity<AttentionTracker>);

impl Global for GlobalAttentionTracker {}

impl AttentionTracker {
    fn global(cx: &App) -> Option<Entity<Self>> {
        cx.try_global::<GlobalAttentionTracker>()
            .map(|tracker| tracker.0.clone())
    }
}

fn track_terminal_attention(
    terminal_view: &mut TerminalView,
    window: Option<&mut Window>,
    cx: &mut Context<TerminalView>,
) {
    let (Some(window), Some(attention_tracker)) = (window, AttentionTracker::global(cx)) else {
        return;
    };
    let terminal_view_id = cx.entity_id();

    cx.subscribe_in(terminal_view.terminal(), window, {
        let attention_tracker = attention_tracker.clone();
        move |terminal_view, terminal, event, window, cx| {
            if *event != terminal::Event::ProgramStateChanged {
                return;
            }
            let unseen_done = terminal.read(cx).program_state() == Some(ProgramState::Done)
                && !terminal_view.focus_handle(cx).contains_focused(window, cx);
            attention_tracker.update(cx, |tracker, cx| {
                if unseen_done {
                    tracker.unseen_done.insert(terminal_view_id);
                } else {
                    tracker.unseen_done.remove(&terminal_view_id);
                }
                cx.notify();
            });
        }
    })
    .detach();

    let focus_handle = terminal_view.focus_handle(cx);
    cx.on_focus_in(&focus_handle, window, {
        let attention_tracker = attention_tracker.clone();
        move |_, _, cx| {
            attention_tracker.update(cx, |tracker, cx| {
                if tracker.unseen_done.remove(&terminal_view_id) {
                    cx.notify();
                }
            });
        }
    })
    .detach();

    cx.on_release(move |_, cx| {
        attention_tracker.update(cx, |tracker, cx| {
            tracker.unseen_done.remove(&terminal_view_id);
            cx.notify();
        });
    })
    .detach();
}

fn needs_attention(terminal_view: &Entity<TerminalView>, cx: &App) -> bool {
    match terminal_view.read(cx).terminal().read(cx).program_state() {
        Some(ProgramState::Blocked | ProgramState::Error) => true,
        Some(ProgramState::Done) => AttentionTracker::global(cx).is_some_and(|tracker| {
            tracker
                .read(cx)
                .unseen_done
                .contains(&terminal_view.entity_id())
        }),
        Some(ProgramState::Working | ProgramState::Idle) | None => false,
    }
}

fn attention_count(workspace: &Workspace, cx: &App) -> usize {
    session_terminals(workspace, cx)
        .iter()
        .filter(|view| match view {
            SessionView::Terminal { terminal_view, .. } => needs_attention(terminal_view, cx),
            SessionView::Editor => false,
        })
        .count()
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    All,
    SessionsOnly,
    CurrentSession,
}

#[derive(Clone)]
enum SessionView {
    Editor,
    Terminal {
        pane: Entity<Pane>,
        terminal_view: Entity<TerminalView>,
        in_panel: bool,
    },
}

#[derive(Clone)]
struct Session {
    window: WindowHandle<MultiWorkspace>,
    workspace: Entity<Workspace>,
    is_current: bool,
}

/// Terminals in the terminal panel come first, in panel order, followed by terminals opened in
/// the center panes.
fn session_terminals(workspace: &Workspace, cx: &App) -> Vec<SessionView> {
    let panel_panes = workspace
        .panel::<TerminalPanel>(cx)
        .map(|panel| {
            panel
                .read(cx)
                .panes()
                .into_iter()
                .cloned()
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let panes = panel_panes
        .into_iter()
        .map(|pane| (pane, true))
        .chain(workspace.panes().iter().map(|pane| (pane.clone(), false)));

    let mut terminals = Vec::new();
    for (pane, in_panel) in panes {
        for terminal_view in pane.read(cx).items_of_type::<TerminalView>() {
            terminals.push(SessionView::Terminal {
                pane: pane.clone(),
                terminal_view,
                in_panel,
            });
        }
    }
    terminals
}

fn session_views(workspace: &Workspace, cx: &App) -> Vec<SessionView> {
    let mut views = vec![SessionView::Editor];
    views.extend(session_terminals(workspace, cx));
    views
}

/// Index into `views` of the focused terminal, or 0 (the editor) when no terminal is focused.
fn focused_view_index(views: &[SessionView], window: &Window, cx: &App) -> usize {
    views
        .iter()
        .position(|view| match view {
            SessionView::Editor => false,
            SessionView::Terminal { terminal_view, .. } => terminal_view
                .read(cx)
                .focus_handle(cx)
                .contains_focused(window, cx),
        })
        .unwrap_or(0)
}

fn activate_view(
    workspace: &mut Workspace,
    view: &SessionView,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    match view {
        SessionView::Editor => {
            workspace.close_panel::<TerminalPanel>(window, cx);
            let pane = workspace.active_pane().clone();
            window.focus(&pane.read(cx).focus_handle(cx), cx);
        }
        SessionView::Terminal {
            pane,
            terminal_view,
            in_panel,
        } => {
            let in_panel = *in_panel;
            if in_panel {
                workspace.open_panel::<TerminalPanel>(window, cx);
            }
            pane.update(cx, |pane, cx| {
                if let Some(index) = pane.index_for_item(terminal_view) {
                    pane.activate_item(index, true, true, window, cx);
                }
                if in_panel && !pane.is_zoomed() {
                    pane.zoom_in(&ZoomIn, window, cx);
                }
            });
        }
    }
}

/// The terminal panel keeps its last active terminal, which serves as the "last view" when
/// leaving the editor, so no separate view history is kept.
fn last_panel_terminal(workspace: &Workspace, cx: &App) -> Option<SessionView> {
    let pane = workspace.panel::<TerminalPanel>(cx)?.read(cx).pane()?;
    let terminal_view = pane.read(cx).active_item()?.downcast::<TerminalView>()?;
    Some(SessionView::Terminal {
        pane,
        terminal_view,
        in_panel: true,
    })
}

fn toggle_last_view(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let views = session_views(workspace, cx);
    if focused_view_index(&views, window, cx) != 0 {
        activate_view(workspace, &SessionView::Editor, window, cx);
        return;
    }
    match last_panel_terminal(workspace, cx) {
        Some(view) => activate_view(workspace, &view, window, cx),
        // Focusing an empty terminal panel spawns a terminal in it.
        None => {
            workspace.focus_panel::<TerminalPanel>(window, cx);
        }
    }
}

fn cycle_view(
    workspace: &mut Workspace,
    forward: bool,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let views = session_views(workspace, cx);
    let current = focused_view_index(&views, window, cx);
    let target = if forward {
        (current + 1) % views.len()
    } else {
        (current + views.len() - 1) % views.len()
    };
    if let Some(view) = views.get(target) {
        activate_view(workspace, view, window, cx);
    }
}

/// Sessions of all windows, most recently used first. Must not be called while any of the
/// workspaces is being updated.
fn sessions_by_recency(current_window: AnyWindowHandle, cx: &App) -> Vec<Session> {
    // The platform window order is used where available, but it may be missing or omit windows
    // (e.g. minimized ones), which are then appended.
    let mut windows: Vec<WindowHandle<MultiWorkspace>> = Vec::new();
    for window in cx
        .window_stack()
        .unwrap_or_default()
        .into_iter()
        .chain(cx.windows())
    {
        if let Some(window) = window.downcast::<MultiWorkspace>()
            && !windows
                .iter()
                .any(|known| known.window_id() == window.window_id())
        {
            windows.push(window);
        }
    }
    // List the invoking window first so its sessions rank ahead of other windows', even when
    // the platform reports no window order.
    if let Some(index) = windows
        .iter()
        .position(|window| window.window_id() == current_window.window_id())
    {
        let window = windows.remove(index);
        windows.insert(0, window);
    }

    let mut sessions = Vec::new();
    for window in windows {
        let Ok(multi_workspace) = window.read(cx) else {
            continue;
        };
        let displayed = multi_workspace.workspace().clone();
        let is_current_window = window.window_id() == current_window.window_id();
        for workspace in multi_workspace.workspaces_by_recency() {
            sessions.push(Session {
                window,
                is_current: is_current_window && workspace == displayed,
                workspace,
            });
        }
    }
    sessions
}

/// Terminals of all sessions in a stable order (windows, then sessions as held, then panel
/// order), so that repeated jumps visit every terminal needing attention in turn. Must not be
/// called while any of the workspaces is being updated.
fn all_terminals(cx: &App) -> Vec<(Session, SessionView)> {
    let mut terminals = Vec::new();
    for window in cx.windows() {
        let Some(window) = window.downcast::<MultiWorkspace>() else {
            continue;
        };
        let Ok(multi_workspace) = window.read(cx) else {
            continue;
        };
        for workspace in multi_workspace.workspaces() {
            let session = Session {
                window,
                workspace: workspace.clone(),
                is_current: false,
            };
            for view in session_terminals(workspace.read(cx), cx) {
                terminals.push((session.clone(), view));
            }
        }
    }
    terminals
}

fn jump_to_next_attention(focused_terminal: Option<EntityId>, cx: &mut App) {
    let terminals = all_terminals(cx);
    let is_focused = |view: &SessionView| match view {
        SessionView::Terminal { terminal_view, .. } => {
            Some(terminal_view.entity_id()) == focused_terminal
        }
        SessionView::Editor => false,
    };
    let start = terminals
        .iter()
        .position(|(_, view)| is_focused(view))
        .map_or(0, |index| index + 1);
    let target = terminals
        .iter()
        .cycle()
        .skip(start)
        .take(terminals.len())
        .find(|(_, view)| {
            !is_focused(view)
                && matches!(view, SessionView::Terminal { terminal_view, .. } if needs_attention(terminal_view, cx))
        })
        .cloned();
    if let Some((session, view)) = target {
        activate_session(&session, Some(view), cx);
    }
}

fn toggle_last_session(current_window: AnyWindowHandle, cx: &mut App) {
    let sessions = sessions_by_recency(current_window, cx);
    if let Some(session) = sessions.iter().find(|session| !session.is_current) {
        activate_session(session, None, cx);
    }
}

fn activate_session(session: &Session, view: Option<SessionView>, cx: &mut App) {
    let workspace = session.workspace.clone();
    session
        .window
        .update(cx, |multi_workspace, window, cx| {
            window.activate_window();
            multi_workspace.activate(workspace.clone(), None, window, cx);
            if let Some(view) = view {
                workspace.update(cx, |workspace, cx| {
                    activate_view(workspace, &view, window, cx)
                });
            }
        })
        .log_err();
}

fn open_recent_project(current_window: AnyWindowHandle, paths: Vec<PathBuf>, cx: &mut App) {
    let Some(window) = current_window.downcast::<MultiWorkspace>() else {
        return;
    };
    window
        .update(cx, |multi_workspace, window, cx| {
            multi_workspace
                .open_project(paths, OpenMode::Activate, window, cx)
                .detach_and_log_err(cx);
        })
        .log_err();
}

fn session_name(workspace: &Entity<Workspace>, cx: &App) -> SharedString {
    let project = workspace.read(cx).project().read(cx);
    let names = project
        .visible_worktrees(cx)
        .map(|worktree| worktree.read(cx).root_name_str().to_string())
        .collect::<Vec<_>>();
    if names.is_empty() {
        "Empty Workspace".into()
    } else {
        names.join(", ").into()
    }
}

fn session_branch(workspace: &Entity<Workspace>, cx: &App) -> Option<SharedString> {
    let repository = workspace
        .read(cx)
        .project()
        .read(cx)
        .active_repository(cx)?;
    let branch = repository.read(cx).branch.as_ref()?.name().to_string();
    Some(branch.into())
}

fn view_title(view: &SessionView, cx: &App) -> SharedString {
    match view {
        SessionView::Editor => "editor".into(),
        SessionView::Terminal { terminal_view, .. } => {
            terminal_view.read(cx).tab_content_text(0, cx)
        }
    }
}

/// Like `view_title`, but without the working directory name that Zed prefixes to shell
/// titles, since rows show the working directory next to the title already.
fn compact_view_title(view: &SessionView, cx: &App) -> SharedString {
    let SessionView::Terminal { terminal_view, .. } = view else {
        return view_title(view, cx);
    };
    let terminal_view = terminal_view.read(cx);
    let terminal = terminal_view.terminal().read(cx);
    let has_custom_title = terminal_view
        .custom_title()
        .is_some_and(|title| !title.trim().is_empty());
    if has_custom_title || terminal.task().is_some() {
        return view_title(view, cx);
    }
    let title = terminal.title(false);
    let working_directory = terminal.working_directory();
    match strip_directory_prefix(&title, working_directory.as_deref()) {
        Some(process) => process.to_string().into(),
        None => view_title(view, cx),
    }
}

/// `Terminal::title` composes shell titles as `<working directory name> — <process>`, with
/// the directory taken from the same process info as `Terminal::working_directory`. Returns
/// the process part when `title` has exactly that prefix.
fn strip_directory_prefix<'a>(title: &'a str, working_directory: Option<&Path>) -> Option<&'a str> {
    let directory_name = working_directory?.file_name()?.to_string_lossy();
    title
        .strip_prefix(directory_name.as_ref())?
        .strip_prefix(" — ")
        .filter(|process| !process.is_empty())
}

fn view_detail(session: &Session, view: &SessionView, cx: &App) -> Option<SharedString> {
    match view {
        SessionView::Editor => {
            let item = session
                .workspace
                .read(cx)
                .active_pane()
                .read(cx)
                .active_item()?;
            Some(item.tab_content_text(0, cx))
        }
        SessionView::Terminal { terminal_view, .. } => {
            let working_directory = terminal_view
                .read(cx)
                .terminal()
                .read(cx)
                .working_directory()?;
            Some(
                working_directory
                    .compact()
                    .to_string_lossy()
                    .into_owned()
                    .into(),
            )
        }
    }
}

fn render_terminal_status(terminal_view: &Entity<TerminalView>, cx: &App) -> Option<AnyElement> {
    let program_state = terminal_view.read(cx).terminal().read(cx).program_state();
    let (icon, color, label) = if let Some(program_state) = program_state {
        match program_state {
            ProgramState::Working => (IconName::ArrowCircle, Color::Accent, "working"),
            ProgramState::Blocked => (IconName::Warning, Color::Warning, "waiting"),
            ProgramState::Done if needs_attention(terminal_view, cx) => {
                (IconName::Check, Color::Success, "done")
            }
            ProgramState::Done => (IconName::Check, Color::Muted, "done"),
            ProgramState::Idle => (IconName::Circle, Color::Muted, "idle"),
            ProgramState::Error => (IconName::XCircle, Color::Error, "error"),
        }
    } else if terminal_view.read(cx).has_bell() {
        (IconName::Bell, Color::Warning, "bell")
    } else {
        match terminal_view.read(cx).terminal().read(cx).task()?.status {
            TaskStatus::Running => (IconName::PlayFilled, Color::Accent, "running"),
            TaskStatus::Completed { success: true } => (IconName::Check, Color::Success, "done"),
            TaskStatus::Completed { success: false } => (IconName::XCircle, Color::Error, "failed"),
            TaskStatus::Unknown => return None,
        }
    };
    Some(
        h_flex()
            .gap_1()
            .child(Icon::new(icon).color(color).size(IconSize::Small))
            .child(Label::new(label).color(color).size(LabelSize::Small))
            .into_any_element(),
    )
}

fn render_attention_count(count: usize) -> impl IntoElement {
    h_flex()
        .gap_1()
        .child(
            Icon::new(IconName::Warning)
                .color(Color::Warning)
                .size(IconSize::Small),
        )
        .child(
            Label::new(count.to_string())
                .color(Color::Warning)
                .size(LabelSize::Small),
        )
}

fn row_label(title: SharedString, detail: Option<SharedString>) -> impl IntoElement {
    h_flex()
        .gap_2()
        .min_w_0()
        .overflow_hidden()
        .child(Label::new(title).truncate())
        .when_some(detail, |this, detail| {
            this.child(
                Label::new(detail)
                    .color(Color::Muted)
                    .size(LabelSize::Small)
                    .truncate(),
            )
        })
}

pub struct SessionSwitcher {
    picker: Entity<Picker<SessionSwitcherDelegate>>,
    _load_recent_projects: Task<()>,
    _observe_attention: Option<Subscription>,
}

impl SessionSwitcher {
    fn toggle(
        workspace: &mut Workspace,
        mode: Mode,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        // Focus moves into the switcher once it opens, so the view to preselect is determined
        // now. It mirrors `ToggleLastView`: the editor when a terminal is focused, otherwise
        // the last used terminal.
        let views = session_views(workspace, cx);
        let preselected_terminal = if focused_view_index(&views, window, cx) == 0 {
            match last_panel_terminal(workspace, cx) {
                Some(SessionView::Terminal { terminal_view, .. }) => {
                    Some(terminal_view.entity_id())
                }
                Some(SessionView::Editor) | None => None,
            }
        } else {
            None
        };
        let current_window = window.window_handle();
        let fs = workspace.app_state().fs.clone();
        let db = WorkspaceDb::global(cx);

        workspace.toggle_modal(window, cx, |window, cx| {
            let delegate = SessionSwitcherDelegate {
                session_switcher: cx.entity().downgrade(),
                current_window,
                mode,
                preselected_terminal,
                recent_workspaces: Vec::new(),
                rows: Vec::new(),
                selected_index: 0,
                has_query: false,
            };
            let picker = cx
                .new(|cx| Picker::list(delegate, window, cx).initial_width(rems(MODAL_WIDTH_REMS)));

            let load_recent_projects = if mode == Mode::CurrentSession {
                Task::ready(())
            } else {
                let picker = picker.downgrade();
                cx.spawn_in(window, async move |_, cx| {
                    let Some(recent_workspaces) =
                        db.recent_project_workspaces(fs.as_ref()).await.log_err()
                    else {
                        return;
                    };
                    picker
                        .update_in(cx, |picker, window, cx| {
                            picker.delegate.recent_workspaces = recent_workspaces;
                            let query = picker.query(cx);
                            picker.update_matches(query, window, cx);
                        })
                        .log_err();
                })
            };

            let observe_attention = AttentionTracker::global(cx).map(|attention_tracker| {
                cx.observe(&attention_tracker, |this, _, cx| {
                    this.picker.update(cx, |_, cx| cx.notify());
                })
            });

            Self {
                picker,
                _load_recent_projects: load_recent_projects,
                _observe_attention: observe_attention,
            }
        });
    }
}

impl ModalView for SessionSwitcher {}

impl EventEmitter<DismissEvent> for SessionSwitcher {}

impl Focusable for SessionSwitcher {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.picker.focus_handle(cx)
    }
}

impl Render for SessionSwitcher {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .key_context("SessionSwitcher")
            .w(rems(MODAL_WIDTH_REMS))
            .child(self.picker.clone())
    }
}

#[derive(Clone)]
enum Row {
    Session(Session),
    View {
        session: Session,
        view: SessionView,
        /// The view's index for `ActivateView`.
        index: usize,
    },
    Recent {
        name: SharedString,
        location: SharedString,
        paths: Vec<PathBuf>,
    },
}

impl Row {
    fn search_text(&self, cx: &App) -> String {
        match self {
            Row::Session(session) => {
                let name = session_name(&session.workspace, cx);
                match session_branch(&session.workspace, cx) {
                    Some(branch) => format!("{name} {branch}"),
                    None => name.to_string(),
                }
            }
            Row::View { session, view, .. } => format!(
                "{} {}",
                session_name(&session.workspace, cx),
                view_title(view, cx)
            ),
            Row::Recent { name, location, .. } => format!("{name} {location}"),
        }
    }
}

pub struct SessionSwitcherDelegate {
    session_switcher: WeakEntity<SessionSwitcher>,
    current_window: AnyWindowHandle,
    mode: Mode,
    preselected_terminal: Option<EntityId>,
    recent_workspaces: Vec<RecentWorkspace>,
    rows: Vec<Row>,
    selected_index: usize,
    has_query: bool,
}

impl SessionSwitcherDelegate {
    fn all_rows(&self, cx: &App) -> Vec<Row> {
        let sessions = sessions_by_recency(self.current_window, cx);
        let mut rows = Vec::new();
        for session in &sessions {
            let include_session = match self.mode {
                Mode::All | Mode::SessionsOnly => true,
                Mode::CurrentSession => false,
            };
            let include_views = match self.mode {
                Mode::All => true,
                Mode::SessionsOnly => false,
                Mode::CurrentSession => session.is_current,
            };
            if include_session {
                rows.push(Row::Session(session.clone()));
            }
            if include_views {
                rows.extend(
                    session_views(session.workspace.read(cx), cx)
                        .into_iter()
                        .enumerate()
                        .map(|(index, view)| Row::View {
                            session: session.clone(),
                            view,
                            index,
                        }),
                );
            }
        }

        if self.mode != Mode::CurrentSession {
            let open_projects: HashSet<ProjectGroupKey> = sessions
                .iter()
                .map(|session| session.workspace.read(cx).project_group_key(cx))
                .collect();
            for recent in &self.recent_workspaces {
                if !matches!(recent.location, SerializedWorkspaceLocation::Local) {
                    continue;
                }
                let key = recent.project_group_key();
                if open_projects.contains(&key) {
                    continue;
                }
                let location = recent
                    .paths
                    .paths()
                    .iter()
                    .map(|path| path.compact().to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
                    .join(", ");
                rows.push(Row::Recent {
                    name: key.display_name(&std::collections::HashMap::default()),
                    location: location.into(),
                    paths: recent.paths.paths().to_vec(),
                });
            }
        }
        rows
    }

    fn update_rows(&mut self, query: &str, cx: &App) {
        let rows = self.all_rows(cx);
        self.has_query = !query.is_empty();
        if query.is_empty() {
            self.selected_index = self.default_selection(&rows);
            self.rows = rows;
            return;
        }

        let candidates = rows
            .iter()
            .enumerate()
            .map(|(index, row)| StringMatchCandidate::new(index, &row.search_text(cx)))
            .collect::<Vec<_>>();
        let matches = fuzzy_nucleo::match_strings(
            &candidates,
            query,
            fuzzy_nucleo::Case::Smart,
            fuzzy_nucleo::LengthPenalty::On,
            candidates.len(),
        );
        self.rows = matches
            .into_iter()
            .filter_map(|string_match| rows.get(string_match.candidate_id).cloned())
            .collect();
        self.selected_index = 0;
    }

    /// Preselects what the "toggle last" actions would switch to, so confirming right away
    /// behaves like them.
    fn default_selection(&self, rows: &[Row]) -> usize {
        let position = match self.mode {
            Mode::All | Mode::SessionsOnly => rows
                .iter()
                .position(|row| matches!(row, Row::Session(session) if !session.is_current)),
            Mode::CurrentSession => rows.iter().position(|row| match row {
                Row::View {
                    view: SessionView::Editor,
                    ..
                } => self.preselected_terminal.is_none(),
                Row::View {
                    view: SessionView::Terminal { terminal_view, .. },
                    ..
                } => self.preselected_terminal == Some(terminal_view.entity_id()),
                Row::Session(_) | Row::Recent { .. } => false,
            }),
        };
        position.unwrap_or(0)
    }

    /// Nested rows sit under their session and next to their working directory, so they use
    /// the compact title. Filtered rows stand alone and keep the full title.
    fn display_title(&self, view: &SessionView, cx: &App) -> SharedString {
        if self.has_query {
            view_title(view, cx)
        } else {
            compact_view_title(view, cx)
        }
    }
}

impl PickerDelegate for SessionSwitcherDelegate {
    type ListItem = ListItem;

    fn name() -> &'static str {
        "session switcher"
    }

    fn placeholder_text(&self, _window: &mut Window, _cx: &mut App) -> Arc<str> {
        match self.mode {
            Mode::CurrentSession => "Switch to the editor or a terminal…".into(),
            Mode::All | Mode::SessionsOnly => "Switch session…".into(),
        }
    }

    fn no_matches_text(&self, _window: &mut Window, _cx: &mut App) -> Option<SharedString> {
        Some("No sessions".into())
    }

    fn match_count(&self) -> usize {
        self.rows.len()
    }

    fn selected_index(&self) -> usize {
        self.selected_index
    }

    fn select_on_hover(&self) -> bool {
        false
    }

    fn set_selected_index(
        &mut self,
        ix: usize,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) {
        self.selected_index = ix;
    }

    fn separators_after_indices(&self) -> Vec<usize> {
        if self.has_query {
            return Vec::new();
        }
        match self
            .rows
            .iter()
            .position(|row| matches!(row, Row::Recent { .. }))
        {
            Some(first_recent) if first_recent > 0 => vec![first_recent - 1],
            _ => Vec::new(),
        }
    }

    fn update_matches(
        &mut self,
        query: String,
        _window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        // Rows are read from every session's workspace, and the workspace that opens this
        // picker is still being updated while the picker is constructed. This is deferred at
        // the app level rather than the window level, because a window-level deferral holds
        // the window and its `MultiWorkspace` could not be read.
        let picker = cx.entity();
        cx.defer(move |cx| {
            picker.update(cx, |picker, cx| {
                picker.delegate.update_rows(&query, cx);
                cx.notify();
            });
        });
        Task::ready(())
    }

    fn confirm(&mut self, _secondary: bool, _window: &mut Window, cx: &mut Context<Picker<Self>>) {
        let Some(row) = self.rows.get(self.selected_index).cloned() else {
            return;
        };
        // Dismiss first: closing the modal restores focus to where it was opened from, which
        // would otherwise override the focus set by switching.
        self.session_switcher
            .update(cx, |_, cx| cx.emit(DismissEvent))
            .log_err();
        let current_window = self.current_window;
        cx.defer(move |cx| match row {
            Row::Session(session) => activate_session(&session, None, cx),
            Row::View { session, view, .. } => activate_session(&session, Some(view), cx),
            Row::Recent { paths, .. } => open_recent_project(current_window, paths, cx),
        });
    }

    fn dismissed(&mut self, _window: &mut Window, cx: &mut Context<Picker<Self>>) {
        self.session_switcher
            .update(cx, |_, cx| cx.emit(DismissEvent))
            .log_err();
    }

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        _window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let row = self.rows.get(ix)?;
        let item = ListItem::new(ix)
            .spacing(ListItemSpacing::Sparse)
            .inset(true)
            .toggle_state(selected);

        let item = match row {
            Row::Session(session) => {
                let terminal_count = session_terminals(session.workspace.read(cx), cx).len();
                let mut details = Vec::new();
                if let Some(branch) = session_branch(&session.workspace, cx) {
                    details.push(branch.to_string());
                }
                details.push(match terminal_count {
                    1 => "1 terminal".to_string(),
                    count => format!("{count} terminals"),
                });
                let attention_count = attention_count(session.workspace.read(cx), cx);
                item.start_slot(Icon::new(IconName::FolderOpen).color(Color::Muted))
                    .child(row_label(
                        session_name(&session.workspace, cx),
                        Some(details.join(" · ").into()),
                    ))
                    .end_slot(
                        h_flex()
                            .gap_2()
                            .when(attention_count > 0, |this| {
                                this.child(render_attention_count(attention_count))
                            })
                            .when(session.is_current, |this| {
                                this.child(
                                    Label::new("current")
                                        .color(Color::Muted)
                                        .size(LabelSize::Small),
                                )
                            }),
                    )
            }
            Row::View {
                session,
                view,
                index,
            } => {
                let icon = match view {
                    SessionView::Editor => IconName::FileCode,
                    SessionView::Terminal { .. } => IconName::Terminal,
                };
                let detail = view_detail(session, view, cx);
                // Filtered rows are ranked by score rather than grouped under their session,
                // so they name it themselves.
                let detail = if self.has_query {
                    let name = session_name(&session.workspace, cx);
                    Some(match detail {
                        Some(detail) => format!("{name} · {detail}").into(),
                        None => name,
                    })
                } else {
                    detail
                };
                let indent_level = if self.has_query || self.mode == Mode::CurrentSession {
                    0
                } else {
                    1
                };
                let status = match view {
                    SessionView::Editor => None,
                    SessionView::Terminal { terminal_view, .. } => {
                        render_terminal_status(terminal_view, cx)
                    }
                };
                item.indent_level(indent_level)
                    .start_slot(Icon::new(icon).color(Color::Muted))
                    .child(
                        h_flex()
                            .gap_2()
                            .min_w_0()
                            .overflow_hidden()
                            .child(
                                Label::new(index.to_string())
                                    .color(Color::Muted)
                                    .size(LabelSize::Small),
                            )
                            .child(row_label(self.display_title(view, cx), detail)),
                    )
                    .end_slot::<AnyElement>(status)
            }
            Row::Recent { name, location, .. } => item
                .start_slot(Icon::new(IconName::Folder).color(Color::Muted))
                .child(row_label(name.clone(), Some(location.clone()))),
        };
        Some(item)
    }
}

/// Status bar count of terminals needing attention across all sessions, hidden when there are
/// none. Clicking it jumps to the next one.
struct AttentionIndicator {
    _observe_attention: Option<Subscription>,
    _observe_settings: Subscription,
}

impl AttentionIndicator {
    fn new(cx: &mut Context<Self>) -> Self {
        Self {
            _observe_attention: AttentionTracker::global(cx)
                .map(|attention_tracker| cx.observe(&attention_tracker, |_, _, cx| cx.notify())),
            _observe_settings: cx.observe_global::<SettingsStore>(|_, cx| cx.notify()),
        }
    }

    /// Counts across all windows. The window being rendered is borrowed, so its
    /// `MultiWorkspace` is read through `window` rather than through its handle.
    fn total_attention_count(window: &Window, cx: &App) -> usize {
        let current_window_id = window.window_handle().window_id();
        let mut multi_workspaces = Vec::new();
        for handle in cx.windows() {
            if handle.window_id() == current_window_id {
                if let Some(Some(multi_workspace)) = window.root::<MultiWorkspace>() {
                    multi_workspaces.push(multi_workspace);
                }
            } else if let Some(handle) = handle.downcast::<MultiWorkspace>()
                && let Ok(multi_workspace) = handle.entity(cx)
            {
                multi_workspaces.push(multi_workspace);
            }
        }
        multi_workspaces
            .iter()
            .flat_map(|multi_workspace| multi_workspace.read(cx).workspaces())
            .map(|workspace| attention_count(workspace.read(cx), cx))
            .sum()
    }
}

impl Render for AttentionIndicator {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let count = if SessionSwitcherSettings::get_global(cx).show_attention_count {
            Self::total_attention_count(window, cx)
        } else {
            0
        };
        if count == 0 {
            return div().into_any_element();
        }
        let button = Button::new("session-switcher-attention-count", count.to_string())
            .start_icon(
                Icon::new(IconName::Warning)
                    .color(Color::Warning)
                    .size(IconSize::Small),
            )
            .label_size(LabelSize::Small)
            .color(Color::Warning)
            .tooltip(Tooltip::for_action_title(
                "Jump to Terminal Needing Attention",
                &JumpToNextAttention,
            ))
            .on_click(|_, window, cx| {
                window.dispatch_action(JumpToNextAttention.boxed_clone(), cx);
            });
        div()
            .debug_selector(|| "session-switcher-attention-count".into())
            .child(button)
            .into_any_element()
    }
}

impl StatusItemView for AttentionIndicator {
    fn set_active_pane_item(
        &mut self,
        _active_pane_item: Option<&dyn ItemHandle>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
    }

    fn hide_setting(&self, _: &App) -> Option<HideStatusItem> {
        Some(HideStatusItem::new(|settings| {
            settings
                .session_switcher
                .get_or_insert_default()
                .show_attention_count = Some(false);
        }))
    }
}
