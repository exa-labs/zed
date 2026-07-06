//! A panel that displays the Lean proof state (goals and expected type) at the
//! cursor, similar to the Lean infoview in VS Code. It talks to the Lean
//! language server using the `$/lean/plainGoal` and `$/lean/plainTermGoal`
//! extension requests.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result};
use editor::{Editor, EditorEvent};
use gpui::{
    App, AsyncWindowContext, Context, Entity, EventEmitter, FocusHandle, Focusable, Pixels,
    Subscription, Task, WeakEntity, Window, actions, div, prelude::*, px,
};
use language::{Buffer, point_to_lsp};
use lsp::LanguageServer;
use project::Project;
use serde::{Deserialize, Serialize};
use settings::Settings;
use text::ToPointUtf16;
use theme_settings::ThemeSettings;
use ui::{Divider, IconName, Label, LabelCommon, LabelSize, prelude::*, v_flex};
use util::ResultExt;
use workspace::{
    Workspace,
    dock::{DockPosition, Panel, PanelEvent},
};

const UPDATE_DEBOUNCE: Duration = Duration::from_millis(75);
const DEFAULT_WIDTH: Pixels = px(360.);

enum LeanPlainGoalRequest {}

impl lsp::request::Request for LeanPlainGoalRequest {
    type Params = lsp::TextDocumentPositionParams;
    type Result = Option<PlainGoal>;
    const METHOD: &'static str = "$/lean/plainGoal";
}

#[derive(Debug, Serialize, Deserialize)]
struct PlainGoal {
    #[allow(dead_code)]
    rendered: String,
    goals: Vec<String>,
}

enum LeanPlainTermGoalRequest {}

impl lsp::request::Request for LeanPlainTermGoalRequest {
    type Params = lsp::TextDocumentPositionParams;
    type Result = Option<PlainTermGoal>;
    const METHOD: &'static str = "$/lean/plainTermGoal";
}

#[derive(Debug, Serialize, Deserialize)]
struct PlainTermGoal {
    goal: String,
}

actions!(
    lean_infoview,
    [
        /// Toggles focus on the Lean infoview panel.
        ToggleFocus
    ]
);

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
            workspace.toggle_panel_focus::<LeanInfoView>(window, cx);
        });
    })
    .detach();
}

#[derive(Default)]
enum InfoViewState {
    #[default]
    NoLeanBuffer,
    Loading,
    Loaded {
        goals: Vec<String>,
        term_goal: Option<String>,
    },
}

pub struct LeanInfoView {
    focus_handle: FocusHandle,
    position: DockPosition,
    active_editor: Option<Entity<Editor>>,
    state: InfoViewState,
    _workspace_subscription: Subscription,
    _editor_subscription: Option<Subscription>,
    update_task: Task<()>,
}

impl LeanInfoView {
    pub fn load(
        workspace: WeakEntity<Workspace>,
        cx: AsyncWindowContext,
    ) -> Task<Result<Entity<Self>>> {
        cx.spawn(async move |cx| {
            workspace.update_in(cx, |workspace, window, cx| Self::new(workspace, window, cx))
        })
    }

    fn new(
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Entity<Self> {
        let workspace_handle = cx.entity();
        let initial_editor = active_full_editor(workspace, cx);
        cx.new(|cx| {
            let workspace_subscription = cx.subscribe_in(
                &workspace_handle,
                window,
                |this: &mut Self, workspace, event, window, cx| {
                    if let workspace::Event::ActiveItemChanged = event {
                        let active_editor = active_full_editor(workspace.read(cx), cx);
                        this.handle_active_editor_changed(active_editor, window, cx);
                    }
                },
            );

            let mut this = Self {
                focus_handle: cx.focus_handle(),
                position: DockPosition::Right,
                active_editor: None,
                state: InfoViewState::default(),
                _workspace_subscription: workspace_subscription,
                _editor_subscription: None,
                update_task: Task::ready(()),
            };
            this.handle_active_editor_changed(initial_editor, window, cx);
            this
        })
    }

    fn handle_active_editor_changed(
        &mut self,
        active_editor: Option<Entity<Editor>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(editor) = active_editor.filter(|editor| editor_has_lean_buffer(editor, cx)) else {
            if self.active_editor.take().is_some() {
                self._editor_subscription = None;
                self.state = InfoViewState::NoLeanBuffer;
                cx.notify();
            }
            return;
        };

        if self.active_editor.as_ref() == Some(&editor) {
            return;
        }

        self._editor_subscription = Some(cx.subscribe_in(
            &editor,
            window,
            |this: &mut Self, _, event: &EditorEvent, _, cx| match event {
                EditorEvent::SelectionsChanged { local: true } | EditorEvent::BufferEdited => {
                    this.schedule_update(cx);
                }
                _ => {}
            },
        ));
        self.active_editor = Some(editor);
        self.schedule_update(cx);
    }

    fn schedule_update(&mut self, cx: &mut Context<Self>) {
        let Some(editor) = self.active_editor.clone() else {
            return;
        };
        self.update_task = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(UPDATE_DEBOUNCE).await;
            Self::update_goals(this, editor, cx).await.log_err();
        });
    }

    async fn update_goals(
        this: WeakEntity<Self>,
        editor: Entity<Editor>,
        cx: &mut gpui::AsyncApp,
    ) -> Result<()> {
        let Some((server, buffer, position)) =
            this.update(cx, |_, cx| lean_server_and_position_for_editor(&editor, cx))?
        else {
            return Ok(());
        };

        this.update(cx, |this, cx| {
            if !matches!(this.state, InfoViewState::Loaded { .. }) {
                this.state = InfoViewState::Loading;
                cx.notify();
            }
        })?;

        let params = buffer.read_with(cx, |buffer, cx| {
            text_document_position_params(buffer, position, cx)
        })?;

        let plain_goal = server
            .request::<LeanPlainGoalRequest>(params.clone(), lsp::DEFAULT_LSP_REQUEST_TIMEOUT)
            .await
            .into_response()
            .context("lean plain goal request")?;
        let plain_term_goal = server
            .request::<LeanPlainTermGoalRequest>(params, lsp::DEFAULT_LSP_REQUEST_TIMEOUT)
            .await
            .into_response()
            .context("lean plain term goal request")?;

        this.update(cx, |this, cx| {
            this.state = InfoViewState::Loaded {
                goals: plain_goal.map(|goal| goal.goals).unwrap_or_default(),
                term_goal: plain_term_goal.map(|term_goal| term_goal.goal),
            };
            cx.notify();
        })?;
        Ok(())
    }

    fn render_goal_block(&self, text: &str, cx: &App) -> Div {
        let buffer_font = ThemeSettings::get_global(cx).buffer_font.family.clone();
        v_flex()
            .w_full()
            .p_2()
            .rounded_sm()
            .bg(cx.theme().colors().editor_background)
            .font_family(buffer_font)
            .text_size(TextSize::Small.rems(cx))
            .children(
                text.lines()
                    .map(|line| div().child(SharedString::from(line.to_owned()))),
            )
    }
}

fn active_full_editor(workspace: &Workspace, cx: &App) -> Option<Entity<Editor>> {
    workspace.active_item(cx).and_then(|item| {
        item.act_as::<Editor>(cx)
            .filter(|editor| editor.read(cx).mode().is_full())
    })
}

fn editor_has_lean_buffer(editor: &Entity<Editor>, cx: &App) -> bool {
    editor
        .read(cx)
        .buffer()
        .read(cx)
        .all_buffers()
        .into_iter()
        .filter_map(|buffer| buffer.read(cx).language().map(|language| language.name()))
        .any(|name| is_lean_language(name.as_ref()))
}

fn is_lean_language(language_name: &str) -> bool {
    language_name.eq_ignore_ascii_case("lean 4") || language_name.eq_ignore_ascii_case("lean")
}

fn lean_server_and_position_for_editor(
    editor: &Entity<Editor>,
    cx: &mut App,
) -> Option<(Arc<LanguageServer>, Entity<Buffer>, language::Anchor)> {
    let editor = editor.read(cx);
    let project = editor.project()?.clone();
    let cursor = editor.selections.newest_anchor().head();
    let (buffer, position) = editor
        .buffer()
        .read(cx)
        .text_anchor_for_position(cursor, cx)?;
    let language = buffer.read(cx).language()?;
    if !is_lean_language(language.name().as_ref()) {
        return None;
    }
    let server = find_lean_server(&project, &buffer, cx)?;
    Some((server, buffer, position))
}

fn find_lean_server(
    project: &Entity<Project>,
    buffer: &Entity<Buffer>,
    cx: &mut App,
) -> Option<Arc<LanguageServer>> {
    let lsp_store = project.read(cx).lsp_store();
    lsp_store.update(cx, |lsp_store, cx| {
        buffer.update(cx, |buffer, cx| {
            lsp_store
                .running_language_servers_for_local_buffer(buffer, cx)
                .map(|(_, server)| server.clone())
                .find(|server| server.name().0.to_lowercase().contains("lean"))
        })
    })
}

fn text_document_position_params(
    buffer: &Buffer,
    position: language::Anchor,
    cx: &App,
) -> Result<lsp::TextDocumentPositionParams> {
    let file = language::File::as_local(buffer.file().context("buffer has no file")?.as_ref())
        .context("buffer is not local")?;
    let uri = lsp::Uri::from_file_path(file.abs_path(cx))
        .ok()
        .context("failed to convert buffer path to LSP URI")?;
    let point = position.to_point_utf16(&buffer.snapshot());
    Ok(lsp::TextDocumentPositionParams {
        text_document: lsp::TextDocumentIdentifier { uri },
        position: point_to_lsp(point),
    })
}

impl Render for LeanInfoView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let content = match &self.state {
            InfoViewState::NoLeanBuffer => v_flex().p_2().child(
                Label::new("Open a Lean file to see the proof state.")
                    .color(Color::Muted)
                    .size(LabelSize::Small),
            ),
            InfoViewState::Loading => v_flex().p_2().child(
                Label::new("Loading goals…")
                    .color(Color::Muted)
                    .size(LabelSize::Small),
            ),
            InfoViewState::Loaded { goals, term_goal } => {
                let mut content = v_flex().p_2().gap_2();
                content = content.child(
                    Label::new("Tactic state")
                        .color(Color::Default)
                        .size(LabelSize::Small),
                );
                if goals.is_empty() {
                    content = content.child(
                        Label::new("No goals")
                            .color(Color::Muted)
                            .size(LabelSize::Small),
                    );
                } else {
                    content =
                        content.children(goals.iter().map(|goal| self.render_goal_block(goal, cx)));
                }
                if let Some(term_goal) = term_goal {
                    content = content
                        .child(Divider::horizontal())
                        .child(
                            Label::new("Expected type")
                                .color(Color::Default)
                                .size(LabelSize::Small),
                        )
                        .child(self.render_goal_block(term_goal, cx));
                }
                content
            }
        };

        v_flex()
            .id("lean-infoview")
            .key_context("LeanInfoView")
            .track_focus(&self.focus_handle)
            .size_full()
            .overflow_y_scroll()
            .bg(cx.theme().colors().panel_background)
            .child(content)
    }
}

impl Focusable for LeanInfoView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for LeanInfoView {}

impl Panel for LeanInfoView {
    fn persistent_name() -> &'static str {
        "LeanInfoView"
    }

    fn panel_key() -> &'static str {
        "LeanInfoView"
    }

    fn position(&self, _window: &Window, _cx: &App) -> DockPosition {
        self.position
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        matches!(position, DockPosition::Left | DockPosition::Right)
    }

    fn set_position(&mut self, position: DockPosition, _: &mut Window, cx: &mut Context<Self>) {
        self.position = position;
        cx.notify();
    }

    fn default_size(&self, _window: &Window, _cx: &App) -> Pixels {
        DEFAULT_WIDTH
    }

    fn icon(&self, _window: &Window, _cx: &App) -> Option<IconName> {
        Some(IconName::Info)
    }

    fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
        Some("Lean Infoview")
    }

    fn toggle_action(&self) -> Box<dyn gpui::Action> {
        Box::new(ToggleFocus)
    }

    fn activation_priority(&self) -> u32 {
        9
    }
}
