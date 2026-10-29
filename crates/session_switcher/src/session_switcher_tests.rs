use super::*;
use gpui::{TestAppContext, VisualTestContext};
use project::Project;
use serde_json::json;
use terminal::{
    TerminalBuilder,
    terminal_settings::{AlternateScroll, CursorShape},
};
use util::{path, path_list::PathList, paths::PathStyle};
use workspace::{AppState, WorkspaceId};

fn init_test(cx: &mut TestAppContext) -> Arc<AppState> {
    cx.update(|cx| {
        let app_state = AppState::test(cx);
        theme_settings::init(theme::LoadThemes::JustBase, cx);
        editor::init(cx);
        terminal_view::init(cx);
        super::init(cx);
        app_state
    })
}

async fn project(
    app_state: &Arc<AppState>,
    root: &str,
    cx: &mut TestAppContext,
) -> Entity<Project> {
    app_state
        .fs
        .as_fake()
        .insert_tree(root, json!({ "file.txt": "" }))
        .await;
    Project::test(app_state.fs.clone(), [root.as_ref()], cx).await
}

fn add_terminal_panel(
    workspace: &Entity<Workspace>,
    cx: &mut VisualTestContext,
) -> Entity<TerminalPanel> {
    workspace.update_in(cx, |workspace, window, cx| {
        let panel = cx.new(|cx| TerminalPanel::new(workspace, window, cx));
        workspace.add_panel(panel.clone(), window, cx);
        panel
    })
}

/// Adds a display-only terminal (no shell process) to the terminal panel and makes it the
/// panel's active terminal.
fn add_terminal(
    workspace: &Entity<Workspace>,
    panel: &Entity<TerminalPanel>,
    title: &str,
    cx: &mut VisualTestContext,
) -> Entity<TerminalView> {
    let title = title.to_string();
    workspace.update_in(cx, |workspace, window, cx| {
        let terminal = cx.new(|cx| {
            TerminalBuilder::new_display_only(
                CursorShape::default(),
                AlternateScroll::On,
                None,
                0,
                cx.background_executor(),
                PathStyle::local(),
            )
            .subscribe(cx)
        });
        let terminal_view = cx.new(|cx| {
            let mut terminal_view = TerminalView::new(
                terminal,
                workspace.weak_handle(),
                workspace.database_id(),
                workspace.project().downgrade(),
                window,
                cx,
            );
            terminal_view.set_custom_title(Some(title), cx);
            terminal_view
        });
        let pane = panel
            .read(cx)
            .panes()
            .first()
            .map(|pane| (*pane).clone())
            .expect("terminal panel always has a pane");
        pane.update(cx, |pane, cx| {
            pane.add_item(
                Box::new(terminal_view.clone()),
                true,
                false,
                None,
                window,
                cx,
            )
        });
        terminal_view
    })
}

fn focus_editor(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) {
    workspace.update_in(cx, |workspace, window, cx| {
        let pane = workspace.active_pane().clone();
        window.focus(&pane.read(cx).focus_handle(cx), cx);
    });
    cx.run_until_parked();
}

fn is_focused(terminal_view: &Entity<TerminalView>, cx: &mut VisualTestContext) -> bool {
    cx.update(|window, cx| {
        terminal_view
            .read(cx)
            .focus_handle(cx)
            .contains_focused(window, cx)
    })
}

fn editor_is_focused(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> bool {
    cx.update(|window, cx| {
        workspace
            .read(cx)
            .active_pane()
            .read(cx)
            .focus_handle(cx)
            .contains_focused(window, cx)
    })
}

fn terminal_panel_open(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> bool {
    workspace.read_with(cx, |workspace, cx| {
        workspace.all_docks().iter().any(|dock| {
            let dock = dock.read(cx);
            dock.panel::<TerminalPanel>().is_some() && dock.is_open()
        })
    })
}

fn terminal_panel_zoomed(panel: &Entity<TerminalPanel>, cx: &mut VisualTestContext) -> bool {
    panel.read_with(cx, |panel, cx| {
        panel
            .panes()
            .first()
            .is_some_and(|pane| pane.read(cx).is_zoomed())
    })
}

fn open_switcher(
    action: Toggle,
    workspace: &Entity<Workspace>,
    cx: &mut VisualTestContext,
) -> Entity<Picker<SessionSwitcherDelegate>> {
    cx.dispatch_action(action);
    cx.run_until_parked();
    workspace.update(cx, |workspace, cx| {
        workspace
            .active_modal::<SessionSwitcher>(cx)
            .expect("session switcher should be open")
            .read(cx)
            .picker
            .clone()
    })
}

/// Describes the switcher rows as `[session]` / `view` / `(recent)` lines, with the selected
/// row marked by `>`.
fn describe_rows(
    picker: &Entity<Picker<SessionSwitcherDelegate>>,
    cx: &mut VisualTestContext,
) -> Vec<String> {
    picker.read_with(cx, |picker, cx| {
        let delegate = &picker.delegate;
        delegate
            .rows
            .iter()
            .enumerate()
            .map(|(index, row)| {
                let marker = if index == delegate.selected_index {
                    ">"
                } else {
                    " "
                };
                let description = match row {
                    Row::Session(session) => {
                        let current = if session.is_current { " current" } else { "" };
                        format!("[{}]{current}", session_name(&session.workspace, cx))
                    }
                    Row::View { view, .. } => view_title(view, cx).to_string(),
                    Row::Recent { name, .. } => format!("({name})"),
                };
                format!("{marker} {description}")
            })
            .collect()
    })
}

fn confirm_selected(picker: &Entity<Picker<SessionSwitcherDelegate>>, cx: &mut VisualTestContext) {
    picker.update_in(cx, |picker, window, cx| {
        picker.delegate.confirm(false, window, cx)
    });
    cx.run_until_parked();
}

fn select_row(
    picker: &Entity<Picker<SessionSwitcherDelegate>>,
    description: &str,
    cx: &mut VisualTestContext,
) {
    let index = describe_rows(picker, cx)
        .iter()
        .position(|row| row[2..] == *description)
        .unwrap_or_else(|| panic!("no row {description:?}"));
    picker.update(cx, |picker, _| picker.delegate.selected_index = index);
}

#[gpui::test]
async fn test_toggle_last_view_switches_between_editor_and_last_terminal(cx: &mut TestAppContext) {
    let app_state = init_test(cx);
    let project = project(&app_state, path!("/root_a"), cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
    let panel = add_terminal_panel(&workspace, cx);
    let server = add_terminal(&workspace, &panel, "rails", cx);
    let shell = add_terminal(&workspace, &panel, "zsh", cx);
    focus_editor(&workspace, cx);

    cx.dispatch_action(ToggleLastView);
    cx.run_until_parked();
    assert!(is_focused(&shell, cx), "the panel's last active terminal");
    assert!(!is_focused(&server, cx));
    assert!(terminal_panel_open(&workspace, cx));
    assert!(terminal_panel_zoomed(&panel, cx));

    cx.dispatch_action(ToggleLastView);
    cx.run_until_parked();
    assert!(editor_is_focused(&workspace, cx));
    assert!(!terminal_panel_open(&workspace, cx));

    cx.dispatch_action(ToggleLastView);
    cx.run_until_parked();
    assert!(is_focused(&shell, cx));
    assert!(terminal_panel_zoomed(&panel, cx));
}

#[gpui::test]
async fn test_view_cycling_and_activation_by_index(cx: &mut TestAppContext) {
    let app_state = init_test(cx);
    let project = project(&app_state, path!("/root_a"), cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
    let panel = add_terminal_panel(&workspace, cx);
    let server = add_terminal(&workspace, &panel, "rails", cx);
    let shell = add_terminal(&workspace, &panel, "zsh", cx);
    focus_editor(&workspace, cx);

    cx.dispatch_action(NextView);
    cx.run_until_parked();
    assert!(is_focused(&server, cx));
    assert!(terminal_panel_zoomed(&panel, cx));

    cx.dispatch_action(NextView);
    cx.run_until_parked();
    assert!(is_focused(&shell, cx));

    cx.dispatch_action(NextView);
    cx.run_until_parked();
    assert!(
        editor_is_focused(&workspace, cx),
        "wraps around to the editor"
    );

    cx.dispatch_action(PreviousView);
    cx.run_until_parked();
    assert!(is_focused(&shell, cx), "wraps around backwards");

    cx.dispatch_action(ActivateView(1));
    cx.run_until_parked();
    assert!(is_focused(&server, cx));

    cx.dispatch_action(ActivateView(0));
    cx.run_until_parked();
    assert!(editor_is_focused(&workspace, cx));

    cx.dispatch_action(ActivateView(9));
    cx.run_until_parked();
    assert!(
        editor_is_focused(&workspace, cx),
        "out-of-range indices are ignored"
    );
}

#[gpui::test]
async fn test_toggle_last_session(cx: &mut TestAppContext) {
    let app_state = init_test(cx);
    let project_a = project(&app_state, path!("/root_a"), cx).await;
    let project_b = project(&app_state, path!("/root_b"), cx).await;
    let project_c = project(&app_state, path!("/root_c"), cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project_a, window, cx));
    let workspace_a = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
    let workspace_b = multi_workspace.update_in(cx, |mw, window, cx| {
        mw.test_add_workspace(project_b, window, cx)
    });
    let workspace_c = multi_workspace.update_in(cx, |mw, window, cx| {
        mw.test_add_workspace(project_c, window, cx)
    });
    multi_workspace.update_in(cx, |mw, window, cx| {
        mw.activate(workspace_a.clone(), None, window, cx);
    });
    cx.run_until_parked();

    cx.dispatch_action(ToggleLastSession);
    cx.run_until_parked();
    multi_workspace.read_with(cx, |mw, _| assert_eq!(mw.workspace(), &workspace_c));

    cx.dispatch_action(ToggleLastSession);
    cx.run_until_parked();
    multi_workspace.read_with(cx, |mw, _| assert_eq!(mw.workspace(), &workspace_a));

    multi_workspace.read_with(cx, |mw, _| {
        assert!(mw.workspaces().any(|workspace| workspace == &workspace_b));
    });
}

#[gpui::test]
async fn test_switcher_lists_sessions_with_their_views(cx: &mut TestAppContext) {
    let app_state = init_test(cx);
    let project_a = project(&app_state, path!("/root_a"), cx).await;
    let project_b = project(&app_state, path!("/root_b"), cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project_a, window, cx));
    let workspace_a = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
    let panel_a = add_terminal_panel(&workspace_a, cx);
    let server = add_terminal(&workspace_a, &panel_a, "rails", cx);
    add_terminal(&workspace_a, &panel_a, "zsh", cx);
    let workspace_b = multi_workspace.update_in(cx, |mw, window, cx| {
        mw.test_add_workspace(project_b, window, cx)
    });
    cx.run_until_parked();

    let picker = open_switcher(Toggle::default(), &workspace_b, cx);
    assert_eq!(
        describe_rows(&picker, cx),
        [
            "  [root_b] current",
            "  editor",
            "> [root_a]",
            "  editor",
            "  rails",
            "  zsh",
        ],
        "the current session comes first, and the previous session is preselected"
    );

    select_row(&picker, "rails", cx);
    confirm_selected(&picker, cx);
    multi_workspace.read_with(cx, |mw, _| assert_eq!(mw.workspace(), &workspace_a));
    assert!(is_focused(&server, cx));
    assert!(terminal_panel_zoomed(&panel_a, cx));
    workspace_a.read_with(cx, |workspace, cx| {
        assert!(workspace.active_modal::<SessionSwitcher>(cx).is_none());
    });
    workspace_b.read_with(cx, |workspace, cx| {
        assert!(workspace.active_modal::<SessionSwitcher>(cx).is_none());
    });
}

#[gpui::test]
async fn test_switcher_modes_and_filtering(cx: &mut TestAppContext) {
    let app_state = init_test(cx);
    let project_a = project(&app_state, path!("/root_a"), cx).await;
    let project_b = project(&app_state, path!("/root_b"), cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project_a, window, cx));
    let workspace_a = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
    multi_workspace.update_in(cx, |mw, window, cx| {
        mw.test_add_workspace(project_b, window, cx)
    });
    multi_workspace.update_in(cx, |mw, window, cx| {
        mw.activate(workspace_a.clone(), None, window, cx);
    });
    let panel = add_terminal_panel(&workspace_a, cx);
    add_terminal(&workspace_a, &panel, "rails", cx);
    add_terminal(&workspace_a, &panel, "zsh", cx);
    focus_editor(&workspace_a, cx);

    let picker = open_switcher(
        Toggle {
            sessions_only: true,
            current_session_only: false,
        },
        &workspace_a,
        cx,
    );
    assert_eq!(
        describe_rows(&picker, cx),
        ["  [root_a] current", "> [root_b]"]
    );
    cx.dispatch_action(menu::Cancel);
    cx.run_until_parked();

    let picker = open_switcher(
        Toggle {
            sessions_only: false,
            current_session_only: true,
        },
        &workspace_a,
        cx,
    );
    assert_eq!(
        describe_rows(&picker, cx),
        ["  editor", "  rails", "> zsh"],
        "with the editor focused, the last used terminal is preselected"
    );
    cx.dispatch_action(menu::Cancel);
    cx.run_until_parked();

    let picker = open_switcher(Toggle::default(), &workspace_a, cx);
    picker.update_in(cx, |picker, window, cx| {
        picker.set_query("rails", window, cx)
    });
    cx.run_until_parked();
    assert_eq!(describe_rows(&picker, cx), ["> rails"]);
}

#[gpui::test]
async fn test_switcher_lists_recent_projects_that_are_not_open(cx: &mut TestAppContext) {
    let app_state = init_test(cx);
    let project_a = project(&app_state, path!("/root_a"), cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project_a, window, cx));
    let workspace_a = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
    app_state
        .fs
        .as_fake()
        .insert_tree(path!("/recent"), json!({ "file.txt": "" }))
        .await;

    let recent_workspace = |id: i64, path: &str| {
        let paths = PathList::new(&[PathBuf::from(path)]);
        RecentWorkspace {
            workspace_id: WorkspaceId::from_i64(id),
            location: SerializedWorkspaceLocation::Local,
            paths: paths.clone(),
            identity_paths: paths,
            timestamp: chrono::Utc::now(),
        }
    };

    let picker = open_switcher(Toggle::default(), &workspace_a, cx);
    picker.update_in(cx, |picker, window, cx| {
        picker.delegate.recent_workspaces = vec![
            recent_workspace(1, path!("/root_a")),
            recent_workspace(2, path!("/recent")),
        ];
        picker.update_matches(String::new(), window, cx);
    });
    cx.run_until_parked();
    assert_eq!(
        describe_rows(&picker, cx),
        ["> [root_a] current", "  editor", "  (recent)"],
        "projects that are already open are not repeated as recent projects"
    );
    picker.read_with(cx, |picker, _| {
        assert_eq!(picker.delegate.separators_after_indices(), [1]);
    });

    select_row(&picker, "(recent)", cx);
    confirm_selected(&picker, cx);
    multi_workspace.read_with(cx, |mw, cx| {
        assert_ne!(mw.workspace(), &workspace_a);
        assert_eq!(session_name(mw.workspace(), cx), "recent");
        assert!(mw.workspaces().any(|workspace| workspace == &workspace_a));
    });
}
