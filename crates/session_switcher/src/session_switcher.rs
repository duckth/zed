#[cfg(test)]
mod session_switcher_tests;

use std::{path::PathBuf, sync::Arc};

use collections::HashSet;
use fuzzy_nucleo::StringMatchCandidate;
use gpui::{
    Action, AnyElement, AnyWindowHandle, App, Context, DismissEvent, Entity, EntityId,
    EventEmitter, FocusHandle, Focusable, Render, Task, TaskExt as _, WeakEntity, Window,
    WindowHandle, actions, rems,
};
use picker::{Picker, PickerDelegate};
use schemars::JsonSchema;
use serde::Deserialize;
use terminal::TaskStatus;
use terminal_view::{TerminalView, terminal_panel::TerminalPanel};
use ui::{ListItem, ListItemSpacing, prelude::*};
use util::{ResultExt as _, paths::PathExt as _};
use workspace::{
    ModalView, MultiWorkspace, OpenMode, Pane, ProjectGroupKey, RecentWorkspace,
    SerializedWorkspaceLocation, Workspace, WorkspaceDb, ZoomIn, dock::Panel as _, item::Item as _,
};

const MODAL_WIDTH_REMS: f32 = 34.;

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
    ]
);

pub fn init(cx: &mut App) {
    cx.observe_new(register_actions).detach();
}

fn register_actions(
    workspace: &mut Workspace,
    _window: Option<&mut Window>,
    _cx: &mut Context<Workspace>,
) {
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

fn render_terminal_status(terminal_view: &TerminalView, cx: &App) -> Option<AnyElement> {
    let (icon, color, label) = if terminal_view.has_bell() {
        (IconName::Bell, Color::Warning, "bell")
    } else {
        match terminal_view.terminal().read(cx).task()?.status {
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

            Self {
                picker,
                _load_recent_projects: load_recent_projects,
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
            Row::View { session, view } => format!(
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
                        .map(|view| Row::View {
                            session: session.clone(),
                            view,
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
            Row::View { session, view } => activate_session(&session, Some(view), cx),
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
                item.start_slot(Icon::new(IconName::FolderOpen).color(Color::Muted))
                    .child(row_label(
                        session_name(&session.workspace, cx),
                        Some(details.join(" · ").into()),
                    ))
                    .end_slot::<Label>(session.is_current.then(|| {
                        Label::new("current")
                            .color(Color::Muted)
                            .size(LabelSize::Small)
                    }))
            }
            Row::View { session, view } => {
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
                        render_terminal_status(terminal_view.read(cx), cx)
                    }
                };
                item.indent_level(indent_level)
                    .start_slot(Icon::new(icon).color(Color::Muted))
                    .child(row_label(view_title(view, cx), detail))
                    .end_slot::<AnyElement>(status)
            }
            Row::Recent { name, location, .. } => item
                .start_slot(Icon::new(IconName::Folder).color(Color::Muted))
                .child(row_label(name.clone(), Some(location.clone()))),
        };
        Some(item)
    }
}
