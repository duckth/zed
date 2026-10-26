use std::{cmp::Reverse, sync::Arc};

use collections::HashSet;
use editor::Editor;
use fuzzy_nucleo::StringMatchCandidate;
use gpui::{
    AnyElement, App, Context, DismissEvent, Entity, EntityId, EventEmitter, FocusHandle, Focusable,
    HighlightStyle, MouseButton, MouseUpEvent, ParentElement, Render, Styled, Subscription, Task,
    WeakEntity, Window, actions,
};
use picker::{HighlightedTextBuilder, Picker, PickerDelegate, PreviewSource, PreviewUpdate};
use project::{Project, ProjectPath};
use settings::SettingsStore;
use ui::{DecoratedIcon, HighlightedLabel, ListItem, ListItemSpacing, Tooltip, prelude::*};
use util::{ResultExt, paths::PathStyle, rel_path::RelPath};
use workspace::{
    Event as WorkspaceEvent, ModalView, Pane, SaveIntent, Workspace, item::ItemHandle,
    pane::render_item_indicator,
};

use crate::{CloseSelectedItem, show_item_in_pane, tab_icon};

actions!(
    tab_finder,
    [
        /// Toggles the tab finder: a searchable list of the open tabs across all panes, with an
        /// optional preview. Opens the selected tab in the active pane.
        Toggle,
    ]
);

pub fn init(cx: &mut App) {
    cx.observe_new(TabFinder::register).detach();
}

pub struct TabFinder {
    picker: Entity<Picker<TabFinderDelegate>>,
    _subscriptions: Vec<Subscription>,
}

impl ModalView for TabFinder {}

impl TabFinder {
    fn register(
        workspace: &mut Workspace,
        _window: Option<&mut Window>,
        _: &mut Context<Workspace>,
    ) {
        workspace.register_action(|workspace, _: &Toggle, window, cx| {
            let tabs = collect_tabs(workspace, cx);
            let project = workspace.project().clone();
            let workspace_handle = cx.entity();
            workspace.toggle_modal(window, cx, |window, cx| {
                TabFinder::new(workspace_handle, project, tabs, window, cx)
            });
        });
    }

    fn new(
        workspace: Entity<Workspace>,
        project: Entity<Project>,
        tabs: Vec<Tab>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let delegate = TabFinderDelegate {
            tab_finder: cx.entity().downgrade(),
            workspace: workspace.downgrade(),
            project: project.clone(),
            tabs,
            matches: Vec::new(),
            selected_index: 0,
            query: String::new(),
        };
        let preview = picker_preview::editor_preview(project, window, cx);
        let picker = cx.new(|cx| Picker::uniform_list_with_preview(delegate, preview, window, cx));

        let workspace_subscription = cx.subscribe_in(
            &workspace,
            window,
            |tab_finder, workspace, event, window, cx| {
                if matches!(
                    event,
                    WorkspaceEvent::ItemAdded { .. }
                        | WorkspaceEvent::ItemRemoved { .. }
                        | WorkspaceEvent::PaneRemoved
                ) {
                    let tabs = collect_tabs(workspace.read(cx), cx);
                    tab_finder.picker.update(cx, |picker, cx| {
                        picker.delegate.tabs = tabs;
                        let query = picker.query(cx);
                        picker.update_matches(query, window, cx);
                    });
                }
            },
        );

        Self {
            picker,
            _subscriptions: vec![workspace_subscription],
        }
    }

    fn handle_close_selected_item(
        &mut self,
        _: &CloseSelectedItem,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.picker.update(cx, |picker, cx| {
            let selected_index = picker.delegate.selected_index;
            picker.delegate.close_tab_at(selected_index, window, cx);
        });
    }
}

impl EventEmitter<DismissEvent> for TabFinder {}

impl Focusable for TabFinder {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.picker.focus_handle(cx)
    }
}

impl Render for TabFinder {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .key_context("TabFinder")
            .on_action(cx.listener(Self::handle_close_selected_item))
            .child(self.picker.clone())
    }
}

struct Tab {
    pane: WeakEntity<Pane>,
    item: Box<dyn ItemHandle>,
    project_path: Option<ProjectPath>,
    name: String,
    /// The tab's directory, ending in a separator, or empty for tabs that aren't files.
    directory: String,
}

struct TabFinderMatch {
    tab_index: usize,
    name_positions: Vec<usize>,
    directory_positions: Vec<usize>,
}

pub struct TabFinderDelegate {
    tab_finder: WeakEntity<TabFinder>,
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    tabs: Vec<Tab>,
    matches: Vec<TabFinderMatch>,
    selected_index: usize,
    query: String,
}

/// The tabs of all panes, most recently activated first, with a file open in several panes
/// listed once.
fn collect_tabs(workspace: &Workspace, cx: &App) -> Vec<Tab> {
    let project = workspace.project().read(cx);
    let path_style = project.path_style(cx);
    let hide_root = should_hide_root(project, cx);
    let history = workspace.recently_activated_items(cx);

    let mut items = Vec::new();
    for pane in workspace.panes() {
        for item in pane.read(cx).items() {
            items.push((pane.downgrade(), item.boxed_clone()));
        }
    }
    items.sort_by_key(|(_, item)| Reverse(history.get(&item.item_id()).copied()));

    let mut seen_paths = HashSet::default();
    items
        .into_iter()
        .filter_map(|(pane, item)| {
            let project_path = item.project_path(cx);
            if let Some(project_path) = &project_path
                && !seen_paths.insert(project_path.clone())
            {
                return None;
            }
            let (name, directory) = project_path
                .as_ref()
                .and_then(|project_path| {
                    path_labels(project_path, project, hide_root, path_style, cx)
                })
                .unwrap_or_else(|| (item.tab_content_text(0, cx).to_string(), String::new()));
            Some(Tab {
                pane,
                item,
                project_path,
                name,
                directory,
            })
        })
        .collect()
}

fn path_labels(
    project_path: &ProjectPath,
    project: &Project,
    hide_root: bool,
    path_style: PathStyle,
    cx: &App,
) -> Option<(String, String)> {
    let full_path: Arc<RelPath> = match project.worktree_for_id(project_path.worktree_id, cx) {
        Some(worktree) if !hide_root => worktree
            .read(cx)
            .root_name()
            .join(&project_path.path)
            .into(),
        _ => project_path.path.clone(),
    };
    let name = full_path.file_name()?.to_string();
    let directory = full_path
        .parent()
        .filter(|parent| !parent.is_empty())
        .map(|parent| {
            format!(
                "{}{}",
                parent.display(path_style),
                path_style.primary_separator()
            )
        })
        .unwrap_or_default();
    Some((name, directory))
}

/// Mirrors the file finder, so the same file shows the same path in both pickers.
fn should_hide_root(project: &Project, cx: &App) -> bool {
    let multiple_worktrees = project
        .visible_worktrees(cx)
        .filter(|worktree| !worktree.read(cx).is_single_file())
        .nth(1)
        .is_some();
    let hide_root = cx
        .global::<SettingsStore>()
        .merged_settings()
        .project_panel
        .as_ref()
        .and_then(|project_panel| project_panel.hide_root)
        .unwrap_or(false);
    hide_root && !multiple_worktrees
}

impl TabFinderDelegate {
    fn tab_at(&self, ix: usize) -> Option<&Tab> {
        self.tabs.get(self.matches.get(ix)?.tab_index)
    }

    fn selected_item_id(&self) -> Option<EntityId> {
        self.tab_at(self.selected_index)
            .map(|tab| tab.item.item_id())
    }

    fn close_tab_at(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        let Some(tab) = self.tab_at(ix) else {
            return;
        };
        // A file is listed once even when it's open in several panes, so closing it closes
        // every one of its tabs.
        if let Some(project_path) = &tab.project_path {
            let Some(workspace) = self.workspace.upgrade() else {
                return;
            };
            workspace.update(cx, |workspace, cx| {
                workspace.close_items_with_project_path(
                    project_path,
                    SaveIntent::Close,
                    true,
                    window,
                    cx,
                );
            });
        } else {
            let Some(pane) = tab.pane.upgrade() else {
                return;
            };
            pane.update(cx, |pane, cx| {
                pane.close_item_by_id(tab.item.item_id(), SaveIntent::Close, window, cx)
                    .detach_and_log_err(cx);
            });
        }
    }
}

impl PickerDelegate for TabFinderDelegate {
    type ListItem = ListItem;

    fn name() -> &'static str {
        "tab finder"
    }

    fn placeholder_text(&self, _window: &mut Window, _cx: &mut App) -> Arc<str> {
        "Search open tabs…".into()
    }

    fn no_matches_text(&self, _window: &mut Window, _cx: &mut App) -> Option<SharedString> {
        Some("No tabs".into())
    }

    fn match_count(&self) -> usize {
        self.matches.len()
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

    fn update_matches(
        &mut self,
        query: String,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        let query_changed = query != self.query;
        let previously_selected_item = self.selected_item_id();

        self.matches = if query.is_empty() {
            (0..self.tabs.len())
                .map(|tab_index| TabFinderMatch {
                    tab_index,
                    name_positions: Vec::new(),
                    directory_positions: Vec::new(),
                })
                .collect()
        } else {
            let candidates = self
                .tabs
                .iter()
                .enumerate()
                .map(|(tab_index, tab)| {
                    StringMatchCandidate::new(tab_index, format!("{}{}", tab.directory, tab.name))
                })
                .collect::<Vec<_>>();
            fuzzy_nucleo::match_strings(
                &candidates,
                &query,
                fuzzy_nucleo::Case::Smart,
                fuzzy_nucleo::LengthPenalty::On,
                10000,
            )
            .into_iter()
            .map(|string_match| {
                let directory_len = self
                    .tabs
                    .get(string_match.candidate_id)
                    .map_or(0, |tab| tab.directory.len());
                let (directory_positions, name_positions): (Vec<usize>, Vec<usize>) = string_match
                    .positions
                    .into_iter()
                    .partition(|position| *position < directory_len);
                TabFinderMatch {
                    tab_index: string_match.candidate_id,
                    name_positions: name_positions
                        .into_iter()
                        .map(|position| position - directory_len)
                        .collect(),
                    directory_positions,
                }
            })
            .collect()
        };
        self.query = query;

        // A new query starts from its best match; the same query (re-run because tabs were
        // opened or closed) keeps the selection where it was.
        self.selected_index = if query_changed {
            0
        } else {
            previously_selected_item
                .and_then(|item_id| {
                    self.matches.iter().position(|finder_match| {
                        self.tabs
                            .get(finder_match.tab_index)
                            .is_some_and(|tab| tab.item.item_id() == item_id)
                    })
                })
                .unwrap_or_else(|| {
                    self.selected_index
                        .min(self.matches.len().saturating_sub(1))
                })
        };
        Task::ready(())
    }

    fn confirm(&mut self, _secondary: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        let Some(tab) = self.tab_at(self.selected_index) else {
            return;
        };
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let target_pane = workspace.read(cx).active_pane().clone();
        show_item_in_pane(
            &workspace,
            &target_pane,
            &tab.pane,
            tab.item.as_ref(),
            window,
            cx,
        );
        self.tab_finder
            .update(cx, |_, cx| cx.emit(DismissEvent))
            .log_err();
    }

    fn dismissed(&mut self, _window: &mut Window, cx: &mut Context<Picker<Self>>) {
        self.tab_finder
            .update(cx, |_, cx| cx.emit(DismissEvent))
            .log_err();
    }

    fn try_get_preview_data_for_match(&self, cx: &App) -> Option<PreviewUpdate> {
        let tab = self.tab_at(self.selected_index)?;
        let buffer = tab
            .item
            .act_as::<Editor>(cx)
            .and_then(|editor| editor.read(cx).buffer().read(cx).as_singleton());
        Some(match buffer {
            Some(buffer) => PreviewUpdate {
                source: PreviewSource::Buffer(buffer),
                match_location: None,
            },
            None => {
                let mut message = HighlightedTextBuilder::default();
                message.push_plain("No preview for ");
                message.push_styled(
                    &tab.name,
                    HighlightStyle {
                        color: Some(cx.theme().colors().text_accent),
                        ..Default::default()
                    },
                );
                PreviewUpdate::message(message.build())
            }
        })
    }

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let finder_match = self.matches.get(ix)?;
        let tab = self.tabs.get(finder_match.tab_index)?;

        let icon = tab_icon(tab.item.as_ref(), &self.project, selected, window, cx);
        let name_label =
            HighlightedLabel::new(tab.name.clone(), finder_match.name_positions.clone())
                .single_line();
        let directory_label = HighlightedLabel::new(
            tab.directory.clone(),
            finder_match.directory_positions.clone(),
        )
        .single_line()
        .size(LabelSize::Small)
        .color(Color::Muted);

        let indicator = render_item_indicator(tab.item.boxed_clone(), cx);
        let indicator_color = indicator
            .as_ref()
            .map_or(Color::default(), |indicator| indicator.color);
        let indicator = h_flex()
            .flex_shrink_0()
            .children(indicator)
            .child(div().w_2())
            .into_any_element();
        let close_button = div()
            .id("close-button")
            .on_mouse_up(
                // On macOS a ctrl-click arrives as a right click.
                MouseButton::Right,
                cx.listener(move |picker, _: &MouseUpEvent, window, cx| {
                    cx.stop_propagation();
                    picker.delegate.close_tab_at(ix, window, cx);
                }),
            )
            .child(
                IconButton::new("close_tab", IconName::Close)
                    .icon_size(IconSize::Small)
                    .icon_color(indicator_color)
                    .tooltip(Tooltip::for_action_title("Close", &CloseSelectedItem))
                    .on_click(cx.listener(move |picker, _, window, cx| {
                        cx.stop_propagation();
                        picker.delegate.close_tab_at(ix, window, cx);
                    })),
            )
            .into_any_element();

        Some(
            ListItem::new(ix)
                .spacing(ListItemSpacing::Sparse)
                .inset(true)
                .toggle_state(selected)
                .start_slot::<DecoratedIcon>(icon)
                .child(
                    h_flex()
                        .w_full()
                        .min_w_0()
                        .gap_1p5()
                        .child(name_label.flex_none().truncate_middle())
                        .child(directory_label.flex_1().truncate_start()),
                )
                .map(|item| {
                    if selected {
                        item.end_slot::<AnyElement>(close_button)
                    } else {
                        item.end_slot::<AnyElement>(indicator)
                            .end_slot_on_hover::<AnyElement>(close_button)
                    }
                }),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{TestAppContext, VisualTestContext};
    use serde_json::json;
    use util::{path, rel_path::rel_path};
    use workspace::{AppState, MultiWorkspace, SplitDirection, item::test::TestItem};

    fn init_test(cx: &mut TestAppContext) -> Arc<AppState> {
        cx.update(|cx| {
            let state = AppState::test(cx);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            crate::init(cx);
            editor::init(cx);
            state
        })
    }

    async fn open_file(
        file_path: &str,
        workspace: &Entity<Workspace>,
        cx: &mut VisualTestContext,
    ) -> Box<dyn ItemHandle> {
        let project = workspace.read_with(cx, |workspace, _| workspace.project().clone());
        let worktree_id = project.update(cx, |project, cx| {
            let worktree = project.worktrees(cx).last().expect("worktree not found");
            worktree.read(cx).id()
        });
        let project_path = ProjectPath {
            worktree_id,
            path: rel_path(file_path).into(),
        };
        workspace
            .update_in(cx, move |workspace, window, cx| {
                workspace.open_path(project_path, None, true, window, cx)
            })
            .await
            .expect("failed to open file")
    }

    fn open_tab_finder(
        workspace: &Entity<Workspace>,
        cx: &mut VisualTestContext,
    ) -> Entity<Picker<TabFinderDelegate>> {
        cx.dispatch_action(Toggle);
        workspace.update(cx, |workspace, cx| {
            workspace
                .active_modal::<TabFinder>(cx)
                .expect("tab finder is not open")
                .read(cx)
                .picker
                .clone()
        })
    }

    fn listed_tabs(picker: &Picker<TabFinderDelegate>) -> Vec<(String, String)> {
        picker
            .delegate
            .matches
            .iter()
            .filter_map(|finder_match| picker.delegate.tabs.get(finder_match.tab_index))
            .map(|tab| (tab.directory.clone(), tab.name.clone()))
            .collect()
    }

    async fn setup(
        files: serde_json::Value,
        cx: &mut TestAppContext,
    ) -> (Entity<Workspace>, &mut VisualTestContext) {
        let app_state = init_test(cx);
        app_state
            .fs
            .as_fake()
            .insert_tree(path!("/root"), files)
            .await;
        let project = Project::test(app_state.fs.clone(), [path!("/root").as_ref()], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
        let workspace =
            multi_workspace.read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone());
        (workspace, cx)
    }

    #[gpui::test]
    async fn test_lists_tabs_of_all_panes_most_recent_first(cx: &mut TestAppContext) {
        let (workspace, cx) = setup(json!({"a.txt": "", "b.txt": "", "c.txt": ""}), cx).await;
        open_file("a.txt", &workspace, cx).await;
        open_file("b.txt", &workspace, cx).await;
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.split_pane(
                workspace.active_pane().clone(),
                SplitDirection::Right,
                window,
                cx,
            );
        });
        open_file("c.txt", &workspace, cx).await;
        open_file("a.txt", &workspace, cx).await;

        let picker = open_tab_finder(&workspace, cx);
        picker.read_with(cx, |picker, _| {
            assert_eq!(
                listed_tabs(picker),
                [
                    ("root/".to_string(), "a.txt".to_string()),
                    ("root/".to_string(), "c.txt".to_string()),
                    ("root/".to_string(), "b.txt".to_string()),
                ],
                "a.txt is open in both panes but listed once, as the most recent tab"
            );
            assert_eq!(
                picker.delegate.selected_index, 0,
                "the current tab is selected"
            );
        });
    }

    #[gpui::test]
    async fn test_query_matches_directories(cx: &mut TestAppContext) {
        let (workspace, cx) = setup(
            json!({
                "src": {"main.rs": ""},
                "tests": {"main.rs": ""},
            }),
            cx,
        )
        .await;
        open_file("src/main.rs", &workspace, cx).await;
        open_file("tests/main.rs", &workspace, cx).await;

        let picker = open_tab_finder(&workspace, cx);
        picker.update_in(cx, |picker, window, cx| {
            picker.update_matches("srcmain".to_string(), window, cx);
        });
        picker.read_with(cx, |picker, _| {
            assert_eq!(
                listed_tabs(picker).first(),
                Some(&("root/src/".to_string(), "main.rs".to_string()))
            );
            let best_match = picker.delegate.matches.first().expect("no matches");
            assert_eq!(best_match.directory_positions, [5, 6, 7]);
            assert_eq!(best_match.name_positions, [0, 1, 2, 3]);
        });
    }

    #[gpui::test]
    async fn test_confirm_opens_tab_in_active_pane(cx: &mut TestAppContext) {
        let (workspace, cx) = setup(json!({"a.txt": ""}), cx).await;
        let original_tab = open_file("a.txt", &workspace, cx).await;
        let target_pane = workspace.update_in(cx, |workspace, window, cx| {
            workspace.split_pane(
                workspace.active_pane().clone(),
                SplitDirection::Right,
                window,
                cx,
            )
        });

        open_tab_finder(&workspace, cx);
        cx.dispatch_action(menu::Confirm);
        cx.run_until_parked();

        let opened_editor = target_pane.read_with(cx, |pane, cx| {
            pane.active_item()
                .and_then(|item| item.act_as::<Editor>(cx))
                .expect("the active pane should show the file")
        });
        assert_ne!(
            opened_editor.entity_id(),
            original_tab.item_id(),
            "the file is opened as a new tab in the active pane, not moved"
        );
        workspace.read_with(cx, |workspace, cx| {
            assert!(workspace.active_modal::<TabFinder>(cx).is_none());
        });
    }

    #[gpui::test]
    async fn test_preview_shows_buffer_or_message(cx: &mut TestAppContext) {
        let (workspace, cx) = setup(json!({"a.txt": ""}), cx).await;
        open_file("a.txt", &workspace, cx).await;
        let terminal_like_item = cx.new(|cx| TestItem::new(cx).with_label("terminal"));
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.add_item_to_active_pane(Box::new(terminal_like_item), None, true, window, cx);
        });

        let picker = open_tab_finder(&workspace, cx);
        picker.update_in(cx, |picker, window, cx| {
            let preview = picker.delegate.try_get_preview_data_for_match(cx);
            assert!(
                matches!(
                    preview.map(|update| update.source),
                    Some(PreviewSource::Message(_))
                ),
                "a tab without a buffer shows a message"
            );

            picker.delegate.set_selected_index(1, window, cx);
            let preview = picker.delegate.try_get_preview_data_for_match(cx);
            assert!(
                matches!(
                    preview.map(|update| update.source),
                    Some(PreviewSource::Buffer(_))
                ),
                "a file tab previews its buffer"
            );
        });
    }
}
