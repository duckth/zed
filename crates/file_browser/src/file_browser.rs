mod changes;

use std::{any::Any, any::TypeId, path::Path, sync::Arc};

use anyhow::{Context as _, Result, anyhow};
use changes::{Change, plan_changes};
use collections::HashMap;

use editor::{
    Addon, Editor, Inlay, SelectionEffects, display_map::HighlightKey,
    items::entry_git_aware_label_color, scroll::Autoscroll,
};
use file_icons::FileIcons;
use gpui::{
    AnyEntity, App, AsyncWindowContext, Context, DismissEvent, Entity, EventEmitter, FocusHandle,
    Focusable, HighlightStyle, KeyContext, ParentElement, PromptLevel, Render, Styled,
    Subscription, Task, TaskExt, WeakEntity, Window, actions,
};
use language::{Buffer, BufferEvent, Point};
use project::{
    Entry, InlayId, Project, ProjectEntryId, ProjectPath, WorktreeId, git_store::GitStoreEvent,
};
use ui::prelude::*;
use util::rel_path::RelPath;
use workspace::{
    ModalView, SaveIntent, Workspace,
    item::{Item, ItemEvent, SaveOptions},
    notifications::DetachAndPromptErr,
};

actions!(
    file_browser,
    [
        /// Opens the file browser at the directory of the active file, or at the project root.
        Open,
        /// Opens the file browser in a floating window, or closes it if it's already open.
        ToggleFloating,
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
            workspace.register_action(|workspace, _: &ToggleFloating, window, cx| {
                FileBrowserModal::toggle(workspace, window, cx);
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
    /// The entries as listed in the buffer before any unsaved edits.
    entries: Vec<BrowserEntry>,
    listing_text: String,
    /// Whether edits to the listing can be saved to the file system.
    editable: bool,
    show_hidden: bool,
    highlight_group_count: usize,
    icon_inlay_ids: Vec<InlayId>,
    next_icon_inlay_id: usize,
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
    /// Keep the cursor on the name it's on, e.g. when the directory's contents change.
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
    Edited,
    OpenedFile,
}

/// What became of unsaved edits after asking the user about them.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Resolution {
    Applied,
    Discarded,
    Cancelled,
}

/// Where a new browser starts.
struct Location {
    worktree_id: WorktreeId,
    directory: Arc<RelPath>,
    selected_name: Option<String>,
}

impl Location {
    /// The directory of the active file with that file selected, or else the project root.
    fn initial(workspace: &Workspace, cx: &App) -> Option<Self> {
        let active_path = workspace
            .active_item(cx)
            .and_then(|item| item.project_path(cx));
        match active_path {
            Some(active_path) => Some(Self {
                worktree_id: active_path.worktree_id,
                directory: active_path
                    .path
                    .parent()
                    .map_or_else(RelPath::empty_arc, Into::into),
                selected_name: active_path.path.file_name().map(str::to_string),
            }),
            None => {
                let worktree = workspace.project().read(cx).visible_worktrees(cx).next()?;
                Some(Self {
                    worktree_id: worktree.read(cx).id(),
                    directory: RelPath::empty_arc(),
                    selected_name: None,
                })
            }
        }
    }
}

impl FileBrowser {
    fn open(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
        let Some(location) = Location::initial(workspace, cx) else {
            return;
        };
        let project = workspace.project().clone();
        let workspace_handle = workspace.weak_handle();
        let browser =
            cx.new(|cx| FileBrowser::new(project, workspace_handle, location, true, window, cx));
        workspace.add_item_to_active_pane(Box::new(browser), None, true, window, cx);
    }

    fn new(
        project: Entity<Project>,
        workspace: WeakEntity<Workspace>,
        location: Location,
        editable: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let Location {
            worktree_id,
            directory,
            selected_name,
        } = location;
        let buffer = cx.new(|cx| Buffer::local("", cx));
        let editor = cx.new(|cx| {
            let mut editor = Editor::for_buffer(buffer.clone(), None, window, cx);
            editor.set_read_only(!editable);
            editor.register_addon(FileBrowserAddon);
            editor
        });

        let buffer_subscription = cx.subscribe(&buffer, |this, _, event: &BufferEvent, cx| {
            if matches!(event, BufferEvent::Edited { .. }) {
                this.update_decorations(cx);
                cx.emit(Event::Edited);
            }
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
                this.update_decorations(cx);
            }
        });
        let theme_subscription =
            cx.observe_global::<theme::GlobalTheme>(|this, cx| this.update_decorations(cx));

        let mut browser = Self {
            editor,
            buffer,
            project,
            workspace,
            worktree_id,
            directory,
            entries: Vec::new(),
            listing_text: String::new(),
            editable,
            show_hidden: false,
            highlight_group_count: 0,
            icon_inlay_ids: Vec::new(),
            next_icon_inlay_id: 0,
            _subscriptions: vec![
                buffer_subscription,
                project_subscription,
                git_subscription,
                theme_subscription,
            ],
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

    /// Lists the directory again, unless there are unsaved edits, which would be lost.
    fn refresh(
        &mut self,
        cursor_target: CursorTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.has_unsaved_edits(cx) {
            return;
        }
        let entries = self.read_entries(cx);
        // When the entry under the cursor disappears (e.g. it was deleted), the cursor stays on
        // the same row rather than jumping to the top.
        let (target_name, fallback_row) = match cursor_target {
            CursorTarget::Keep => (self.name_under_cursor(cx), self.cursor_row(cx)),
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
            self.listing_text = text.clone();
            self.set_text_without_undo(text, cx);
        }

        let row = target_name
            .and_then(|name| self.entries.iter().position(|entry| entry.name == name))
            .unwrap_or(fallback_row);
        self.select_row(row, window, cx);
        self.update_decorations(cx);
    }

    fn select_row(&mut self, row: usize, window: &mut Window, cx: &mut Context<Self>) {
        let row = row.min(self.entries.len().saturating_sub(1));
        let cursor = Point::new(row as u32, 0);
        self.editor.update(cx, |editor, cx| {
            editor.change_selections(
                SelectionEffects::scroll(Autoscroll::fit()),
                window,
                cx,
                |selections| selections.select_ranges([cursor..cursor]),
            );
        });
    }

    /// Replaces the buffer's text and clears its undo history, so that undo can't bring back the
    /// listing of another directory, or edits that were already applied or discarded.
    fn set_text_without_undo(&mut self, text: String, cx: &mut Context<Self>) {
        self.buffer.update(cx, |buffer, cx| {
            buffer.set_text(text, cx);
            while let Some(transaction_id) = buffer
                .peek_undo_stack()
                .or_else(|| buffer.peek_redo_stack())
                .map(|entry| entry.transaction_id())
            {
                buffer.forget_transaction(transaction_id);
            }
        });
    }

    fn has_unsaved_edits(&self, cx: &App) -> bool {
        self.editable && self.buffer.read(cx).text() != self.listing_text
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

    fn name_under_cursor(&self, cx: &mut App) -> Option<String> {
        let row = self.cursor_row(cx);
        let text = self.buffer.read(cx).text();
        let line = text.lines().nth(row)?.trim().trim_end_matches('/');
        (!line.is_empty()).then(|| line.to_string())
    }

    fn entry_under_cursor(&self, cx: &mut App) -> Option<&BrowserEntry> {
        let row = self.cursor_row(cx);
        self.entries.get(row)
    }

    /// Colors and icons follow the text rather than the listed entries, so that they stay on the
    /// right lines while the listing is edited.
    fn update_decorations(&mut self, cx: &mut Context<Self>) {
        let text = self.buffer.read(cx).text();
        let entries_by_line = self
            .entries
            .iter()
            .map(|entry| (entry.line(), entry))
            .collect::<HashMap<_, _>>();
        let git_store = self.project.read(cx).git_store().read(cx);

        let mut icon_paths = Vec::new();
        let mut groups: Vec<(Color, Vec<(usize, usize)>)> = Vec::new();
        for (row, line) in text.lines().enumerate() {
            let name = line.trim();
            if name.is_empty() {
                continue;
            }
            let entry = entries_by_line.get(name);
            let is_dir = entry.map_or_else(|| name.ends_with('/'), |entry| entry.is_dir);
            let path = Path::new(name.trim_end_matches('/'));
            let icon_path = if is_dir {
                FileIcons::get_folder_icon(false, path, cx)
            } else {
                FileIcons::get_icon(path, cx)
            };
            if let Some(icon_path) = icon_path {
                icon_paths.push((row, icon_path));
            }

            let mut color = entry.map_or(Color::Default, |entry| {
                let project_path = ProjectPath {
                    worktree_id: self.worktree_id,
                    path: entry.path.clone(),
                };
                let git_status = git_store
                    .display_status_for_project_path(&project_path, cx)
                    .map(|status| status.summary())
                    .unwrap_or_default();
                entry_git_aware_label_color(git_status, entry.is_ignored, false)
            });
            if color == Color::Default && is_dir {
                color = Color::Accent;
            }
            if color == Color::Default {
                continue;
            }
            match groups
                .iter_mut()
                .find(|(group_color, _)| *group_color == color)
            {
                Some((_, rows)) => rows.push((row, line.len())),
                None => groups.push((color, vec![(row, line.len())])),
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
        let first_inlay_id = self.next_icon_inlay_id;
        self.next_icon_inlay_id += icon_paths.len();
        let previous_inlay_ids = std::mem::take(&mut self.icon_inlay_ids);
        self.icon_inlay_ids = self.editor.update(cx, |editor, cx| {
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

            // Icons are inlays rather than buffer text, so that motions, search and edits only
            // ever see the names. Anchoring before the line start keeps the cursor after the icon.
            let inlays = icon_paths
                .into_iter()
                .enumerate()
                .map(|(index, (row, icon_path))| {
                    let position = snapshot.anchor_before(Point::new(row as u32, 0));
                    Inlay::icon(first_inlay_id + index, position, icon_path)
                })
                .collect::<Vec<_>>();
            let inlay_ids = inlays.iter().map(|inlay| inlay.id).collect();
            editor.splice_inlays(&previous_inlay_ids, inlays, cx);
            inlay_ids
        });
    }

    fn plan_changes(&self, cx: &App) -> Result<Vec<Change>> {
        let worktree = self
            .project
            .read(cx)
            .worktree_for_id(self.worktree_id, cx)
            .map(|worktree| worktree.read(cx).snapshot());
        plan_changes(
            &self.directory,
            &self.entries,
            &self.buffer.read(cx).text(),
            |path| {
                worktree
                    .as_ref()
                    .is_some_and(|worktree| worktree.entry_for_path(path).is_some())
            },
        )
    }

    /// Asks whether to apply the unsaved edits to the file system, listing the changes that
    /// would be made, and applies them if so. When `can_discard` is set, the edits can also be
    /// thrown away instead.
    fn resolve_edits(
        &mut self,
        can_discard: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<Resolution>> {
        if !self.has_unsaved_edits(cx) {
            return Task::ready(Ok(Resolution::Applied));
        }
        let plan = self.plan_changes(cx);
        if let Ok(changes) = &plan
            && changes.is_empty()
        {
            // Only blank lines or whitespace changed.
            self.listing_text = self.buffer.read(cx).text();
            self.refresh(CursorTarget::Keep, window, cx);
            cx.emit(Event::Edited);
            return Task::ready(Ok(Resolution::Applied));
        }

        const APPLY: &str = "Apply";
        const DISCARD: &str = "Discard";
        const CANCEL: &str = "Cancel";
        let (level, message, detail, answers) = match &plan {
            Ok(changes) => {
                let level = if changes.iter().any(Change::is_trash) {
                    PromptLevel::Warning
                } else {
                    PromptLevel::Info
                };
                let message = if can_discard {
                    "Apply your changes before leaving this directory?".to_string()
                } else if changes.len() == 1 {
                    "Apply this change?".to_string()
                } else {
                    format!("Apply these {} changes?", changes.len())
                };
                let detail = changes
                    .iter()
                    .map(Change::description)
                    .collect::<Vec<_>>()
                    .join("\n");
                let answers = if can_discard {
                    vec![APPLY, DISCARD, CANCEL]
                } else {
                    vec![APPLY, CANCEL]
                };
                (level, message, detail, answers)
            }
            Err(error) => {
                let answers = if can_discard {
                    vec![DISCARD, CANCEL]
                } else {
                    vec![CANCEL]
                };
                (
                    PromptLevel::Critical,
                    "Your changes can't be applied".to_string(),
                    error.to_string(),
                    answers,
                )
            }
        };
        let prompt = window.prompt(level, &message, Some(&detail), &answers, cx);
        let project = self.project.clone();
        let worktree_id = self.worktree_id;
        cx.spawn_in(window, async move |this, cx| {
            let answer = answers.get(prompt.await?).copied();
            match (answer, plan) {
                (Some(APPLY), Ok(changes)) => {
                    let result = apply_changes(&project, worktree_id, changes, cx).await;
                    // Even when a change failed, the ones before it were made, so the listing
                    // is reloaded to show what's actually on disk.
                    this.update_in(cx, |this, window, cx| {
                        this.listing_text = this.buffer.read(cx).text();
                        this.entries.clear();
                        this.refresh(CursorTarget::Keep, window, cx);
                        cx.emit(Event::Edited);
                    })?;
                    result?;
                    Ok(Resolution::Applied)
                }
                (Some(DISCARD), _) => {
                    this.update_in(cx, |this, window, cx| {
                        let row = this.cursor_row(cx);
                        let listing_text = this.listing_text.clone();
                        this.set_text_without_undo(listing_text, cx);
                        this.select_row(row, window, cx);
                    })?;
                    Ok(Resolution::Discarded)
                }
                _ => Ok(Resolution::Cancelled),
            }
        })
    }

    /// Runs `action` once there are no unsaved edits, asking what to do with any first.
    fn after_resolving_edits(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        action: impl FnOnce(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) {
        if !self.has_unsaved_edits(cx) {
            action(self, window, cx);
            return;
        }
        let resolve = self.resolve_edits(true, window, cx);
        cx.spawn_in(window, async move |this, cx| {
            if resolve.await? != Resolution::Cancelled {
                this.update_in(cx, |this, window, cx| action(this, window, cx))?;
            }
            anyhow::Ok(())
        })
        .detach_and_prompt_err("Failed to apply changes", window, cx, |_, _, _| None);
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
        self.after_resolving_edits(window, cx, Self::open_entry_under_cursor);
    }

    fn open_entry_under_cursor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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
        let browser_pane = workspace.read(cx).pane_for(&browser);
        let target_pane = browser_pane
            .clone()
            .unwrap_or_else(|| workspace.read(cx).active_pane().clone());
        let open_task = workspace.update(cx, |workspace, cx| {
            workspace.open_path(
                project_path,
                Some(target_pane.downgrade()),
                true,
                window,
                cx,
            )
        });
        let Some(pane) = browser_pane else {
            // A floating browser isn't in a pane; its modal closes itself on this event.
            cx.emit(Event::OpenedFile);
            open_task.detach_and_log_err(cx);
            return;
        };
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
        self.after_resolving_edits(window, cx, |this, window, cx| {
            let Some(parent) = this.directory.parent() else {
                return;
            };
            let current_name = this.directory.file_name().map(str::to_string);
            let cursor_target = current_name.map_or(CursorTarget::First, CursorTarget::Name);
            this.navigate_to(parent.into(), cursor_target, window, cx);
        });
    }

    fn go_to_root(&mut self, _: &GoToRoot, window: &mut Window, cx: &mut Context<Self>) {
        self.after_resolving_edits(window, cx, |this, window, cx| {
            this.navigate_to(RelPath::empty_arc(), CursorTarget::First, window, cx);
        });
    }

    fn toggle_hidden(&mut self, _: &ToggleHidden, window: &mut Window, cx: &mut Context<Self>) {
        self.after_resolving_edits(window, cx, |this, window, cx| {
            this.show_hidden = !this.show_hidden;
            this.refresh(CursorTarget::Keep, window, cx);
        });
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

async fn apply_changes(
    project: &Entity<Project>,
    worktree_id: WorktreeId,
    changes: Vec<Change>,
    cx: &mut AsyncWindowContext,
) -> Result<()> {
    for change in changes {
        let result = match &change {
            Change::CreateDirectory { path } | Change::CreateFile { path } => {
                let is_directory = matches!(change, Change::CreateDirectory { .. });
                let project_path = ProjectPath {
                    worktree_id,
                    path: path.clone(),
                };
                project
                    .update(cx, |project, cx| {
                        project.create_entry(project_path, is_directory, cx)
                    })
                    .await
                    .map(drop)
            }
            Change::Rename { entry_id, to, .. } => {
                let project_path = ProjectPath {
                    worktree_id,
                    path: to.clone(),
                };
                project
                    .update(cx, |project, cx| {
                        project.rename_entry(*entry_id, project_path, cx)
                    })
                    .await
                    .map(drop)
            }
            Change::Trash { entry_id, .. } => {
                match project.update(cx, |project, cx| project.trash_entry(*entry_id, cx)) {
                    Some(task) => task.await.map(drop),
                    None => Err(anyhow!("it no longer exists")),
                }
            }
        };
        result.with_context(|| format!("Couldn't apply: {}", change.description()))?;
    }
    Ok(())
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

    fn added_to_workspace(
        &mut self,
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Vim's `:w` saves through the workspace the editor belongs to.
        self.editor.update(cx, |editor, cx| {
            editor.added_to_workspace(workspace, window, cx)
        });
    }

    fn is_dirty(&self, cx: &App) -> bool {
        self.has_unsaved_edits(cx)
    }

    fn can_save(&self, _: &App) -> bool {
        self.editable
    }

    fn save(
        &mut self,
        options: SaveOptions,
        _: Entity<Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        // Changes to the file system are only made when asked for explicitly.
        if options.autosave {
            return Task::ready(Ok(()));
        }
        let resolve = self.resolve_edits(false, window, cx);
        cx.background_spawn(async move { resolve.await.map(|_| ()) })
    }

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        match event {
            Event::DirectoryChanged => f(ItemEvent::UpdateTab),
            Event::Edited => {
                f(ItemEvent::Edit);
                f(ItemEvent::UpdateTab);
            }
            Event::OpenedFile => {}
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

pub struct FileBrowserModal {
    browser: Entity<FileBrowser>,
    _subscription: Subscription,
}

impl FileBrowserModal {
    fn toggle(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
        let Some(location) = Location::initial(workspace, cx) else {
            return;
        };
        let project = workspace.project().clone();
        let workspace_handle = workspace.weak_handle();
        workspace.toggle_modal(window, cx, |window, cx| {
            // Saving from a modal isn't possible: `:` would close it, and there is no tab to show
            // unsaved changes on.
            let browser = cx
                .new(|cx| FileBrowser::new(project, workspace_handle, location, false, window, cx));
            let subscription = cx.subscribe(&browser, |_, _, event: &Event, cx| match event {
                // The modal shows the directory in its header.
                Event::DirectoryChanged => cx.notify(),
                Event::Edited => {}
                Event::OpenedFile => cx.emit(DismissEvent),
            });
            Self {
                browser,
                _subscription: subscription,
            }
        });
    }

    fn dismiss(&mut self, _: &menu::Cancel, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }
}

impl EventEmitter<DismissEvent> for FileBrowserModal {}

impl ModalView for FileBrowserModal {}

impl Focusable for FileBrowserModal {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.browser.focus_handle(cx)
    }
}

impl Render for FileBrowserModal {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let viewport_size = window.viewport_size();
        let title = self.browser.read(cx).title(cx);
        v_flex()
            .key_context("FileBrowserModal")
            .on_action(cx.listener(Self::dismiss))
            .w(rems(40.))
            .max_w(viewport_size.width * 0.9)
            .h(viewport_size.height * 0.6)
            .elevation_3(cx)
            .overflow_hidden()
            .child(
                h_flex()
                    .flex_none()
                    .gap_1p5()
                    .px_2()
                    .py_1()
                    .border_b_1()
                    .border_color(cx.theme().colors().border_variant)
                    .child(
                        Icon::new(IconName::Folder)
                            .size(IconSize::Small)
                            .color(Color::Muted),
                    )
                    .child(Label::new(title).size(LabelSize::Small).color(Color::Muted)),
            )
            .child(div().flex_1().min_h_0().child(self.browser.clone()))
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
        build_workspace(app_state, cx).await
    }

    async fn setup_with_vim(
        cx: &mut TestAppContext,
    ) -> (Entity<Workspace>, &mut VisualTestContext) {
        let app_state = init_test(cx);
        // Vim registers some of its actions (e.g. `:`) on workspaces as they're created.
        enable_vim(cx);
        build_workspace(app_state, cx).await
    }

    async fn build_workspace(
        app_state: Arc<AppState>,
        cx: &mut TestAppContext,
    ) -> (Entity<Workspace>, &mut VisualTestContext) {
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
    async fn test_shows_an_icon_before_each_entry(cx: &mut TestAppContext) {
        let (workspace, cx) = setup(cx).await;
        open_file("src/main.rs", &workspace, cx).await;
        cx.dispatch_action(Open);
        cx.run_until_parked();
        let browser = active_browser(&workspace, cx);

        let display_text = |browser: &Entity<FileBrowser>, cx: &mut VisualTestContext| {
            browser.update(cx, |browser, cx| {
                browser
                    .editor
                    .update(cx, |editor, cx| editor.display_text(cx))
            })
        };
        assert_eq!(display_text(&browser, cx), "▫lib.rs\n▫main.rs");
        let cursor_column = browser.update(cx, |browser, cx| {
            browser.editor.update(cx, |editor, cx| {
                let display_snapshot = editor.display_snapshot(cx);
                editor
                    .selections
                    .newest_display(&display_snapshot)
                    .head()
                    .column()
            })
        });
        assert_eq!(
            cursor_column as usize,
            "▫".len(),
            "the cursor should be after the icon"
        );

        cx.dispatch_action(GoToParent);
        assert_listing(&browser, &["docs/", "src/", "A.txt", "b.txt"], 1, cx);
        assert_eq!(display_text(&browser, cx), "▫docs/\n▫src/\n▫A.txt\n▫b.txt");
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

    fn enable_vim(cx: &mut TestAppContext) {
        cx.update(|cx| {
            command_palette::init(cx);
            vim::init(cx);
        });
        // Vim only notices the setting change (and e.g. handles `:w`) once it's observing settings.
        cx.update(|cx| {
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
    }

    #[gpui::test]
    async fn test_vim_keys(cx: &mut TestAppContext) {
        let (workspace, cx) = setup_with_vim(cx).await;
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

    fn floating_browser(
        workspace: &Entity<Workspace>,
        cx: &mut VisualTestContext,
    ) -> Option<Entity<FileBrowser>> {
        workspace.read_with(cx, |workspace, cx| {
            workspace
                .active_modal::<FileBrowserModal>(cx)
                .map(|modal| modal.read(cx).browser.clone())
        })
    }

    #[gpui::test]
    async fn test_floating_browser(cx: &mut TestAppContext) {
        let (workspace, cx) = setup_with_vim(cx).await;
        open_file("src/main.rs", &workspace, cx).await;

        cx.dispatch_action(ToggleFloating);
        cx.run_until_parked();
        let browser = floating_browser(&workspace, cx).expect("the browser should be floating");
        assert_listing(&browser, &["lib.rs", "main.rs"], 1, cx);

        cx.simulate_keystrokes("-");
        assert_listing(&browser, &["docs/", "src/", "A.txt", "b.txt"], 1, cx);
        cx.simulate_keystrokes(": / n");
        cx.run_until_parked();
        assert!(
            floating_browser(&workspace, cx).is_some(),
            "the command line and search shouldn't replace the browser"
        );
        cx.simulate_keystrokes("q");
        assert!(
            floating_browser(&workspace, cx).is_none(),
            "q should close it"
        );

        cx.dispatch_action(ToggleFloating);
        cx.run_until_parked();
        assert!(floating_browser(&workspace, cx).is_some());
        cx.simulate_keystrokes("escape");
        assert!(
            floating_browser(&workspace, cx).is_none(),
            "escape should close it"
        );

        cx.dispatch_action(ToggleFloating);
        cx.run_until_parked();
        cx.simulate_keystrokes("k enter");
        cx.run_until_parked();
        assert!(
            floating_browser(&workspace, cx).is_none(),
            "opening a file should close it"
        );
        workspace.read_with(cx, |workspace, cx| {
            let pane = workspace.active_pane().read(cx);
            assert_eq!(pane.items_len(), 2);
            let editor = workspace
                .active_item_as::<Editor>(cx)
                .expect("lib.rs should be open");
            assert_eq!(editor.read(cx).title(cx), "lib.rs");
        });
        cx.update(|window, cx| {
            let editor = workspace
                .read(cx)
                .active_item_as::<Editor>(cx)
                .expect("lib.rs should be open");
            assert!(
                editor.focus_handle(cx).is_focused(window),
                "the opened file should have focus"
            );
        });
    }

    fn set_listing_text(browser: &Entity<FileBrowser>, text: &str, cx: &mut VisualTestContext) {
        browser.update_in(cx, |browser, window, cx| {
            browser
                .editor
                .update(cx, |editor, cx| editor.set_text(text, window, cx));
        });
    }

    fn is_dirty(browser: &Entity<FileBrowser>, cx: &mut VisualTestContext) -> bool {
        browser.read_with(cx, |browser, cx| browser.is_dirty(cx))
    }

    fn project_paths(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> Vec<String> {
        let fs = workspace.read_with(cx, |workspace, _| workspace.app_state().fs.clone());
        let mut paths = fs
            .as_fake()
            .paths(false)
            .into_iter()
            .filter_map(|path| {
                let path = path.strip_prefix(path!("/root")).ok()?;
                let path = path.to_str()?.replace('\\', "/");
                (!path.is_empty()).then_some(path)
            })
            .collect::<Vec<_>>();
        paths.sort();
        paths
    }

    #[gpui::test]
    async fn test_saving_applies_edits(cx: &mut TestAppContext) {
        let (workspace, cx) = setup(cx).await;
        cx.dispatch_action(Open);
        cx.run_until_parked();
        let browser = active_browser(&workspace, cx);

        set_listing_text(&browser, "documents/\nsrc/\nA.txt\nnew/file.txt\n", cx);
        assert!(is_dirty(&browser, cx));
        let save = workspace.update_in(cx, |workspace, window, cx| {
            workspace.save_active_item(SaveIntent::Save, window, cx)
        });
        cx.run_until_parked();
        assert_eq!(
            cx.pending_prompt(),
            Some((
                "Apply these 3 changes?".to_string(),
                "Create new/\nRename docs → documents\nRename b.txt → new/file.txt".to_string()
            ))
        );
        cx.simulate_prompt_answer("Apply");
        save.await.expect("saving failed");
        cx.run_until_parked();

        assert!(!is_dirty(&browser, cx));
        // The cursor was on the last line, and stays on that row.
        assert_listing(&browser, &["documents/", "new/", "src/", "A.txt"], 3, cx);
        assert_eq!(
            project_paths(&workspace, cx),
            [
                ".hidden",
                "A.txt",
                "documents",
                "new",
                "new/file.txt",
                "src",
                "src/lib.rs",
                "src/main.rs",
            ]
        );
    }

    #[gpui::test]
    async fn test_cancelling_a_save_keeps_the_edits(cx: &mut TestAppContext) {
        let (workspace, cx) = setup(cx).await;
        cx.dispatch_action(Open);
        cx.run_until_parked();
        let browser = active_browser(&workspace, cx);

        set_listing_text(&browser, "docs/\nsrc/\nA.txt\nc.txt", cx);
        let save = workspace.update_in(cx, |workspace, window, cx| {
            workspace.save_active_item(SaveIntent::Save, window, cx)
        });
        cx.run_until_parked();
        cx.simulate_prompt_answer("Cancel");
        save.await.expect("saving failed");
        assert!(is_dirty(&browser, cx));
        assert!(project_paths(&workspace, cx).contains(&"b.txt".to_string()));
    }

    #[gpui::test]
    async fn test_leaving_a_directory_with_edits_asks_first(cx: &mut TestAppContext) {
        let (workspace, cx) = setup(cx).await;
        cx.dispatch_action(Open);
        cx.run_until_parked();
        let browser = active_browser(&workspace, cx);
        let edited_text = "docs/\nsrc/\nA.txt\nc.txt";
        set_listing_text(&browser, edited_text, cx);
        browser.update_in(cx, |browser, window, cx| browser.select_row(1, window, cx));

        cx.dispatch_action(OpenEntry);
        cx.run_until_parked();
        assert_eq!(
            cx.pending_prompt(),
            Some((
                "Apply your changes before leaving this directory?".to_string(),
                "Rename b.txt → c.txt".to_string()
            ))
        );
        cx.simulate_prompt_answer("Cancel");
        cx.run_until_parked();
        browser.read_with(cx, |browser, cx| {
            assert_eq!(browser.title(cx), "root/");
            assert_eq!(browser.buffer.read(cx).text(), edited_text);
        });

        cx.dispatch_action(OpenEntry);
        cx.run_until_parked();
        cx.simulate_prompt_answer("Discard");
        cx.run_until_parked();
        assert!(!is_dirty(&browser, cx));
        assert_listing(&browser, &["lib.rs", "main.rs"], 0, cx);
        assert!(project_paths(&workspace, cx).contains(&"b.txt".to_string()));
    }

    #[gpui::test]
    async fn test_editing_with_vim(cx: &mut TestAppContext) {
        let (workspace, cx) = setup_with_vim(cx).await;
        open_file("b.txt", &workspace, cx).await;
        cx.dispatch_action(Open);
        cx.run_until_parked();
        let browser = active_browser(&workspace, cx);
        assert_listing(&browser, &["docs/", "src/", "A.txt", "b.txt"], 3, cx);

        cx.simulate_keystrokes("d d : w");
        // The command palette matches commands in the background.
        cx.run_until_parked();
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        assert_eq!(
            cx.pending_prompt(),
            Some(("Apply this change?".to_string(), "Trash b.txt".to_string()))
        );
        cx.simulate_prompt_answer("Apply");
        cx.run_until_parked();
        assert_listing(&browser, &["docs/", "src/", "A.txt"], 2, cx);
        assert!(!project_paths(&workspace, cx).contains(&"b.txt".to_string()));

        // Undo can't bring back a listing that no longer matches the file system.
        cx.simulate_keystrokes("- u");
        assert!(!is_dirty(&browser, cx));
        cx.simulate_keystrokes("k enter");
        assert_listing(&browser, &["lib.rs", "main.rs"], 0, cx);
        cx.simulate_keystrokes("u");
        assert!(!is_dirty(&browser, cx));
        assert_listing(&browser, &["lib.rs", "main.rs"], 0, cx);
    }
}
