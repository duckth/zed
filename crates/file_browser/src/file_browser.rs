use std::{any::Any, any::TypeId, sync::Arc};

use editor::{
    Addon, Editor, SelectionEffects, display_map::HighlightKey, items::entry_git_aware_label_color,
    scroll::Autoscroll,
};
use gpui::{
    AnyEntity, App, Context, Entity, EventEmitter, FocusHandle, Focusable, HighlightStyle,
    KeyContext, ParentElement, Render, Styled, Subscription, TaskExt, WeakEntity, Window, actions,
};
use language::{Buffer, Point};
use project::{Entry, Project, ProjectEntryId, ProjectPath, WorktreeId, git_store::GitStoreEvent};
use ui::prelude::*;
use util::rel_path::RelPath;
use workspace::{
    SaveIntent, Workspace,
    item::{Item, ItemEvent},
};

actions!(
    file_browser,
    [
        /// Opens the file browser at the directory of the active file, or at the project root.
        Open,
        /// Opens the file or directory under the cursor in the file browser.
        OpenEntry,
        /// Shows the parent directory in the file browser.
        GoToParent,
        /// Shows the project root in the file browser.
        GoToRoot,
        /// Toggles showing hidden files (those starting with a dot) in the file browser.
        ToggleHidden,
    ]
);

pub fn init(cx: &mut App) {
    cx.observe_new(
        |workspace: &mut Workspace, _window, _: &mut Context<Workspace>| {
            workspace.register_action(|workspace, _: &Open, window, cx| {
                FileBrowser::open(workspace, window, cx);
            });
        },
    )
    .detach();
}

pub struct FileBrowser {
    editor: Entity<Editor>,
    buffer: Entity<Buffer>,
    project: Entity<Project>,
    workspace: WeakEntity<Workspace>,
    worktree_id: WorktreeId,
    directory: Arc<RelPath>,
    entries: Vec<BrowserEntry>,
    show_hidden: bool,
    highlight_group_count: usize,
    _subscriptions: Vec<Subscription>,
}

#[derive(Clone, PartialEq)]
struct BrowserEntry {
    id: ProjectEntryId,
    name: String,
    path: Arc<RelPath>,
    is_dir: bool,
    is_ignored: bool,
}

impl BrowserEntry {
    fn line(&self) -> String {
        if self.is_dir {
            format!("{}/", self.name)
        } else {
            self.name.clone()
        }
    }
}

enum CursorTarget {
    /// Keep the cursor on the entry it's on, e.g. when the directory's contents change.
    Keep,
    /// Put the cursor on the entry with this name, falling back to the first entry.
    Name(String),
    First,
}

/// Gives the browser's editor a `FileBrowser` key context, so keymaps can bind keys that only
/// apply inside the browser.
struct FileBrowserAddon;

impl Addon for FileBrowserAddon {
    fn extend_key_context(&self, key_context: &mut KeyContext, _: &App) {
        key_context.add("FileBrowser");
    }

    fn to_any(&self) -> &dyn Any {
        self
    }
}

pub enum Event {
    DirectoryChanged,
}

impl FileBrowser {
    fn open(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
        let project = workspace.project().clone();
        let active_path = workspace
            .active_item(cx)
            .and_then(|item| item.project_path(cx));
        let (worktree_id, directory, selected_name) = match active_path {
            Some(active_path) => (
                active_path.worktree_id,
                active_path
                    .path
                    .parent()
                    .map_or_else(RelPath::empty_arc, Into::into),
                active_path.path.file_name().map(str::to_string),
            ),
            None => {
                let Some(worktree) = project.read(cx).visible_worktrees(cx).next() else {
                    return;
                };
                (worktree.read(cx).id(), RelPath::empty_arc(), None)
            }
        };

        let workspace_handle = workspace.weak_handle();
        let browser = cx.new(|cx| {
            FileBrowser::new(
                project,
                workspace_handle,
                worktree_id,
                directory,
                selected_name,
                window,
                cx,
            )
        });
        workspace.add_item_to_active_pane(Box::new(browser), None, true, window, cx);
    }

    fn new(
        project: Entity<Project>,
        workspace: WeakEntity<Workspace>,
        worktree_id: WorktreeId,
        directory: Arc<RelPath>,
        selected_name: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let buffer = cx.new(|cx| Buffer::local("", cx));
        let editor = cx.new(|cx| {
            let mut editor = Editor::for_buffer(buffer.clone(), None, window, cx);
            editor.set_read_only(true);
            editor.register_addon(FileBrowserAddon);
            editor
        });

        let project_subscription =
            cx.subscribe_in(&project, window, |this, _, event, window, cx| match event {
                project::Event::WorktreeUpdatedEntries(worktree_id, _)
                    if *worktree_id == this.worktree_id =>
                {
                    this.refresh(CursorTarget::Keep, window, cx);
                }
                _ => {}
            });
        let git_store = project.read(cx).git_store().clone();
        let git_subscription = cx.subscribe(&git_store, |this, _, event: &GitStoreEvent, cx| {
            if matches!(event, GitStoreEvent::RepositoryUpdated(..)) {
                this.update_highlights(cx);
            }
        });

        let mut browser = Self {
            editor,
            buffer,
            project,
            workspace,
            worktree_id,
            directory,
            entries: Vec::new(),
            show_hidden: false,
            highlight_group_count: 0,
            _subscriptions: vec![project_subscription, git_subscription],
        };
        browser.load_directory(window, cx);
        let cursor_target = selected_name.map_or(CursorTarget::First, CursorTarget::Name);
        browser.refresh(cursor_target, window, cx);
        browser
    }

    fn read_entries(&self, cx: &App) -> Vec<BrowserEntry> {
        let Some(worktree) = self.project.read(cx).worktree_for_id(self.worktree_id, cx) else {
            return Vec::new();
        };
        let snapshot = worktree.read(cx).snapshot();
        let mut entries = snapshot
            .child_entries(&self.directory)
            .filter_map(|entry: &Entry| {
                let name = entry.path.file_name()?.to_string();
                if !self.show_hidden && name.starts_with('.') {
                    return None;
                }
                Some(BrowserEntry {
                    id: entry.id,
                    name,
                    path: entry.path.clone(),
                    is_dir: entry.is_dir(),
                    is_ignored: entry.is_ignored,
                })
            })
            .collect::<Vec<_>>();
        entries.sort_by(|a, b| {
            b.is_dir
                .cmp(&a.is_dir)
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        });
        entries
    }

    /// Directories that are gitignored or hidden aren't scanned until they're opened, so their
    /// contents arrive in a later worktree update, which refreshes the listing.
    fn load_directory(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let unloaded_entry_id = self
            .project
            .read(cx)
            .worktree_for_id(self.worktree_id, cx)
            .and_then(|worktree| {
                let entry = worktree.read(cx).entry_for_path(&self.directory)?;
                (entry.kind == project::EntryKind::UnloadedDir).then_some(entry.id)
            });
        let Some(entry_id) = unloaded_entry_id else {
            return;
        };
        let worktree_id = self.worktree_id;
        if let Some(task) = self.project.update(cx, |project, cx| {
            project.expand_entry(worktree_id, entry_id, cx)
        }) {
            cx.spawn_in(window, async move |_, _| task.await)
                .detach_and_log_err(cx);
        }
    }

    fn refresh(
        &mut self,
        cursor_target: CursorTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let entries = self.read_entries(cx);
        // When the entry under the cursor disappears (e.g. it was deleted), the cursor stays on
        // the same row rather than jumping to the top.
        let (target_name, fallback_row) = match cursor_target {
            CursorTarget::Keep => (
                self.entry_under_cursor(cx).map(|entry| entry.name.clone()),
                self.cursor_row(cx),
            ),
            CursorTarget::Name(name) => (Some(name), 0),
            CursorTarget::First => (None, 0),
        };

        if entries != self.entries {
            self.entries = entries;
            let text = self
                .entries
                .iter()
                .map(BrowserEntry::line)
                .collect::<Vec<_>>()
                .join("\n");
            self.buffer
                .update(cx, |buffer, cx| buffer.set_text(text, cx));
        }

        let row = target_name
            .and_then(|name| self.entries.iter().position(|entry| entry.name == name))
            .unwrap_or_else(|| fallback_row.min(self.entries.len().saturating_sub(1)));
        let cursor = Point::new(row as u32, 0);
        self.editor.update(cx, |editor, cx| {
            editor.change_selections(
                SelectionEffects::scroll(Autoscroll::fit()),
                window,
                cx,
                |selections| selections.select_ranges([cursor..cursor]),
            );
        });
        self.update_highlights(cx);
    }

    fn cursor_row(&self, cx: &mut App) -> usize {
        self.editor.update(cx, |editor, cx| {
            let display_snapshot = editor.display_snapshot(cx);
            editor
                .selections
                .newest::<Point>(&display_snapshot)
                .head()
                .row as usize
        })
    }

    fn entry_under_cursor(&self, cx: &mut App) -> Option<&BrowserEntry> {
        let row = self.cursor_row(cx);
        self.entries.get(row)
    }

    fn update_highlights(&mut self, cx: &mut Context<Self>) {
        let git_store = self.project.read(cx).git_store().read(cx);
        let mut groups: Vec<(Color, Vec<(usize, usize)>)> = Vec::new();
        for (row, entry) in self.entries.iter().enumerate() {
            let project_path = ProjectPath {
                worktree_id: self.worktree_id,
                path: entry.path.clone(),
            };
            let git_status = git_store
                .display_status_for_project_path(&project_path, cx)
                .map(|status| status.summary())
                .unwrap_or_default();
            let mut color = entry_git_aware_label_color(git_status, entry.is_ignored, false);
            if color == Color::Default && entry.is_dir {
                color = Color::Accent;
            }
            if color == Color::Default {
                continue;
            }
            let line_len = entry.line().len();
            match groups
                .iter_mut()
                .find(|(group_color, _)| *group_color == color)
            {
                Some((_, rows)) => rows.push((row, line_len)),
                None => groups.push((color, vec![(row, line_len)])),
            }
        }

        let styled_groups = groups
            .into_iter()
            .map(|(color, rows)| {
                let style = HighlightStyle {
                    color: Some(color.color(cx)),
                    ..Default::default()
                };
                (style, rows)
            })
            .collect::<Vec<_>>();
        let previous_group_count = self.highlight_group_count;
        self.highlight_group_count = styled_groups.len();
        self.editor.update(cx, |editor, cx| {
            for group_index in 0..previous_group_count {
                editor.clear_highlights(HighlightKey::FileBrowser(group_index), cx);
            }
            let snapshot = editor.buffer().read(cx).snapshot(cx);
            for (group_index, (style, rows)) in styled_groups.into_iter().enumerate() {
                let ranges = rows
                    .into_iter()
                    .map(|(row, line_len)| {
                        let row = row as u32;
                        snapshot.anchor_before(Point::new(row, 0))
                            ..snapshot.anchor_after(Point::new(row, line_len as u32))
                    })
                    .collect();
                editor.highlight_text(HighlightKey::FileBrowser(group_index), ranges, style, cx);
            }
        });
    }

    fn navigate_to(
        &mut self,
        directory: Arc<RelPath>,
        cursor_target: CursorTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.directory = directory;
        self.load_directory(window, cx);
        self.refresh(cursor_target, window, cx);
        cx.emit(Event::DirectoryChanged);
    }

    fn open_entry(&mut self, _: &OpenEntry, window: &mut Window, cx: &mut Context<Self>) {
        let Some(entry) = self.entry_under_cursor(cx).cloned() else {
            return;
        };
        if entry.is_dir {
            self.navigate_to(entry.path, CursorTarget::First, window, cx);
            return;
        }

        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let project_path = ProjectPath {
            worktree_id: self.worktree_id,
            path: entry.path,
        };
        let browser = cx.entity();
        let pane = workspace
            .read(cx)
            .pane_for(&browser)
            .unwrap_or_else(|| workspace.read(cx).active_pane().clone());
        let open_task = workspace.update(cx, |workspace, cx| {
            workspace.open_path(project_path, Some(pane.downgrade()), true, window, cx)
        });
        // Like Oil, the file takes the browser's place instead of opening next to it.
        let browser_id = browser.entity_id();
        cx.spawn_in(window, async move |_, cx| {
            open_task.await?;
            pane.update_in(cx, |pane, window, cx| {
                pane.close_item_by_id(browser_id, SaveIntent::Skip, window, cx)
            })?
            .await
        })
        .detach_and_log_err(cx);
    }

    fn go_to_parent(&mut self, _: &GoToParent, window: &mut Window, cx: &mut Context<Self>) {
        let Some(parent) = self.directory.parent() else {
            return;
        };
        let current_name = self.directory.file_name().map(str::to_string);
        let cursor_target = current_name.map_or(CursorTarget::First, CursorTarget::Name);
        self.navigate_to(parent.into(), cursor_target, window, cx);
    }

    fn go_to_root(&mut self, _: &GoToRoot, window: &mut Window, cx: &mut Context<Self>) {
        self.navigate_to(RelPath::empty_arc(), CursorTarget::First, window, cx);
    }

    fn toggle_hidden(&mut self, _: &ToggleHidden, window: &mut Window, cx: &mut Context<Self>) {
        self.show_hidden = !self.show_hidden;
        self.refresh(CursorTarget::Keep, window, cx);
    }

    fn title(&self, cx: &App) -> String {
        let path_style = self.project.read(cx).path_style(cx);
        let root_name = self
            .project
            .read(cx)
            .worktree_for_id(self.worktree_id, cx)
            .map(|worktree| {
                worktree
                    .read(cx)
                    .root_name()
                    .display(path_style)
                    .to_string()
            })
            .unwrap_or_default();
        let separator = path_style.primary_separator();
        if self.directory.is_empty() {
            format!("{root_name}{separator}")
        } else {
            format!(
                "{root_name}{separator}{}{separator}",
                self.directory.display(path_style)
            )
        }
    }
}

impl EventEmitter<Event> for FileBrowser {}

impl Focusable for FileBrowser {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor.focus_handle(cx)
    }
}

impl Render for FileBrowser {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .on_action(cx.listener(Self::open_entry))
            .on_action(cx.listener(Self::go_to_parent))
            .on_action(cx.listener(Self::go_to_root))
            .on_action(cx.listener(Self::toggle_hidden))
            .child(self.editor.clone())
    }
}

impl Item for FileBrowser {
    type Event = Event;

    fn tab_content_text(&self, _detail: usize, cx: &App) -> SharedString {
        self.title(cx).into()
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::Folder))
    }

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        match event {
            Event::DirectoryChanged => f(ItemEvent::UpdateTab),
        }
    }

    fn act_as_type<'a>(
        &'a self,
        type_id: TypeId,
        self_handle: &'a Entity<Self>,
        _: &'a App,
    ) -> Option<AnyEntity> {
        if type_id == TypeId::of::<Self>() {
            Some(self_handle.clone().into())
        } else if type_id == TypeId::of::<Editor>() {
            Some(self.editor.clone().into())
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{TestAppContext, VisualTestContext};
    use serde_json::json;
    use util::{path, rel_path::rel_path};
    use workspace::{AppState, MultiWorkspace};

    fn init_test(cx: &mut TestAppContext) -> Arc<AppState> {
        cx.update(|cx| {
            let state = AppState::test(cx);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            crate::init(cx);
            editor::init(cx);
            state
        })
    }

    async fn setup(cx: &mut TestAppContext) -> (Entity<Workspace>, &mut VisualTestContext) {
        let app_state = init_test(cx);
        app_state
            .fs
            .as_fake()
            .insert_tree(
                path!("/root"),
                json!({
                    ".hidden": "",
                    "b.txt": "",
                    "A.txt": "",
                    "docs": {},
                    "src": {"main.rs": "", "lib.rs": ""},
                }),
            )
            .await;
        let project = Project::test(app_state.fs.clone(), [path!("/root").as_ref()], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
        let workspace =
            multi_workspace.read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone());
        (workspace, cx)
    }

    async fn open_file(file_path: &str, workspace: &Entity<Workspace>, cx: &mut VisualTestContext) {
        let worktree_id = workspace.read_with(cx, |workspace, cx| {
            let worktree = workspace.project().read(cx).worktrees(cx).next();
            worktree.expect("worktree not found").read(cx).id()
        });
        let project_path = ProjectPath {
            worktree_id,
            path: rel_path(file_path).into(),
        };
        workspace
            .update_in(cx, |workspace, window, cx| {
                workspace.open_path(project_path, None, true, window, cx)
            })
            .await
            .expect("failed to open file");
    }

    fn active_browser(
        workspace: &Entity<Workspace>,
        cx: &mut VisualTestContext,
    ) -> Entity<FileBrowser> {
        workspace.read_with(cx, |workspace, cx| {
            workspace
                .active_item_as::<FileBrowser>(cx)
                .expect("the file browser is not the active item")
        })
    }

    #[track_caller]
    fn assert_listing(
        browser: &Entity<FileBrowser>,
        expected_lines: &[&str],
        expected_cursor_row: usize,
        cx: &mut VisualTestContext,
    ) {
        let (text, cursor_row) = browser.update(cx, |browser, cx| {
            (browser.editor.read(cx).text(cx), browser.cursor_row(cx))
        });
        assert_eq!(text, expected_lines.join("\n"));
        assert_eq!(
            cursor_row, expected_cursor_row,
            "cursor is on the wrong entry"
        );
    }

    #[gpui::test]
    async fn test_navigates_directories(cx: &mut TestAppContext) {
        let (workspace, cx) = setup(cx).await;
        open_file("src/main.rs", &workspace, cx).await;

        cx.dispatch_action(Open);
        cx.run_until_parked();
        let browser = active_browser(&workspace, cx);
        assert_listing(&browser, &["lib.rs", "main.rs"], 1, cx);
        browser.read_with(cx, |browser, cx| {
            assert_eq!(browser.title(cx), "root/src/");
        });

        cx.dispatch_action(GoToParent);
        assert_listing(&browser, &["docs/", "src/", "A.txt", "b.txt"], 1, cx);
        browser.read_with(cx, |browser, cx| assert_eq!(browser.title(cx), "root/"));

        cx.dispatch_action(GoToParent);
        assert_listing(&browser, &["docs/", "src/", "A.txt", "b.txt"], 1, cx);

        cx.dispatch_action(ToggleHidden);
        assert_listing(
            &browser,
            &["docs/", "src/", ".hidden", "A.txt", "b.txt"],
            1,
            cx,
        );

        cx.dispatch_action(OpenEntry);
        assert_listing(&browser, &["lib.rs", "main.rs"], 0, cx);

        cx.dispatch_action(GoToRoot);
        assert_listing(
            &browser,
            &["docs/", "src/", ".hidden", "A.txt", "b.txt"],
            0,
            cx,
        );
    }

    #[gpui::test]
    async fn test_opening_a_file_replaces_the_browser(cx: &mut TestAppContext) {
        let (workspace, cx) = setup(cx).await;
        cx.dispatch_action(Open);
        cx.run_until_parked();
        let browser = active_browser(&workspace, cx);
        assert_listing(&browser, &["docs/", "src/", "A.txt", "b.txt"], 0, cx);

        browser.update_in(cx, |browser, window, cx| {
            browser.navigate_to(
                rel_path("src").into(),
                CursorTarget::Name("main.rs".into()),
                window,
                cx,
            );
        });
        cx.dispatch_action(OpenEntry);
        cx.run_until_parked();

        workspace.read_with(cx, |workspace, cx| {
            let pane = workspace.active_pane().read(cx);
            assert_eq!(pane.items_len(), 1, "the browser tab should be closed");
            let editor = workspace
                .active_item_as::<Editor>(cx)
                .expect("the file should be open");
            assert_eq!(editor.read(cx).title(cx), "main.rs");
        });
    }

    #[gpui::test]
    async fn test_vim_keys(cx: &mut TestAppContext) {
        let (workspace, cx) = setup(cx).await;
        cx.update(|_, cx| {
            vim::init(cx);
            cx.update_global::<settings::SettingsStore, _>(|store, cx| {
                store.update_user_settings(cx, |settings| settings.vim_mode = Some(true));
            });
            for (keymap, source) in [
                (
                    "keymaps/default-macos.json",
                    settings::KeybindSource::Default,
                ),
                ("keymaps/vim.json", settings::KeybindSource::Vim),
            ] {
                let mut bindings =
                    settings::KeymapFile::load_asset_allow_partial_failure(keymap, cx)
                        .expect("failed to load keymap");
                for binding in &mut bindings {
                    binding.set_meta(source.meta());
                }
                cx.bind_keys(bindings);
            }
        });
        open_file("src/main.rs", &workspace, cx).await;
        cx.dispatch_action(Open);
        cx.run_until_parked();
        let browser = active_browser(&workspace, cx);
        assert_listing(&browser, &["lib.rs", "main.rs"], 1, cx);

        cx.simulate_keystrokes("-");
        assert_listing(&browser, &["docs/", "src/", "A.txt", "b.txt"], 1, cx);

        cx.simulate_keystrokes("g .");
        assert_listing(
            &browser,
            &["docs/", "src/", ".hidden", "A.txt", "b.txt"],
            1,
            cx,
        );

        cx.simulate_keystrokes("enter");
        assert_listing(&browser, &["lib.rs", "main.rs"], 0, cx);

        cx.simulate_keystrokes("_");
        assert_listing(
            &browser,
            &["docs/", "src/", ".hidden", "A.txt", "b.txt"],
            0,
            cx,
        );

        cx.simulate_keystrokes("4 j enter");
        cx.run_until_parked();
        workspace.read_with(cx, |workspace, cx| {
            let editor = workspace
                .active_item_as::<Editor>(cx)
                .expect("b.txt should be open");
            assert_eq!(editor.read(cx).title(cx), "b.txt");
        });
    }

    #[gpui::test]
    async fn test_listing_follows_file_system_changes(cx: &mut TestAppContext) {
        let (workspace, cx) = setup(cx).await;
        cx.dispatch_action(Open);
        cx.run_until_parked();
        let browser = active_browser(&workspace, cx);
        browser.update_in(cx, |browser, window, cx| {
            browser.refresh(CursorTarget::Name("b.txt".into()), window, cx);
        });
        assert_listing(&browser, &["docs/", "src/", "A.txt", "b.txt"], 3, cx);

        let fs = workspace.read_with(cx, |workspace, _| workspace.app_state().fs.clone());
        fs.as_fake()
            .insert_file(path!("/root/0.txt"), Vec::new())
            .await;
        cx.run_until_parked();
        assert_listing(
            &browser,
            &["docs/", "src/", "0.txt", "A.txt", "b.txt"],
            4,
            cx,
        );
    }
}
