use std::{path::PathBuf, sync::Arc};

use fuzzy::{StringMatch, StringMatchCandidate};
use gpui::{
    actions, rems, AppContext, DismissEvent, EventEmitter, FocusableView, InteractiveElement,
    Model, ParentElement, Render, SharedString, Styled, Subscription, View, ViewContext,
    VisualContext, WeakView,
};
use picker::{highlighted_match_with_paths::HighlightedMatchWithPaths, Picker, PickerDelegate};
use project::{Inventory, ProjectPath, WorktreeId};
use task::{oneshot_source::OneshotSource, Task};
use ui::{v_flex, ListItem, ListItemSpacing, RenderOnce, Selectable, WindowContext};
use util::ResultExt;
use workspace::{ModalView, Workspace};

use crate::schedule_task;

actions!(task, [Spawn, Rerun]);

/// A modal used to spawn new tasks.
pub(crate) struct TasksModalDelegate {
    inventory: Model<Inventory>,
    candidates: Vec<(Option<WorktreeId>, Arc<dyn Task>)>,
    matches: Vec<StringMatch>,
    selected_index: usize,
    workspace: WeakView<Workspace>,
    prompt: String,
}

impl TasksModalDelegate {
    fn new(inventory: Model<Inventory>, workspace: WeakView<Workspace>) -> Self {
        Self {
            inventory,
            workspace,
            candidates: Vec::new(),
            matches: Vec::new(),
            selected_index: 0,
            prompt: String::default(),
        }
    }

    fn spawn_oneshot(&mut self, cx: &mut AppContext) -> Option<Arc<dyn Task>> {
        self.inventory
            .update(cx, |inventory, _| inventory.source::<OneshotSource>())?
            .update(cx, |oneshot_source, _| {
                Some(
                    oneshot_source
                        .as_any()
                        .downcast_mut::<OneshotSource>()?
                        .spawn(self.prompt.clone()),
                )
            })
    }

    fn active_item_path(
        &mut self,
        cx: &mut ViewContext<'_, Picker<Self>>,
    ) -> Option<(PathBuf, ProjectPath)> {
        let workspace = self.workspace.upgrade()?.read(cx);
        let project = workspace.project().read(cx);
        let active_item = workspace.active_item(cx)?;
        active_item.project_path(cx).and_then(|project_path| {
            project
                .worktree_for_id(project_path.worktree_id, cx)
                .map(|worktree| worktree.read(cx).abs_path().join(&project_path.path))
                .zip(Some(project_path))
        })
    }
}

pub(crate) struct TasksModal {
    picker: View<Picker<TasksModalDelegate>>,
    _subscription: Subscription,
}

impl TasksModal {
    pub(crate) fn new(
        inventory: Model<Inventory>,
        workspace: WeakView<Workspace>,
        cx: &mut ViewContext<Self>,
    ) -> Self {
        let picker = cx
            .new_view(|cx| Picker::uniform_list(TasksModalDelegate::new(inventory, workspace), cx));
        let _subscription = cx.subscribe(&picker, |_, _, _, cx| {
            cx.emit(DismissEvent);
        });
        Self {
            picker,
            _subscription,
        }
    }
}

impl Render for TasksModal {
    fn render(&mut self, cx: &mut ViewContext<Self>) -> impl gpui::prelude::IntoElement {
        v_flex()
            .w(rems(34.))
            .child(self.picker.clone())
            .on_mouse_down_out(cx.listener(|modal, _, cx| {
                modal.picker.update(cx, |picker, cx| {
                    picker.cancel(&Default::default(), cx);
                })
            }))
    }
}

impl EventEmitter<DismissEvent> for TasksModal {}

impl FocusableView for TasksModal {
    fn focus_handle(&self, cx: &gpui::AppContext) -> gpui::FocusHandle {
        self.picker.read(cx).focus_handle(cx)
    }
}

impl ModalView for TasksModal {}

impl PickerDelegate for TasksModalDelegate {
    type ListItem = ListItem;

    fn match_count(&self) -> usize {
        self.matches.len()
    }

    fn selected_index(&self) -> usize {
        self.selected_index
    }

    fn set_selected_index(&mut self, ix: usize, _cx: &mut ViewContext<picker::Picker<Self>>) {
        self.selected_index = ix;
    }

    fn placeholder_text(&self, cx: &mut WindowContext) -> Arc<str> {
        Arc::from(format!(
            "{} runs the selected task, {} spawns a bash-like task from the prompt",
            cx.keystroke_text_for(&menu::Confirm),
            cx.keystroke_text_for(&menu::SecondaryConfirm),
        ))
    }

    fn update_matches(
        &mut self,
        query: String,
        cx: &mut ViewContext<picker::Picker<Self>>,
    ) -> gpui::Task<()> {
        cx.spawn(move |picker, mut cx| async move {
            let Some(candidates) = picker
                .update(&mut cx, |picker, cx| {
                    let (path, worktree) = match picker.delegate.active_item_path(cx) {
                        Some((abs_path, project_path)) => {
                            (Some(abs_path), Some(project_path.worktree_id))
                        }
                        None => (None, None),
                    };
                    picker.delegate.candidates =
                        picker.delegate.inventory.update(cx, |inventory, cx| {
                            inventory.list_tasks(path.as_deref(), worktree, true, cx)
                        });
                    picker
                        .delegate
                        .candidates
                        .iter()
                        .enumerate()
                        .map(|(index, (_, candidate))| StringMatchCandidate {
                            id: index,
                            char_bag: candidate.name().chars().collect(),
                            string: candidate.name().into(),
                        })
                        .collect::<Vec<_>>()
                })
                .ok()
            else {
                return;
            };
            let matches = fuzzy::match_strings(
                &candidates,
                &query,
                true,
                1000,
                &Default::default(),
                cx.background_executor().clone(),
            )
            .await;
            picker
                .update(&mut cx, |picker, _| {
                    let delegate = &mut picker.delegate;
                    delegate.matches = matches;
                    delegate.prompt = query;

                    if delegate.matches.is_empty() {
                        delegate.selected_index = 0;
                    } else {
                        delegate.selected_index =
                            delegate.selected_index.min(delegate.matches.len() - 1);
                    }
                })
                .log_err();
        })
    }

    fn confirm(&mut self, secondary: bool, cx: &mut ViewContext<picker::Picker<Self>>) {
        let current_match_index = self.selected_index();
        let task = if secondary {
            if !self.prompt.trim().is_empty() {
                self.spawn_oneshot(cx)
            } else {
                None
            }
        } else {
            self.matches.get(current_match_index).map(|current_match| {
                let ix = current_match.candidate_id;
                self.candidates[ix].1.clone()
            })
        };

        let Some(task) = task else {
            return;
        };

        self.workspace
            .update(cx, |workspace, cx| {
                schedule_task(workspace, task.as_ref(), cx);
            })
            .ok();
        cx.emit(DismissEvent);
    }

    fn dismissed(&mut self, cx: &mut ViewContext<picker::Picker<Self>>) {
        cx.emit(DismissEvent);
    }

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        cx: &mut ViewContext<picker::Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let hit = &self.matches[ix];
        let mut related_paths = Vec::new();
        let (worktree_id, _) = self.candidates[hit.candidate_id];
        if let Some(worktree_abs_path) = worktree_id.and_then(|worktree_id| {
            self.workspace
                .update(cx, |workspace, cx| {
                    Some(
                        workspace
                            .project()
                            .read(cx)
                            .worktree_for_id(worktree_id, cx)?
                            .read(cx)
                            .abs_path()
                            .to_path_buf(),
                    )
                })
                .ok()
                .flatten()
        }) {
            related_paths.push(worktree_abs_path);
        }

        let highlighted_location = HighlightedMatchWithPaths::new(hit, &related_paths, true);
        Some(
            ListItem::new(SharedString::from(format!("tasks-modal-{ix}")))
                .inset(true)
                .spacing(ListItemSpacing::Sparse)
                .selected(selected)
                .child(highlighted_location.render(cx)),
        )
    }
}
