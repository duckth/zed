use super::*;
use gpui::{TestAppContext, UpdateGlobal as _, VisualTestContext};
use project::Project;
use serde_json::json;
use settings::SettingsStore;
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
                    Row::View { view, index, .. } => {
                        format!("{index} {}", delegate.display_title(view, cx))
                    }
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
            "  0 editor",
            "> [root_a]",
            "  0 editor",
            "  1 rails",
            "  2 zsh",
        ],
        "the current session comes first, and the previous session is preselected"
    );

    select_row(&picker, "1 rails", cx);
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
        ["  0 editor", "  1 rails", "> 2 zsh"],
        "with the editor focused, the last used terminal is preselected"
    );
    cx.dispatch_action(menu::Cancel);
    cx.run_until_parked();

    let picker = open_switcher(Toggle::default(), &workspace_a, cx);
    picker.update_in(cx, |picker, window, cx| {
        picker.set_query("rails", window, cx)
    });
    cx.run_until_parked();
    assert_eq!(describe_rows(&picker, cx), ["> 1 rails"]);
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
        ["> [root_a] current", "  0 editor", "  (recent)"],
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

#[test]
fn test_strip_directory_prefix() {
    let working_directory = Path::new("/projects/zeduck");
    assert_eq!(
        strip_directory_prefix("zeduck — sleep 10000", Some(working_directory)),
        Some("sleep 10000")
    );
    assert_eq!(
        strip_directory_prefix("zeduck — zsh", Some(working_directory)),
        Some("zsh")
    );
    assert_eq!(
        strip_directory_prefix("core — zsh", Some(working_directory)),
        None,
        "a different directory's prefix is kept"
    );
    assert_eq!(
        strip_directory_prefix("zeduck-fork — zsh", Some(working_directory)),
        None,
        "only an exact directory name matches"
    );
    assert_eq!(
        strip_directory_prefix("zeduck — ", Some(working_directory)),
        None,
        "an empty process part is not stripped down to nothing"
    );
    assert_eq!(strip_directory_prefix("zeduck — zsh", None), None);
}

fn set_program_state(
    terminal_view: &Entity<TerminalView>,
    state: Option<ProgramState>,
    cx: &mut VisualTestContext,
) {
    terminal_view.update(cx, |terminal_view, cx| {
        terminal_view.terminal().clone().update(cx, |terminal, cx| {
            terminal.simulate_program_state(state, cx)
        });
    });
    cx.run_until_parked();
}

fn attention(terminal_view: &Entity<TerminalView>, cx: &mut VisualTestContext) -> bool {
    cx.read(|cx| needs_attention(terminal_view, cx))
}

fn total_attention_count(cx: &mut VisualTestContext) -> usize {
    cx.update(|window, cx| AttentionIndicator::total_attention_count(window, cx))
}

fn attention_indicator_shown(cx: &mut VisualTestContext) -> bool {
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();
    cx.debug_bounds("session-switcher-attention-count")
        .is_some()
}

#[gpui::test]
async fn test_terminals_needing_attention(cx: &mut TestAppContext) {
    let app_state = init_test(cx);
    let project = project(&app_state, path!("/root_a"), cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
    let panel = add_terminal_panel(&workspace, cx);
    let waiting = add_terminal(&workspace, &panel, "waiting", cx);
    let finished = add_terminal(&workspace, &panel, "finished", cx);
    let shell = add_terminal(&workspace, &panel, "zsh", cx);
    focus_editor(&workspace, cx);

    set_program_state(&waiting, Some(ProgramState::Blocked), cx);
    set_program_state(&finished, Some(ProgramState::Working), cx);
    assert!(attention(&waiting, cx));
    assert!(
        !attention(&finished, cx),
        "working programs don't need attention"
    );
    assert!(!attention(&shell, cx));

    set_program_state(&finished, Some(ProgramState::Done), cx);
    assert!(
        attention(&finished, cx),
        "finishing while unfocused needs attention"
    );
    assert_eq!(total_attention_count(cx), 2);
    workspace.read_with(cx, |workspace, cx| {
        assert_eq!(attention_count(workspace, cx), 2);
    });
    assert!(attention_indicator_shown(cx));

    cx.dispatch_action(ActivateView(2));
    cx.run_until_parked();
    assert!(is_focused(&finished, cx));
    assert!(!attention(&finished, cx), "looking at it clears done");
    assert_eq!(total_attention_count(cx), 1);

    set_program_state(&finished, Some(ProgramState::Working), cx);
    set_program_state(&finished, Some(ProgramState::Done), cx);
    assert!(
        !attention(&finished, cx),
        "finishing while focused does not need attention"
    );

    set_program_state(&shell, Some(ProgramState::Error), cx);
    set_program_state(&waiting, None, cx);
    assert!(attention(&shell, cx));
    assert!(
        !attention(&waiting, cx),
        "a cleared status needs no attention"
    );
    assert_eq!(total_attention_count(cx), 1);

    cx.update(|_, cx| {
        SettingsStore::update_global(cx, |store, cx| {
            store.update_user_settings(cx, |settings| {
                settings
                    .session_switcher
                    .get_or_insert_default()
                    .show_attention_count = Some(false);
            });
        });
    });
    assert!(
        !attention_indicator_shown(cx),
        "the status bar count can be turned off"
    );

    cx.update(|_, cx| {
        SettingsStore::update_global(cx, |store, cx| {
            store.update_user_settings(cx, |settings| {
                settings
                    .session_switcher
                    .get_or_insert_default()
                    .show_attention_count = Some(true);
            });
        });
    });
    assert!(attention_indicator_shown(cx));
    set_program_state(&shell, Some(ProgramState::Idle), cx);
    assert!(
        !attention_indicator_shown(cx),
        "the count is hidden when nothing needs attention"
    );
}

#[gpui::test]
async fn test_jump_to_next_attention_visits_every_session(cx: &mut TestAppContext) {
    let app_state = init_test(cx);
    let project_a = project(&app_state, path!("/root_a"), cx).await;
    let project_b = project(&app_state, path!("/root_b"), cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project_a, window, cx));
    let workspace_a = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
    let panel_a = add_terminal_panel(&workspace_a, cx);
    let first = add_terminal(&workspace_a, &panel_a, "first", cx);
    let quiet = add_terminal(&workspace_a, &panel_a, "quiet", cx);
    let second = add_terminal(&workspace_a, &panel_a, "second", cx);
    let workspace_b = multi_workspace.update_in(cx, |mw, window, cx| {
        mw.test_add_workspace(project_b, window, cx)
    });
    let panel_b = add_terminal_panel(&workspace_b, cx);
    let failed = add_terminal(&workspace_b, &panel_b, "failed", cx);
    multi_workspace.update_in(cx, |mw, window, cx| {
        mw.activate(workspace_a.clone(), None, window, cx);
    });
    focus_editor(&workspace_a, cx);

    set_program_state(&first, Some(ProgramState::Blocked), cx);
    set_program_state(&quiet, Some(ProgramState::Working), cx);
    set_program_state(&second, Some(ProgramState::Blocked), cx);
    set_program_state(&failed, Some(ProgramState::Error), cx);

    for (expected, workspace) in [
        (&first, &workspace_a),
        (&second, &workspace_a),
        (&failed, &workspace_b),
        (&first, &workspace_a),
    ] {
        cx.dispatch_action(JumpToNextAttention);
        cx.run_until_parked();
        multi_workspace.read_with(cx, |mw, _| assert_eq!(mw.workspace(), workspace));
        assert!(is_focused(expected, cx));
    }
    assert!(!is_focused(&quiet, cx));
}
