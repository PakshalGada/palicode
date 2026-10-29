//! TUI application state, keybindings, layout, and rendering.

use crate::agent::AgentEvent;
use crate::approval::{ApprovalRequest, ApprovalResponse};
use crate::config::{ApprovalMode, Config};
use crate::credentials;
use crate::event::{Event, SubagentState};
use crate::tools::ToolContext;
use crate::ui::agents::AgentPane;
use crate::ui::chat::{ChatState, ToolLogEntry};
use crate::ui::diff::DiffPane;
use crate::ui::plan::PlanPane;
use crate::ui::theme::{SyntaxEngine, Theme};
use crate::ui::util::{TextInput, popup_rect, slice_chars, spinner, truncate_chars};
use crate::ui::worker::{AgentCommand, UiMessage};
use crate::util::text::one_line;
use anyhow::Result;
use crossterm::event::{
    Event as CrosstermEvent, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
    MouseButton, MouseEvent, MouseEventKind,
};
use futures_util::StreamExt;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use ratatui::{Frame, Terminal};
use std::collections::HashMap;
use std::io::Stdout;
use std::time::{Duration, Instant};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::sync::oneshot;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Chat,
    Plan,
    Diff,
    Agents,
}

impl Focus {
    fn next(self) -> Self {
        match self {
            Focus::Chat => Focus::Plan,
            Focus::Plan => Focus::Diff,
            Focus::Diff => Focus::Agents,
            Focus::Agents => Focus::Chat,
        }
    }

    fn previous(self) -> Self {
        match self {
            Focus::Chat => Focus::Agents,
            Focus::Plan => Focus::Chat,
            Focus::Diff => Focus::Plan,
            Focus::Agents => Focus::Diff,
        }
    }
}

#[derive(Debug, Clone)]
enum Action {
    PlanTask,
    PickModel,
    Settings,
    Sessions,
    NewSession,
    Focus(Focus),
    ToolLog,
    ToggleDiffMode,
    ToggleTheme,
    Mode(ApprovalMode),
    Provider(String),
    ClearChat,
    Help,
    Quit,
}

impl Action {
    fn label(&self) -> String {
        match self {
            Action::PlanTask => "Plan a task…".to_string(),
            Action::PickModel => "Switch model…".to_string(),
            Action::Settings => "Settings: OpenCode API key…".to_string(),
            Action::Sessions => "Sessions: switch / new / delete…".to_string(),
            Action::NewSession => "New session".to_string(),
            Action::Focus(focus) => match focus {
                Focus::Chat => "Focus: chat".to_string(),
                Focus::Plan => "Focus: plan".to_string(),
                Focus::Diff => "Focus: diff".to_string(),
                Focus::Agents => "Focus: agents".to_string(),
            },
            Action::ToolLog => "Open tool log".to_string(),
            Action::ToggleDiffMode => "Diff: toggle unified/split".to_string(),
            Action::ToggleTheme => "Theme: toggle dark/light".to_string(),
            Action::Mode(mode) => format!("Approval mode: {mode}"),
            Action::Provider(name) => format!("Switch provider: {name}"),
            Action::ClearChat => "Clear conversation".to_string(),
            Action::Help => "Help".to_string(),
            Action::Quit => "Quit".to_string(),
        }
    }
}

struct PaletteState {
    input: TextInput,
    selected: usize,
}

struct ModelPicker {
    provider: String,
    models: Vec<String>,
    current: String,
    input: TextInput,
    selected: usize,
    loading: bool,
}

struct ToolsOverlay {
    selected: usize,
}

struct PlanEditState {
    index: usize,
    input: TextInput,
}

struct SettingsState {
    input: TextInput,
    status: Option<(String, Color)>,
}

struct SessionListState {
    selected: usize,
}

enum SessionAction {
    Switch(String),
    New,
    Delete(String),
}

struct SessionMeta {
    id: String,
    title: String,
    turns: usize,
    updated: Instant,
}

struct SessionUiState {
    chat: ChatState,
    tools: Vec<ToolLogEntry>,
    plan: PlanPane,
    diff: DiffPane,
    agents: AgentPane,
    tokens_in: u64,
    tokens_out: u64,
}

impl Default for SessionUiState {
    fn default() -> Self {
        Self {
            chat: ChatState::new(),
            tools: Vec::new(),
            plan: PlanPane::new(),
            diff: DiffPane::new(),
            agents: AgentPane::new(),
            tokens_in: 0,
            tokens_out: 0,
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct LayoutAreas {
    chat: Rect,
    plan: Rect,
    diff: Rect,
    agents: Rect,
    input: Rect,
    status: Rect,
}

struct ApprovalModal {
    request: ApprovalRequest,
    reply: oneshot::Sender<ApprovalResponse>,
    scroll: u16,
}

enum Overlay {
    Help,
    Palette(PaletteState),
    Models(ModelPicker),
    Tools(ToolsOverlay),
    PlanEdit(PlanEditState),
    Settings(SettingsState),
    Sessions(SessionListState),
}

struct Toast {
    text: String,
    color: Color,
    expires: Instant,
}

pub struct App {
    theme: Theme,
    syntax: SyntaxEngine,
    focus: Focus,
    chat_insert: bool,
    chat: ChatState,
    tools: Vec<ToolLogEntry>,
    plan: PlanPane,
    diff: DiffPane,
    agents: AgentPane,
    input: TextInput,
    overlay: Option<Overlay>,
    approval: Option<ApprovalModal>,
    toast: Option<Toast>,
    ctx: ToolContext,
    config: Config,
    config_path: std::path::PathBuf,
    agent_tx: UnboundedSender<AgentCommand>,
    ui_rx: UnboundedReceiver<UiMessage>,
    provider: String,
    model: String,
    mode: ApprovalMode,
    tokens_in: u64,
    tokens_out: u64,
    current_session: String,
    session_meta: Vec<SessionMeta>,
    session_states: HashMap<String, SessionUiState>,
    layout: LayoutAreas,
    turn_started: Option<Instant>,
    busy: bool,
    should_quit: bool,
    dirty: bool,
    spinner: usize,
    status_text: String,
}

impl App {
    pub fn new(
        config: Config,
        config_path: std::path::PathBuf,
        ctx: ToolContext,
        provider: String,
        model: String,
        agent_tx: UnboundedSender<AgentCommand>,
        ui_rx: UnboundedReceiver<UiMessage>,
    ) -> Self {
        let theme = Theme::from_name(&config.general.theme);
        let mode = config.general.approval_mode;
        let mut app = Self {
            theme,
            syntax: SyntaxEngine::new(),
            focus: Focus::Chat,
            chat_insert: true,
            chat: ChatState::new(),
            tools: Vec::new(),
            plan: PlanPane::new(),
            diff: DiffPane::new(),
            agents: AgentPane::new(),
            input: TextInput::new(),
            overlay: None,
            approval: None,
            toast: None,
            ctx,
            config,
            config_path,
            agent_tx,
            ui_rx,
            provider,
            model,
            mode,
            tokens_in: 0,
            tokens_out: 0,
            current_session: "default".to_string(),
            session_meta: vec![SessionMeta {
                id: "default".to_string(),
                title: "New session".to_string(),
                turns: 0,
                updated: Instant::now(),
            }],
            session_states: HashMap::new(),
            layout: LayoutAreas::default(),
            turn_started: None,
            busy: false,
            should_quit: false,
            dirty: true,
            spinner: 0,
            status_text: "ready".to_string(),
        };
        app.chat.push_info(format!(
            "palicode {} · {} / {} · mode {}",
            env!("CARGO_PKG_VERSION"),
            app.provider,
            app.model,
            app.mode
        ));
        app.chat
            .push_info("Esc: normal · i/Enter: type · Ctrl+K: palette · Tab: panes · /plan <task>");
        if !app.config_path.is_file() {
            app.chat.push_info(format!(
                "No config at {} — using defaults. Run `palicode config --init` to create one.",
                app.config_path.display()
            ));
        }
        app
    }

    pub async fn run(&mut self, terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
        let mut events = EventStream::new();
        let mut ticker = tokio::time::interval(Duration::from_millis(100));
        loop {
            if self.dirty {
                terminal.draw(|frame| self.draw(frame))?;
                self.dirty = false;
            }

            tokio::select! {
                message = self.ui_rx.recv() => {
                    match message {
                        Some(message) => {
                            self.on_ui_message(message);
                            while let Ok(message) = self.ui_rx.try_recv() {
                                self.on_ui_message(message);
                            }
                        }
                        None => {
                            self.chat.push_error("agent worker stopped");
                            self.should_quit = true;
                        }
                    }
                    self.dirty = true;
                }
                event = events.next() => {
                    if let Some(Ok(event)) = event {
                        self.on_terminal_event(event);
                        self.dirty = true;
                    }
                }
                _ = ticker.tick() => {
                    self.on_tick();
                }
            }

            if self.should_quit {
                break;
            }
        }
        Ok(())
    }

    fn on_tick(&mut self) {
        self.spinner = self.spinner.wrapping_add(1);
        if let Some(toast) = &self.toast
            && toast.expires <= Instant::now()
        {
            self.toast = None;
            self.dirty = true;
        }
        let agents_running = self
            .agents
            .entries
            .iter()
            .any(|entry| entry.state == SubagentState::Started);
        if self.busy || agents_running || self.toast.is_some() {
            self.dirty = true;
        }
    }

    fn on_terminal_event(&mut self, event: CrosstermEvent) {
        match event {
            CrosstermEvent::Key(key) if key.kind != KeyEventKind::Release => {
                self.handle_key(key);
            }
            CrosstermEvent::Paste(text) => {
                self.handle_paste(&text);
                self.dirty = true;
            }
            CrosstermEvent::Mouse(mouse) => {
                self.handle_mouse(mouse);
                self.dirty = true;
            }
            CrosstermEvent::Resize(_, _) => {
                self.dirty = true;
            }
            _ => {}
        }
    }

    fn handle_mouse(&mut self, mouse: MouseEvent) {
        if self.approval.is_some() {
            return;
        }
        match mouse.kind {
            MouseEventKind::ScrollUp => self.handle_mouse_scroll(mouse.column, mouse.row, true),
            MouseEventKind::ScrollDown => self.handle_mouse_scroll(mouse.column, mouse.row, false),
            MouseEventKind::Down(MouseButton::Left) => {
                self.handle_mouse_click(mouse.column, mouse.row)
            }
            _ => {}
        }
    }

    fn handle_mouse_scroll(&mut self, x: u16, y: u16, up: bool) {
        let delta = if up { -1isize } else { 1 };
        if self.overlay.is_some() {
            match &mut self.overlay {
                Some(Overlay::Tools(state)) => {
                    let len = self.tools.len();
                    if len > 0 {
                        state.selected =
                            (state.selected as isize + delta).clamp(0, len as isize - 1) as usize;
                    }
                }
                Some(Overlay::Sessions(state)) => {
                    let len = self.session_meta.len();
                    if len > 0 {
                        state.selected =
                            (state.selected as isize + delta).clamp(0, len as isize - 1) as usize;
                    }
                }
                Some(Overlay::Models(picker)) => {
                    let len = filtered_models(picker).len();
                    if len > 0 {
                        picker.selected =
                            (picker.selected as isize + delta).clamp(0, len as isize - 1) as usize;
                    }
                }
                _ => {}
            }
            return;
        }

        if rect_contains(self.layout.chat, x, y) {
            self.chat.scroll_by(delta * 3);
        } else if rect_contains(self.layout.plan, x, y) {
            self.plan.move_cursor(delta);
        } else if rect_contains(self.layout.diff, x, y) {
            self.diff.next_hunk(delta);
        } else if rect_contains(self.layout.agents, x, y) {
            self.agents.move_cursor(delta);
        }
    }

    fn handle_mouse_click(&mut self, x: u16, y: u16) {
        if self.overlay.is_some() {
            return;
        }
        if rect_contains(self.layout.input, x, y) {
            self.focus_pane(Focus::Chat, true);
        } else if rect_contains(self.layout.chat, x, y) {
            self.focus_pane(Focus::Chat, false);
        } else if rect_contains(self.layout.plan, x, y) {
            self.focus_pane(Focus::Plan, false);
            if y > self.layout.plan.y {
                self.plan.select_row(y - self.layout.plan.y - 1);
            }
        } else if rect_contains(self.layout.diff, x, y) {
            self.focus_pane(Focus::Diff, false);
            if y > self.layout.diff.y {
                self.diff.select_row(y - self.layout.diff.y - 1);
            }
        } else if rect_contains(self.layout.agents, x, y) {
            self.focus_pane(Focus::Agents, false);
        } else if rect_contains(self.layout.status, x, y) {
            // Status bar is informational.
        }
    }

    fn handle_paste(&mut self, text: &str) {
        if self.approval.is_some() {
            return;
        }
        let chat_paste: String = text
            .chars()
            .map(|ch| if ch == '\n' || ch == '\r' { ' ' } else { ch })
            .filter(|ch| !ch.is_control())
            .collect();
        let secret_paste: String = text
            .chars()
            .filter(|ch| !ch.is_control() && !ch.is_whitespace())
            .collect();

        match &mut self.overlay {
            Some(Overlay::Settings(state)) => {
                for ch in secret_paste.chars() {
                    state.input.insert(ch);
                }
            }
            Some(Overlay::Palette(state)) => {
                for ch in chat_paste.chars() {
                    state.input.insert(ch);
                }
                state.selected = 0;
            }
            Some(Overlay::Models(picker)) => {
                for ch in chat_paste.chars() {
                    picker.input.insert(ch);
                }
                picker.selected = 0;
            }
            Some(Overlay::PlanEdit(state)) => {
                for ch in chat_paste.chars() {
                    state.input.insert(ch);
                }
            }
            Some(Overlay::Help) | Some(Overlay::Tools(_)) | Some(Overlay::Sessions(_)) => {}
            None => {
                if self.focus == Focus::Chat && self.chat_insert {
                    for ch in chat_paste.chars() {
                        self.input.insert(ch);
                    }
                }
            }
        }
    }

    fn handle_key(&mut self, key: KeyEvent) {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.should_quit = true;
            return;
        }

        if self.approval.is_some() {
            self.handle_approval_key(key);
            return;
        }

        if key.code == KeyCode::F(1) {
            self.close_overlay();
            self.overlay = Some(Overlay::Help);
            return;
        }

        if key.code == KeyCode::F(2) {
            if matches!(self.overlay, Some(Overlay::Settings(_))) {
                self.close_overlay();
            } else {
                self.close_overlay();
                self.open_settings();
            }
            return;
        }

        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('k') {
            if matches!(self.overlay, Some(Overlay::Palette(_))) {
                self.overlay = None;
            } else {
                self.close_overlay();
                self.open_palette();
            }
            return;
        }

        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char(',') {
            if matches!(self.overlay, Some(Overlay::Settings(_))) {
                self.close_overlay();
            } else {
                self.close_overlay();
                self.open_settings();
            }
            return;
        }

        if self.overlay.is_some() {
            self.handle_overlay_key(key);
            return;
        }

        if key.code == KeyCode::Tab {
            let next = self.focus.next();
            self.focus_pane(next, false);
            return;
        }
        if key.code == KeyCode::BackTab {
            let previous = self.focus.previous();
            self.focus_pane(previous, false);
            return;
        }

        if self.focus == Focus::Chat && self.chat_insert {
            self.handle_chat_insert(key);
            return;
        }

        self.handle_normal_key(key);
    }

    fn handle_chat_insert(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.chat_insert = false;
            }
            KeyCode::Enter => self.submit_input(),
            KeyCode::Backspace => self.input.backspace(),
            KeyCode::Delete => self.input.delete(),
            KeyCode::Left => self.input.left(),
            KeyCode::Right => self.input.right(),
            KeyCode::Home => self.input.home(),
            KeyCode::End => self.input.end(),
            KeyCode::Up => self.chat.scroll_by(-3),
            KeyCode::Down => self.chat.scroll_by(3),
            KeyCode::PageUp => self.chat.scroll_by(-12),
            KeyCode::PageDown => self.chat.scroll_by(12),
            KeyCode::Char(ch)
                if !key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::ALT) =>
            {
                self.input.insert(ch);
            }
            _ => {}
        }
    }

    fn handle_normal_key(&mut self, key: KeyEvent) {
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            return;
        }
        match key.code {
            KeyCode::Char('?') => {
                self.overlay = Some(Overlay::Help);
                return;
            }
            KeyCode::Char('m') => {
                self.open_models();
                return;
            }
            KeyCode::Char('p') => {
                self.focus_pane(Focus::Plan, false);
                return;
            }
            KeyCode::Char('d') => {
                self.focus_pane(Focus::Diff, false);
                return;
            }
            KeyCode::Char('t') => {
                self.open_tools();
                return;
            }
            KeyCode::Char('c') => {
                self.focus_pane(Focus::Chat, false);
                return;
            }
            KeyCode::Char('s') => {
                self.open_sessions();
                return;
            }
            KeyCode::Char('n') if self.focus != Focus::Plan => {
                self.new_session();
                return;
            }
            KeyCode::Esc => {
                if self.focus == Focus::Chat {
                    self.input.clear();
                } else {
                    self.focus_pane(Focus::Chat, false);
                }
                return;
            }
            KeyCode::Char('q') => {
                if self.focus == Focus::Chat {
                    self.should_quit = true;
                } else {
                    self.focus_pane(Focus::Chat, false);
                }
                return;
            }
            _ => {}
        }

        match self.focus {
            Focus::Chat => self.handle_chat_normal(key),
            Focus::Plan => self.handle_plan_key(key),
            Focus::Diff => self.handle_diff_key(key),
            Focus::Agents => self.handle_agents_key(key),
        }
    }

    fn handle_chat_normal(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('i') | KeyCode::Enter => self.chat_insert = true,
            KeyCode::Char('/') => {
                self.chat_insert = true;
                self.input.insert('/');
            }
            KeyCode::Char('j') | KeyCode::Down => self.chat.scroll_by(3),
            KeyCode::Char('k') | KeyCode::Up => self.chat.scroll_by(-3),
            KeyCode::PageDown => self.chat.scroll_by(12),
            KeyCode::PageUp => self.chat.scroll_by(-12),
            KeyCode::Char('g') | KeyCode::Home => self.chat.scroll_to_top(),
            KeyCode::Char('G') | KeyCode::End => self.chat.scroll_to_bottom(),
            _ => {}
        }
    }

    fn handle_plan_key(&mut self, key: KeyEvent) {
        if self.plan.reorder {
            match key.code {
                KeyCode::Char('j') | KeyCode::Down => {
                    self.plan.move_step(1);
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    self.plan.move_step(-1);
                }
                KeyCode::Enter | KeyCode::Esc | KeyCode::Char('r') => {
                    self.plan.toggle_reorder();
                }
                _ => {}
            }
            return;
        }

        match key.code {
            KeyCode::Char('j') | KeyCode::Down => self.plan.move_cursor(1),
            KeyCode::Char('k') | KeyCode::Up => self.plan.move_cursor(-1),
            KeyCode::Char(' ') => self.plan.toggle_selected(),
            KeyCode::Char('a') => self.plan.select_all(),
            KeyCode::Char('n') => self.plan.select_none(),
            KeyCode::Char('r') => self.plan.toggle_reorder(),
            KeyCode::Char('d') => {
                if self.plan.delete_selected() {
                    self.notify("Removed plan step", self.theme.palette.dim);
                } else {
                    self.notify("Cannot delete the last step", self.theme.palette.warning);
                }
            }
            KeyCode::Char('e') => self.edit_plan_step(),
            KeyCode::Enter => self.execute_plan(),
            _ => {}
        }
    }

    fn handle_diff_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => self.diff.next_hunk(1),
            KeyCode::Char('k') | KeyCode::Up => self.diff.next_hunk(-1),
            KeyCode::Char('l') => self.diff.next_file(1),
            KeyCode::Char('h') => self.diff.next_file(-1),
            KeyCode::Left => self.diff.scroll_horizontal(-4),
            KeyCode::Right => self.diff.scroll_horizontal(4),
            KeyCode::Char('u') => self.diff.toggle_mode(),
            KeyCode::Char('a') => {
                if let Some(message) = self.diff.accept_hunk() {
                    self.notify(message, self.theme.palette.success);
                }
            }
            KeyCode::Char('A') => {
                if let Some(message) = self.diff.accept_file() {
                    self.notify(message, self.theme.palette.success);
                }
            }
            KeyCode::Char('r') => match self.diff.reject_hunk(&self.ctx) {
                Ok(message) => self.notify(message, self.theme.palette.warning),
                Err(err) => self.error(format!("{err:#}")),
            },
            KeyCode::Char('R') => match self.diff.reject_file(&self.ctx) {
                Ok(message) => self.notify(message, self.theme.palette.warning),
                Err(err) => self.error(format!("{err:#}")),
            },
            _ => {}
        }
    }

    fn handle_agents_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => self.agents.move_cursor(1),
            KeyCode::Char('k') | KeyCode::Up => self.agents.move_cursor(-1),
            _ => {}
        }
    }

    fn handle_approval_key(&mut self, key: KeyEvent) {
        let response = match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => Some(ApprovalResponse::Allow),
            KeyCode::Char('a') | KeyCode::Char('A') => Some(ApprovalResponse::AllowAlways),
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                Some(ApprovalResponse::Deny {
                    reason: "denied by user".to_string(),
                })
            }
            KeyCode::Up => {
                if let Some(approval) = &mut self.approval {
                    approval.scroll = approval.scroll.saturating_sub(2);
                }
                None
            }
            KeyCode::Down => {
                if let Some(approval) = &mut self.approval {
                    approval.scroll = approval.scroll.saturating_add(2);
                }
                None
            }
            _ => None,
        };

        if let Some(response) = response {
            let Some(modal) = self.approval.take() else {
                return;
            };
            let tool = modal.request.tool.clone();
            let _ = modal.reply.send(response.clone());
            let message = match response {
                ApprovalResponse::Allow => format!("Allowed {tool}"),
                ApprovalResponse::AllowAlways => format!("Allowed {tool} for this session"),
                ApprovalResponse::Deny { .. } => format!("Denied {tool}"),
            };
            let color = match response {
                ApprovalResponse::Deny { .. } => self.theme.palette.error,
                _ => self.theme.palette.success,
            };
            self.notify(message, color);
        }
    }

    fn handle_overlay_key(&mut self, key: KeyEvent) {
        if key.code == KeyCode::Esc {
            self.close_overlay();
            return;
        }

        let Some(mut overlay) = self.overlay.take() else {
            return;
        };
        let mut close = false;
        let mut action = None;
        let mut model_to_set = None;
        let mut session_action = None;

        match &mut overlay {
            Overlay::Help => {
                if matches!(key.code, KeyCode::Char('?') | KeyCode::Char('q')) {
                    close = true;
                }
            }
            Overlay::Palette(state) => {
                let query = state.input.value();
                let actions = self.filtered_actions(&query);
                match key.code {
                    KeyCode::Enter => {
                        action = actions.get(state.selected).cloned();
                        close = true;
                    }
                    KeyCode::Up => {
                        state.selected = state.selected.saturating_sub(1);
                    }
                    KeyCode::Down => {
                        if !actions.is_empty() {
                            state.selected = (state.selected + 1).min(actions.len() - 1);
                        }
                    }
                    KeyCode::Char(ch)
                        if !key.modifiers.contains(KeyModifiers::CONTROL)
                            && !key.modifiers.contains(KeyModifiers::ALT) =>
                    {
                        state.input.insert(ch);
                        state.selected = 0;
                    }
                    KeyCode::Backspace => {
                        state.input.backspace();
                        state.selected = 0;
                    }
                    KeyCode::Delete => state.input.delete(),
                    KeyCode::Left => state.input.left(),
                    KeyCode::Right => state.input.right(),
                    KeyCode::Home => state.input.home(),
                    KeyCode::End => state.input.end(),
                    _ => {}
                }
            }
            Overlay::Models(picker) => {
                let models = filtered_models(picker);
                match key.code {
                    KeyCode::Enter => {
                        model_to_set = models.get(picker.selected).cloned();
                        close = true;
                    }
                    KeyCode::Up => picker.selected = picker.selected.saturating_sub(1),
                    KeyCode::Down => {
                        if !models.is_empty() {
                            picker.selected = (picker.selected + 1).min(models.len() - 1);
                        }
                    }
                    KeyCode::Char(ch)
                        if !key.modifiers.contains(KeyModifiers::CONTROL)
                            && !key.modifiers.contains(KeyModifiers::ALT) =>
                    {
                        picker.input.insert(ch);
                        picker.selected = 0;
                    }
                    KeyCode::Backspace => {
                        picker.input.backspace();
                        picker.selected = 0;
                    }
                    KeyCode::Delete => picker.input.delete(),
                    KeyCode::Left => picker.input.left(),
                    KeyCode::Right => picker.input.right(),
                    KeyCode::Home => picker.input.home(),
                    KeyCode::End => picker.input.end(),
                    _ => {}
                }
            }
            Overlay::Tools(state) => {
                let len = self.tools.len();
                match key.code {
                    KeyCode::Char('j') | KeyCode::Down => {
                        if len > 0 {
                            state.selected = (state.selected + 1).min(len - 1);
                        }
                    }
                    KeyCode::Char('k') | KeyCode::Up => {
                        state.selected = state.selected.saturating_sub(1);
                    }
                    KeyCode::Char('g') => state.selected = 0,
                    KeyCode::Char('G') => state.selected = len.saturating_sub(1),
                    KeyCode::Enter => {
                        let selected = state.selected;
                        if let Some(tool) = self.tools.get_mut(selected) {
                            tool.expanded = !tool.expanded;
                        }
                    }
                    _ => {}
                }
            }
            Overlay::PlanEdit(state) => match key.code {
                KeyCode::Enter => {
                    let index = state.index;
                    let value = state.input.value();
                    if self.plan.edit_description(index, value) {
                        self.notify("Step updated", self.theme.palette.success);
                    } else {
                        self.notify("Description cannot be empty", self.theme.palette.warning);
                    }
                    close = true;
                }
                KeyCode::Char(ch)
                    if !key.modifiers.contains(KeyModifiers::CONTROL)
                        && !key.modifiers.contains(KeyModifiers::ALT) =>
                {
                    state.input.insert(ch);
                }
                KeyCode::Backspace => state.input.backspace(),
                KeyCode::Delete => state.input.delete(),
                KeyCode::Left => state.input.left(),
                KeyCode::Right => state.input.right(),
                KeyCode::Home => state.input.home(),
                KeyCode::End => state.input.end(),
                _ => {}
            },
            Overlay::Sessions(state) => {
                let len = self.session_meta.len();
                match key.code {
                    KeyCode::Char('j') | KeyCode::Down => {
                        if len > 0 {
                            state.selected = (state.selected + 1).min(len - 1);
                        }
                    }
                    KeyCode::Char('k') | KeyCode::Up => {
                        state.selected = state.selected.saturating_sub(1);
                    }
                    KeyCode::Enter => {
                        if let Some(meta) = self.session_meta.get(state.selected) {
                            session_action = Some(SessionAction::Switch(meta.id.clone()));
                            close = true;
                        }
                    }
                    KeyCode::Char('n') => {
                        session_action = Some(SessionAction::New);
                        close = true;
                    }
                    KeyCode::Char('d') => {
                        if let Some(meta) = self.session_meta.get(state.selected) {
                            session_action = Some(SessionAction::Delete(meta.id.clone()));
                        }
                    }
                    _ => {}
                }
            }
            Overlay::Settings(state) => match key.code {
                KeyCode::Enter => {
                    let key_value = state.input.value().trim().to_string();
                    if key_value.is_empty() {
                        state.status = Some((
                            "Paste or type an API key first".to_string(),
                            self.theme.palette.warning,
                        ));
                    } else {
                        match credentials::set_key(credentials::OPENCODE_PROVIDER, &key_value) {
                            Ok(path) => {
                                state.input.wipe();
                                close = true;
                                self.chat.push_info(format!(
                                    "OpenCode API key saved to {} (mode 0600). Use palette → Switch provider: opencode.",
                                    path.display()
                                ));
                                self.notify(
                                    "OpenCode API key saved securely",
                                    self.theme.palette.success,
                                );
                            }
                            Err(err) => {
                                state.status = Some((format!("{err:#}"), self.theme.palette.error));
                            }
                        }
                    }
                }
                KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    match credentials::remove_key(credentials::OPENCODE_PROVIDER) {
                        Ok(_) => {
                            state.input.wipe();
                            state.status = Some((
                                "Stored OpenCode key removed".to_string(),
                                self.theme.palette.warning,
                            ));
                        }
                        Err(err) => {
                            state.status = Some((format!("{err:#}"), self.theme.palette.error));
                        }
                    }
                }
                KeyCode::Char(ch)
                    if !key.modifiers.contains(KeyModifiers::CONTROL)
                        && !key.modifiers.contains(KeyModifiers::ALT) =>
                {
                    state.input.insert(ch);
                }
                KeyCode::Backspace => state.input.backspace(),
                KeyCode::Delete => state.input.delete(),
                KeyCode::Left => state.input.left(),
                KeyCode::Right => state.input.right(),
                KeyCode::Home => state.input.home(),
                KeyCode::End => state.input.end(),
                _ => {}
            },
        }

        if !close {
            self.overlay = Some(overlay);
        }
        if let Some(action) = action {
            self.perform_action(action);
        }
        if let Some(model) = model_to_set {
            self.send(AgentCommand::SetModel(model.clone()));
            self.notify(
                format!("Switching model to {model}…"),
                self.theme.palette.accent,
            );
        }
        if let Some(action) = session_action {
            match action {
                SessionAction::Switch(id) => self.switch_session(id),
                SessionAction::New => self.new_session(),
                SessionAction::Delete(id) => self.delete_session(id),
            }
        }
    }

    fn submit_input(&mut self) {
        let value = self.input.take();
        let value = value.trim().to_string();
        if value.is_empty() {
            return;
        }
        if self.busy {
            self.input.set(value);
            self.notify(
                "Wait for the current turn to finish",
                self.theme.palette.warning,
            );
            return;
        }

        if value == "/help" || value == "/?" {
            self.overlay = Some(Overlay::Help);
            return;
        }
        if value == "/clear" {
            self.clear_chat();
            return;
        }
        if value == "/quit" || value == "/q" {
            self.should_quit = true;
            return;
        }
        if value == "/model" {
            self.open_models();
            return;
        }
        if value == "/settings" {
            self.open_settings();
            return;
        }
        if value == "/mode" {
            self.notify(
                format!("Approval mode: {}", self.mode),
                self.theme.palette.dim,
            );
            return;
        }
        if let Some(mode) = value.strip_prefix("/mode ") {
            self.set_mode_text(mode);
            return;
        }
        if value == "/plan" {
            self.notify("Usage: /plan <task>", self.theme.palette.warning);
            return;
        }
        if let Some(task) = value.strip_prefix("/plan ") {
            self.start_plan(task.trim());
            return;
        }
        if value.starts_with('/') {
            self.notify(
                format!("Unknown command '{value}' (try /help)"),
                self.theme.palette.warning,
            );
            return;
        }

        self.record_prompt(&value);
        self.chat.push_user(&value);
        self.chat_insert = false;
        self.busy = true;
        self.turn_started = Some(Instant::now());
        self.status_text = "thinking".to_string();
        self.send(AgentCommand::Submit(value));
    }

    fn start_plan(&mut self, task: &str) {
        if task.is_empty() {
            self.notify("Usage: /plan <task>", self.theme.palette.warning);
            return;
        }
        if self.busy {
            self.notify(
                "Wait for the current turn to finish",
                self.theme.palette.warning,
            );
            return;
        }
        self.record_prompt(task);
        self.plan.clear_for_new();
        self.focus_pane(Focus::Plan, false);
        self.busy = true;
        self.turn_started = Some(Instant::now());
        self.chat.push_user(format!("/plan {task}"));
        self.chat.push_info("Planning…");
        self.status_text = "planning".to_string();
        self.send(AgentCommand::GeneratePlan(task.to_string()));
    }

    fn execute_plan(&mut self) {
        if self.busy {
            self.notify(
                "Wait for the current turn to finish",
                self.theme.palette.warning,
            );
            return;
        }
        let Some(plan) = self.plan.plan.clone() else {
            self.notify("No plan to execute", self.theme.palette.warning);
            return;
        };
        let selected = self.plan.selected_indices();
        if selected.is_empty() {
            self.notify("Select at least one step", self.theme.palette.warning);
            return;
        }
        self.plan.running = true;
        self.plan.review = false;
        self.busy = true;
        self.turn_started = Some(Instant::now());
        self.focus_pane(Focus::Chat, false);
        self.chat
            .push_info(format!("Executing {} plan step(s)…", selected.len()));
        self.status_text = "executing plan".to_string();
        self.send(AgentCommand::ExecutePlan { plan, selected });
    }

    fn edit_plan_step(&mut self) {
        let Some(plan) = &self.plan.plan else {
            return;
        };
        let Some(step) = plan.steps.get(self.plan.cursor) else {
            return;
        };
        self.overlay = Some(Overlay::PlanEdit(PlanEditState {
            index: self.plan.cursor,
            input: TextInput::with_value(step.description.clone()),
        }));
    }

    fn set_mode_text(&mut self, value: &str) {
        match ApprovalMode::parse(value) {
            Some(mode) => {
                self.send(AgentCommand::SetMode(mode));
                self.notify(format!("Approval mode: {mode}"), self.theme.palette.accent);
            }
            None => self.notify(
                format!("Invalid mode '{value}' (suggest | auto-edit | full-auto)"),
                self.theme.palette.warning,
            ),
        }
    }

    fn open_models(&mut self) {
        if self.busy {
            self.notify(
                "Wait for the current turn to finish",
                self.theme.palette.warning,
            );
            return;
        }
        self.overlay = Some(Overlay::Models(ModelPicker {
            provider: self.provider.clone(),
            models: Vec::new(),
            current: self.model.clone(),
            input: TextInput::new(),
            selected: 0,
            loading: true,
        }));
        self.send(AgentCommand::ListModels);
    }

    fn open_palette(&mut self) {
        self.overlay = Some(Overlay::Palette(PaletteState {
            input: TextInput::new(),
            selected: 0,
        }));
    }

    fn open_tools(&mut self) {
        self.overlay = Some(Overlay::Tools(ToolsOverlay {
            selected: self.tools.len().saturating_sub(1),
        }));
    }

    fn open_settings(&mut self) {
        let stored = credentials::has_key(credentials::OPENCODE_PROVIDER);
        let env_set = std::env::var("OPENCODE_API_KEY")
            .map(|value| !value.trim().is_empty())
            .unwrap_or(false);
        let status = if env_set {
            Some((
                "OPENCODE_API_KEY is set in the environment and takes precedence".to_string(),
                self.theme.palette.warning,
            ))
        } else if stored {
            Some((
                "A key is already stored (hidden)".to_string(),
                self.theme.palette.success,
            ))
        } else {
            Some((
                "No OpenCode key stored yet".to_string(),
                self.theme.palette.dim,
            ))
        };
        self.overlay = Some(Overlay::Settings(SettingsState {
            input: TextInput::new(),
            status,
        }));
    }

    fn close_overlay(&mut self) {
        if let Some(Overlay::Settings(state)) = &mut self.overlay {
            state.input.wipe();
        }
        self.overlay = None;
    }

    fn clear_chat(&mut self) {
        if self.busy {
            self.notify(
                "Wait for the current turn to finish",
                self.theme.palette.warning,
            );
            return;
        }
        self.chat.clear();
        self.tools.clear();
        self.plan = PlanPane::new();
        self.diff = DiffPane::new();
        self.agents = AgentPane::new();
        self.tokens_in = 0;
        self.tokens_out = 0;
        if let Some(meta) = self
            .session_meta
            .iter_mut()
            .find(|meta| meta.id == self.current_session)
        {
            meta.title = "New session".to_string();
            meta.turns = 0;
            meta.updated = Instant::now();
        }
        self.send(AgentCommand::ClearHistory);
        self.notify("Conversation cleared", self.theme.palette.dim);
        self.status_text = "ready".to_string();
    }

    fn current_session_title(&self) -> String {
        self.session_meta
            .iter()
            .find(|meta| meta.id == self.current_session)
            .map(|meta| meta.title.clone())
            .unwrap_or_else(|| "Session".to_string())
    }

    fn record_prompt(&mut self, prompt: &str) {
        if let Some(meta) = self
            .session_meta
            .iter_mut()
            .find(|meta| meta.id == self.current_session)
        {
            let first = meta.turns == 0;
            meta.turns += 1;
            if first || meta.title == "New session" {
                meta.title = truncate_chars(&one_line(prompt, 48), 48);
            }
            meta.updated = Instant::now();
        }
    }

    fn open_sessions(&mut self) {
        if self.busy {
            self.notify(
                "Wait for the current turn to finish",
                self.theme.palette.warning,
            );
            return;
        }
        let selected = self
            .session_meta
            .iter()
            .position(|meta| meta.id == self.current_session)
            .unwrap_or(0);
        self.overlay = Some(Overlay::Sessions(SessionListState { selected }));
    }

    fn stash_current_state(&mut self) {
        let state = SessionUiState {
            chat: std::mem::replace(&mut self.chat, ChatState::new()),
            tools: std::mem::take(&mut self.tools),
            plan: std::mem::replace(&mut self.plan, PlanPane::new()),
            diff: std::mem::replace(&mut self.diff, DiffPane::new()),
            agents: std::mem::replace(&mut self.agents, AgentPane::new()),
            tokens_in: self.tokens_in,
            tokens_out: self.tokens_out,
        };
        self.session_states
            .insert(self.current_session.clone(), state);
    }

    fn install_ui_state(&mut self, state: SessionUiState) {
        self.chat = state.chat;
        self.tools = state.tools;
        self.plan = state.plan;
        self.diff = state.diff;
        self.agents = state.agents;
        self.tokens_in = state.tokens_in;
        self.tokens_out = state.tokens_out;
    }

    fn new_session(&mut self) {
        if self.busy {
            self.notify(
                "Wait for the current turn to finish",
                self.theme.palette.warning,
            );
            return;
        }
        self.stash_current_state();
        let id = uuid::Uuid::new_v4().to_string();
        self.current_session = id.clone();
        self.session_meta.push(SessionMeta {
            id: id.clone(),
            title: "New session".to_string(),
            turns: 0,
            updated: Instant::now(),
        });
        self.install_ui_state(SessionUiState::default());
        self.input.clear();
        self.send(AgentCommand::NewSession(id));
        self.overlay = None;
        self.focus_pane(Focus::Chat, true);
        self.turn_started = None;
        self.status_text = "ready".to_string();
        self.chat.push_info("New session");
        self.notify("New session created", self.theme.palette.dim);
    }

    fn switch_session(&mut self, id: String) {
        if self.busy {
            self.notify(
                "Wait for the current turn to finish",
                self.theme.palette.warning,
            );
            return;
        }
        if id == self.current_session {
            self.overlay = None;
            return;
        }
        let title = self
            .session_meta
            .iter()
            .find(|meta| meta.id == id)
            .map(|meta| meta.title.clone())
            .unwrap_or_else(|| "Session".to_string());
        self.stash_current_state();
        self.current_session = id.clone();
        let state = self.session_states.remove(&id).unwrap_or_default();
        self.install_ui_state(state);
        self.input.clear();
        self.send(AgentCommand::SwitchSession(id));
        if let Some(meta) = self
            .session_meta
            .iter_mut()
            .find(|meta| meta.id == self.current_session)
        {
            meta.updated = Instant::now();
        }
        self.overlay = None;
        self.focus_pane(Focus::Chat, false);
        self.turn_started = None;
        self.status_text = "ready".to_string();
        self.chat.push_info(format!("Switched to session: {title}"));
        self.notify("Session switched", self.theme.palette.success);
    }

    fn delete_session(&mut self, id: String) {
        if self.busy {
            self.notify(
                "Wait for the current turn to finish",
                self.theme.palette.warning,
            );
            return;
        }
        if id == self.current_session {
            self.notify(
                "Switch to another session before deleting this one",
                self.theme.palette.warning,
            );
            return;
        }
        self.session_meta.retain(|meta| meta.id != id);
        self.session_states.remove(&id);
        self.send(AgentCommand::DeleteSession(id));
        if let Some(Overlay::Sessions(state)) = &mut self.overlay {
            state.selected = state
                .selected
                .min(self.session_meta.len().saturating_sub(1));
        }
        self.notify("Session deleted", self.theme.palette.dim);
    }

    fn focus_pane(&mut self, focus: Focus, insert: bool) {
        self.focus = focus;
        if focus == Focus::Chat {
            self.chat_insert = insert;
        } else {
            self.chat_insert = false;
        }
    }

    fn perform_action(&mut self, action: Action) {
        match action {
            Action::PlanTask => {
                self.focus_pane(Focus::Chat, true);
                self.input.set("/plan ");
            }
            Action::PickModel => self.open_models(),
            Action::Settings => self.open_settings(),
            Action::Sessions => self.open_sessions(),
            Action::NewSession => self.new_session(),
            Action::Focus(focus) => self.focus_pane(focus, focus == Focus::Chat),
            Action::ToolLog => self.open_tools(),
            Action::ToggleDiffMode => {
                self.diff.toggle_mode();
                self.notify("Diff mode toggled", self.theme.palette.dim);
            }
            Action::ToggleTheme => {
                self.theme.toggle();
                self.notify(
                    format!(
                        "Theme: {}",
                        if self.theme.mode == crate::ui::theme::ThemeMode::Dark {
                            "dark"
                        } else {
                            "light"
                        }
                    ),
                    self.theme.palette.dim,
                );
            }
            Action::Mode(mode) => {
                self.send(AgentCommand::SetMode(mode));
                self.notify(format!("Approval mode: {mode}"), self.theme.palette.accent);
            }
            Action::Provider(name) => {
                if self.busy {
                    self.notify(
                        "Wait for the current turn to finish",
                        self.theme.palette.warning,
                    );
                } else {
                    self.send(AgentCommand::SetProvider(name.clone()));
                    self.notify(
                        format!("Switching provider to {name}…"),
                        self.theme.palette.accent,
                    );
                }
            }
            Action::ClearChat => self.clear_chat(),
            Action::Help => self.overlay = Some(Overlay::Help),
            Action::Quit => self.should_quit = true,
        }
    }

    fn actions(&self) -> Vec<Action> {
        let mut actions = vec![
            Action::PlanTask,
            Action::PickModel,
            Action::Settings,
            Action::Sessions,
            Action::NewSession,
            Action::Focus(Focus::Chat),
            Action::Focus(Focus::Plan),
            Action::Focus(Focus::Diff),
            Action::Focus(Focus::Agents),
            Action::ToolLog,
            Action::ToggleDiffMode,
            Action::ToggleTheme,
            Action::Mode(ApprovalMode::Suggest),
            Action::Mode(ApprovalMode::AutoEdit),
            Action::Mode(ApprovalMode::FullAuto),
            Action::ClearChat,
            Action::Help,
            Action::Quit,
        ];
        for provider in self.config.enabled_providers() {
            if provider.name != self.provider {
                actions.push(Action::Provider(provider.name.clone()));
            }
        }
        actions
    }

    fn filtered_actions(&self, query: &str) -> Vec<Action> {
        let query = query.trim().to_ascii_lowercase();
        let mut actions: Vec<(i32, Action)> = self
            .actions()
            .into_iter()
            .filter_map(|action| {
                let label = action.label().to_ascii_lowercase();
                fuzzy_score(&label, &query).map(|score| (score, action))
            })
            .collect();
        actions.sort_by_key(|left| std::cmp::Reverse(left.0));
        actions.into_iter().map(|(_, action)| action).collect()
    }

    fn send(&self, command: AgentCommand) {
        let _ = self.agent_tx.send(command);
    }

    fn notify(&mut self, text: impl Into<String>, color: Color) {
        self.toast = Some(Toast {
            text: text.into(),
            color,
            expires: Instant::now() + Duration::from_secs(4),
        });
        self.dirty = true;
    }

    fn error(&mut self, text: impl Into<String>) {
        let text = text.into();
        self.chat.push_error(&text);
        self.status_text = "error".to_string();
        self.notify(text, self.theme.palette.error);
    }

    fn on_ui_message(&mut self, message: UiMessage) {
        match message {
            UiMessage::Agent(event) => self.on_agent_event(event),
            UiMessage::Bus(event) => self.on_bus_event(event),
            UiMessage::Approval { request, reply } => {
                self.close_overlay();
                self.approval = Some(ApprovalModal {
                    request,
                    reply,
                    scroll: 0,
                });
                self.status_text = "approval required".to_string();
            }
            UiMessage::Models {
                provider,
                current,
                models,
            } => {
                if let Some(Overlay::Models(picker)) = &mut self.overlay {
                    picker.provider = provider;
                    picker.current = current;
                    picker.models = models;
                    picker.loading = false;
                    picker.selected = 0;
                }
            }
            UiMessage::ModelChanged { provider, model } => {
                self.provider = provider;
                self.model = model;
                self.overlay = None;
                self.chat
                    .push_info(format!("Model: {} / {}", self.provider, self.model));
                self.notify("Model switched", self.theme.palette.success);
            }
            UiMessage::ProviderChanged { provider, model } => {
                self.provider = provider;
                self.model = model;
                self.overlay = None;
                self.chat
                    .push_info(format!("Provider: {} / {}", self.provider, self.model));
                self.notify("Provider switched", self.theme.palette.success);
            }
            UiMessage::ModeChanged(mode) => {
                self.mode = mode;
                self.chat.push_info(format!("Approval mode: {mode}"));
                self.notify(format!("Approval mode: {mode}"), self.theme.palette.accent);
            }
            UiMessage::Status(text) => self.status_text = text,
            UiMessage::Error(text) => {
                if let Some(Overlay::Models(picker)) = &mut self.overlay {
                    picker.loading = false;
                }
                self.error(text);
            }
            UiMessage::TurnFinished => {
                self.busy = false;
                self.turn_started = None;
                self.chat.end_streaming();
                if self.plan.pending {
                    self.plan.pending = false;
                }
                if self.status_text != "ready" {
                    self.status_text = "ready".to_string();
                }
            }
            UiMessage::PlanFinished(outcome) => {
                self.plan.finish();
                let summary = format!(
                    "Plan: {} done, {} failed, {} skipped",
                    outcome.done, outcome.failed, outcome.skipped
                );
                self.notify(
                    summary,
                    if outcome.failed > 0 {
                        self.theme.palette.error
                    } else {
                        self.theme.palette.success
                    },
                );
                if outcome.failed > 0 {
                    self.focus_pane(Focus::Plan, false);
                }
            }
        }
        self.dirty = true;
    }

    fn on_agent_event(&mut self, event: AgentEvent) {
        match event {
            AgentEvent::TextDelta(delta) => {
                self.chat.append_delta(&delta);
                self.status_text = "streaming".to_string();
            }
            AgentEvent::ReasoningDelta(_) => {
                self.status_text = "thinking".to_string();
            }
            AgentEvent::AssistantMessage => {
                self.chat.end_streaming();
            }
            AgentEvent::ToolCall {
                id,
                name,
                arguments,
            } => {
                self.tools.push(ToolLogEntry {
                    id,
                    name: name.clone(),
                    arguments,
                    output: String::new(),
                    is_error: false,
                    duration_ms: None,
                    expanded: false,
                });
                let index = self.tools.len() - 1;
                self.chat.push_tool(index);
                self.status_text = format!("tool: {name}");
            }
            AgentEvent::ToolResult {
                id,
                name,
                output,
                is_error,
                duration_ms,
            } => {
                let index = self
                    .tools
                    .iter()
                    .rposition(|tool| tool.id == id && tool.output.is_empty())
                    .or_else(|| self.tools.iter().rposition(|tool| tool.id == id));
                match index {
                    Some(index) => {
                        let tool = &mut self.tools[index];
                        tool.output = output;
                        tool.is_error = is_error;
                        tool.duration_ms = Some(duration_ms);
                    }
                    None => {
                        self.tools.push(ToolLogEntry {
                            id,
                            name: name.clone(),
                            arguments: String::new(),
                            output,
                            is_error,
                            duration_ms: Some(duration_ms),
                            expanded: false,
                        });
                        let index = self.tools.len() - 1;
                        self.chat.push_tool(index);
                    }
                }
                self.status_text = format!("tool: {name}");
            }
            AgentEvent::PermissionDenied { tool, reason } => {
                self.status_text = format!("denied: {tool}");
                self.notify(format!("{tool}: {reason}"), self.theme.palette.error);
            }
            AgentEvent::Status(text) => {
                self.chat.push_info(text.clone());
                self.status_text = text;
            }
            AgentEvent::Diff { tool, diff } => match self.diff.ingest(&tool, &diff) {
                Ok(count) => {
                    self.status_text = format!("diff: {count} file(s)");
                }
                Err(err) => {
                    self.chat
                        .push_error(format!("could not parse diff: {err:#}"));
                }
            },
            AgentEvent::Usage(usage) => {
                self.tokens_in = self.tokens_in.saturating_add(usage.prompt_tokens);
                self.tokens_out = self.tokens_out.saturating_add(usage.completion_tokens);
            }
            AgentEvent::StepLimit { steps } => {
                self.chat
                    .push_error(format!("step limit reached ({steps} steps)"));
            }
        }
        self.dirty = true;
    }

    fn on_bus_event(&mut self, event: Event) {
        match event {
            Event::Plan(plan) => {
                let is_new = self
                    .plan
                    .plan
                    .as_ref()
                    .map(|current| current.title != plan.title || current.goal != plan.goal)
                    .unwrap_or(true);
                let title = plan.title.clone();
                self.plan.set_plan(plan);
                if is_new {
                    self.focus_pane(Focus::Plan, false);
                    self.status_text = "plan ready".to_string();
                    self.chat.push_info(format!("Plan ready: {title}"));
                    self.notify(
                        "Plan ready — review, then Enter to execute",
                        self.theme.palette.warning,
                    );
                }
            }
            Event::PlanStep {
                index,
                status,
                description,
            } => {
                self.plan.update_step(index, status);
                if status == crate::agent::plan::StepStatus::Running {
                    self.status_text = format!("plan step: {description}");
                }
            }
            Event::PlanComplete {
                done,
                failed,
                skipped,
            } => {
                self.plan.finish();
                let summary =
                    format!("Plan finished: {done} done, {failed} failed, {skipped} skipped");
                self.chat.push_info(summary.clone());
                self.notify(
                    summary,
                    if failed > 0 {
                        self.theme.palette.error
                    } else {
                        self.theme.palette.success
                    },
                );
                if failed > 0 {
                    self.focus_pane(Focus::Plan, false);
                }
            }
            Event::Subagent {
                description,
                state,
                message,
            } => {
                self.agents.update(description, state, message);
            }
            Event::Status(text) => {
                self.status_text = text;
            }
            Event::Error(text) => {
                self.chat.push_error(text.clone());
                self.status_text = "error".to_string();
                self.notify(text, self.theme.palette.error);
            }
            Event::Agent(_) => {}
        }
        self.dirty = true;
    }

    pub fn draw(&mut self, frame: &mut Frame) {
        let area = frame.area();
        frame.render_widget(
            Block::default().style(Style::default().bg(self.theme.palette.bg)),
            area,
        );

        if area.width < 70 || area.height < 20 {
            let message = Paragraph::new("Terminal too small (minimum 70×20)")
                .style(Style::default().fg(self.theme.palette.warning))
                .alignment(ratatui::layout::Alignment::Center);
            let popup = popup_rect(area, 80, 20);
            frame.render_widget(Clear, popup);
            frame.render_widget(message, popup);
            return;
        }

        let outer = Layout::vertical([Constraint::Min(10), Constraint::Length(1)]).split(area);
        let main = Layout::horizontal([
            Constraint::Percentage(62),
            Constraint::Length(1),
            Constraint::Percentage(37),
        ])
        .split(outer[0]);
        let chat_areas =
            Layout::vertical([Constraint::Min(3), Constraint::Length(3)]).split(main[0]);
        let right = Layout::vertical([
            Constraint::Percentage(42),
            Constraint::Percentage(43),
            Constraint::Percentage(15),
        ])
        .split(main[2]);

        let separator: Vec<Line> = (0..main[1].height)
            .map(|_| {
                Line::from(Span::styled(
                    "│",
                    Style::default().fg(self.theme.palette.border),
                ))
            })
            .collect();
        frame.render_widget(Paragraph::new(Text::from(separator)), main[1]);

        self.layout = LayoutAreas {
            chat: chat_areas[0],
            plan: right[0],
            diff: right[1],
            agents: right[2],
            input: chat_areas[1],
            status: outer[1],
        };
        self.render_chat(frame, chat_areas[0], chat_areas[1]);
        self.render_plan(frame, right[0]);
        self.render_diff(frame, right[1]);
        self.render_agents(frame, right[2]);
        self.render_status(frame, outer[1]);

        if let Some(overlay) = &self.overlay {
            match overlay {
                Overlay::Help => self.render_help(frame),
                Overlay::Palette(state) => self.render_palette(frame, state),
                Overlay::Models(picker) => self.render_models(frame, picker),
                Overlay::Tools(state) => self.render_tools(frame, state),
                Overlay::PlanEdit(state) => self.render_plan_edit(frame, state),
                Overlay::Settings(state) => self.render_settings(frame, state),
                Overlay::Sessions(state) => self.render_sessions(frame, state),
            }
        }

        if self.approval.is_some() {
            self.render_approval(frame);
        }
    }

    fn render_chat(&mut self, frame: &mut Frame, area: Rect, input_area: Rect) {
        let focused = self.focus == Focus::Chat;
        let title = if focused && self.chat_insert {
            "Chat · INSERT"
        } else if focused {
            "Chat · NORMAL"
        } else {
            "Chat"
        };
        let inner = pane(frame, area, title, focused, &self.theme);
        self.chat.viewport_height = inner.height;
        let width = inner.width.max(1);
        let lines = self.chat.build_lines(width, &self.tools, &self.theme);
        let paragraph = Paragraph::new(Text::from(lines))
            .style(self.theme.text())
            .scroll((self.chat.scroll, 0));
        frame.render_widget(paragraph, inner);

        let input_focused = focused && self.chat_insert;
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(if input_focused {
                self.theme.palette.border_focus
            } else {
                self.theme.palette.border
            }))
            .style(Style::default().bg(self.theme.palette.input_bg))
            .title(Span::styled(
                " Message ",
                Style::default().fg(if input_focused {
                    self.theme.palette.accent
                } else {
                    self.theme.palette.dim
                }),
            ));
        let input_inner = block.inner(input_area);
        frame.render_widget(block, input_area);

        let (text, cursor_col) = self
            .input
            .window(input_inner.width.saturating_sub(1) as usize);
        if text.is_empty() {
            let placeholder = if self.chat_insert {
                "Ask anything, or /plan <task>…"
            } else {
                "i or Enter to type · Ctrl+K for commands"
            };
            frame.render_widget(
                Paragraph::new(truncate_chars(placeholder, input_inner.width as usize))
                    .style(self.theme.dim()),
                input_inner,
            );
        } else {
            frame.render_widget(Paragraph::new(text).style(self.theme.text()), input_inner);
        }
        if input_focused && input_inner.width > 0 {
            let x = (input_inner.x + cursor_col as u16).min(input_inner.right().saturating_sub(1));
            frame.set_cursor_position(Position::new(x, input_inner.y));
        }
    }

    fn render_plan(&mut self, frame: &mut Frame, area: Rect) {
        let focused = self.focus == Focus::Plan;
        let inner = pane(frame, area, "Plan", focused, &self.theme);
        let sections = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(inner);
        let lines = self.plan.build_lines(sections[0].width.max(1), &self.theme);
        self.plan.viewport_height = sections[0].height;
        self.plan.ensure_cursor_visible(sections[0].height);
        frame.render_widget(
            Paragraph::new(Text::from(lines))
                .style(self.theme.text())
                .scroll((self.plan.scroll, 0)),
            sections[0],
        );
        frame.render_widget(
            Paragraph::new(self.plan.hint(&self.theme)).style(self.theme.dim()),
            sections[1],
        );
    }

    fn render_diff(&mut self, frame: &mut Frame, area: Rect) {
        let focused = self.focus == Focus::Diff;
        let title = match self.diff.mode {
            crate::ui::diff::DiffMode::Unified => "Diff · unified",
            crate::ui::diff::DiffMode::Split => "Diff · split",
        };
        let inner = pane(frame, area, title, focused, &self.theme);
        let sections = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(inner);
        let lines = self
            .diff
            .build_lines(sections[0].width.max(1), &self.theme, &self.syntax);
        self.diff.viewport_height = sections[0].height;
        self.diff.ensure_hunk_visible(sections[0].height);
        frame.render_widget(
            Paragraph::new(Text::from(lines))
                .style(self.theme.text())
                .scroll((self.diff.scroll, 0)),
            sections[0],
        );
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                self.diff.hint(sections[1].width as usize),
                self.theme.dim(),
            ))),
            sections[1],
        );
    }

    fn render_agents(&mut self, frame: &mut Frame, area: Rect) {
        let focused = self.focus == Focus::Agents;
        let inner = pane(frame, area, "Agents", focused, &self.theme);
        let sections = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(inner);
        let lines = self
            .agents
            .build_lines(sections[0].width.max(1), &self.theme);
        self.agents.viewport_height = sections[0].height;
        self.agents.ensure_cursor_visible(sections[0].height);
        frame.render_widget(
            Paragraph::new(Text::from(lines))
                .style(self.theme.text())
                .scroll((self.agents.scroll, 0)),
            sections[0],
        );
        frame.render_widget(
            Paragraph::new(self.agents.hint(&self.theme)).style(self.theme.dim()),
            sections[1],
        );
    }

    fn render_status(&mut self, frame: &mut Frame, area: Rect) {
        let palette = self.theme.palette;
        frame.render_widget(
            Block::default().style(Style::default().bg(palette.status_bg)),
            area,
        );

        let focus = if self.focus == Focus::Chat {
            if self.chat_insert {
                "CHAT·INSERT"
            } else {
                "CHAT"
            }
        } else {
            match self.focus {
                Focus::Plan => "PLAN",
                Focus::Diff => "DIFF",
                Focus::Agents => "AGENTS",
                Focus::Chat => "CHAT",
            }
        };

        let mut spans = vec![Span::styled(
            format!(" {focus} "),
            Style::default()
                .bg(palette.accent)
                .fg(palette.bg)
                .add_modifier(Modifier::BOLD),
        )];
        spans.push(Span::styled(
            format!(" {}/{} ", self.provider, self.model),
            Style::default().fg(palette.fg),
        ));
        spans.push(Span::styled(
            format!(
                "· {} · {} tok ",
                self.mode,
                format_tokens(self.tokens_in + self.tokens_out)
            ),
            Style::default().fg(palette.dim),
        ));

        spans.push(Span::styled(
            format!("· {} ", truncate_chars(&self.current_session_title(), 28)),
            Style::default().fg(palette.dim),
        ));

        if let Some(plan) = &self.plan.plan {
            let done = plan
                .steps
                .iter()
                .filter(|step| step.status == crate::agent::plan::StepStatus::Done)
                .count();
            spans.push(Span::styled(
                format!("· plan {done}/{} ", plan.steps.len()),
                Style::default().fg(palette.dim),
            ));
        }

        if self.approval.is_some() {
            spans.push(Span::styled(
                " APPROVAL REQUIRED ",
                Style::default().bg(palette.error).fg(palette.bg),
            ));
        } else if self.busy {
            spans.push(Span::styled(
                format!("{} ", spinner(self.spinner)),
                Style::default().fg(palette.accent),
            ));
            if let Some(started) = self.turn_started {
                spans.push(Span::styled(
                    format!("{}s ", started.elapsed().as_secs()),
                    Style::default().fg(palette.dim),
                ));
            }
        }

        let (message, message_color) = if let Some(toast) = &self.toast {
            (toast.text.clone(), toast.color)
        } else {
            (self.status_text.clone(), palette.dim)
        };
        spans.push(Span::styled(message, Style::default().fg(message_color)));
        spans.push(Span::styled(
            "  · Ctrl+K palette · F2 settings · ? help ",
            Style::default().fg(palette.dim),
        ));
        spans.push(Span::styled(
            format!(
                "· {} ",
                truncate_chars(&self.ctx.cwd.display().to_string(), 28)
            ),
            Style::default().fg(palette.dim),
        ));

        frame.render_widget(
            Paragraph::new(Line::from(spans)).style(Style::default().bg(palette.status_bg)),
            area,
        );
    }

    fn render_help(&self, frame: &mut Frame) {
        let popup = popup_rect(frame.area(), 72, 72);
        let inner = popup_block(frame, popup, "Help", &self.theme);
        let text = vec![
            help_line(
                "Global",
                "Ctrl+K palette · Tab/Shift+Tab focus · Ctrl+C quit",
                &self.theme,
            ),
            help_line(
                "Chat",
                "Esc normal · i/Enter insert · Enter send · j/k scroll · g/G top/bottom",
                &self.theme,
            ),
            help_line(
                "Plan",
                "j/k select · space approve · a/n all/none · e edit · r reorder · d delete · Enter execute",
                &self.theme,
            ),
            help_line(
                "Diff",
                "j/k hunk · h/l file · a/A accept hunk/file · r/R reject · u unified/split · ←/→ scroll",
                &self.theme,
            ),
            help_line(
                "Agents",
                "j/k select · live read-only subagent status",
                &self.theme,
            ),
            help_line(
                "Chat commands",
                "/plan <task> · /mode [mode] · /model · /clear · /help · /quit",
                &self.theme,
            ),
            Line::from(""),
            Line::from(Span::styled("Esc or ? closes this help.", self.theme.dim())),
        ];
        frame.render_widget(
            Paragraph::new(Text::from(text))
                .style(self.theme.text())
                .wrap(Wrap { trim: false }),
            inner,
        );
    }

    fn render_palette(&self, frame: &mut Frame, state: &PaletteState) {
        let popup = popup_rect(frame.area(), 62, 66);
        let inner = popup_block(frame, popup, "Command palette", &self.theme);
        let sections = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(1),
        ])
        .split(inner);

        let query = state.input.value();
        let (visible, cursor_col) = state
            .input
            .window(sections[0].width.saturating_sub(3) as usize);
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("> ", Style::default().fg(self.theme.palette.accent)),
                Span::styled(
                    if visible.is_empty() {
                        "type to filter…".to_string()
                    } else {
                        visible
                    },
                    if state.input.is_empty() {
                        self.theme.dim()
                    } else {
                        self.theme.text()
                    },
                ),
            ])),
            sections[0],
        );
        frame.set_cursor_position(Position::new(
            (sections[0].x + 2 + cursor_col as u16).min(sections[0].right().saturating_sub(1)),
            sections[0].y,
        ));
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "─".repeat(sections[1].width as usize),
                self.theme.dim(),
            ))),
            sections[1],
        );

        let actions = self.filtered_actions(&query);
        let height = sections[2].height as usize;
        let offset = state.selected.saturating_sub(height.saturating_sub(1));
        let lines: Vec<Line> = actions
            .iter()
            .enumerate()
            .skip(offset)
            .take(height)
            .map(|(index, action)| {
                let style = if index == state.selected {
                    Style::default()
                        .bg(self.theme.palette.selection_bg)
                        .fg(self.theme.palette.fg)
                } else {
                    self.theme.text()
                };
                Line::from(Span::styled(format!("  {}", action.label()), style))
            })
            .collect();
        let lines = if lines.is_empty() {
            vec![Line::from(Span::styled("  no matches", self.theme.dim()))]
        } else {
            lines
        };
        frame.render_widget(Paragraph::new(Text::from(lines)), sections[2]);
    }

    fn render_models(&self, frame: &mut Frame, picker: &ModelPicker) {
        let popup = popup_rect(frame.area(), 56, 64);
        let title = format!("Models · {}", picker.provider);
        let inner = popup_block(frame, popup, &title, &self.theme);
        let sections = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(1),
        ])
        .split(inner);

        let (visible, cursor_col) = picker
            .input
            .window(sections[0].width.saturating_sub(3) as usize);
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("> ", Style::default().fg(self.theme.palette.accent)),
                Span::styled(
                    if visible.is_empty() {
                        "filter models…".to_string()
                    } else {
                        visible
                    },
                    if picker.input.is_empty() {
                        self.theme.dim()
                    } else {
                        self.theme.text()
                    },
                ),
            ])),
            sections[0],
        );
        frame.set_cursor_position(Position::new(
            (sections[0].x + 2 + cursor_col as u16).min(sections[0].right().saturating_sub(1)),
            sections[0].y,
        ));
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "─".repeat(sections[1].width as usize),
                self.theme.dim(),
            ))),
            sections[1],
        );

        let models = filtered_models(picker);
        let height = sections[2].height as usize;
        let offset = picker.selected.saturating_sub(height.saturating_sub(1));
        let mut lines: Vec<Line> = Vec::new();
        if picker.loading {
            lines.push(Line::from(Span::styled(
                "  loading models…",
                self.theme.dim(),
            )));
        } else if models.is_empty() {
            lines.push(Line::from(Span::styled(
                "  no models found",
                self.theme.dim(),
            )));
        } else {
            for (index, model) in models.iter().enumerate().skip(offset).take(height) {
                let selected = index == picker.selected;
                let current = model == &picker.current;
                let style = if selected {
                    Style::default()
                        .bg(self.theme.palette.selection_bg)
                        .fg(self.theme.palette.fg)
                } else {
                    self.theme.text()
                };
                lines.push(Line::from(vec![
                    Span::styled(
                        if current { " * " } else { "   " },
                        style.fg(self.theme.palette.success),
                    ),
                    Span::styled(model.clone(), style),
                ]));
            }
        }
        frame.render_widget(Paragraph::new(Text::from(lines)), sections[2]);
    }

    fn render_tools(&self, frame: &mut Frame, state: &ToolsOverlay) {
        let popup = popup_rect(frame.area(), 86, 76);
        let inner = popup_block(frame, popup, "Tool log", &self.theme);
        let mut lines: Vec<Line> = Vec::new();
        let mut selected_line = 0usize;

        if self.tools.is_empty() {
            lines.push(Line::from(Span::styled(
                "No tool calls yet.",
                self.theme.dim(),
            )));
        }
        for (index, tool) in self.tools.iter().enumerate() {
            let selected = index == state.selected;
            if selected {
                selected_line = lines.len();
            }
            let (icon, color) = if tool.output.is_empty() {
                ("▸", self.theme.palette.tool)
            } else if tool.is_error {
                ("✗", self.theme.palette.error)
            } else {
                ("✓", self.theme.palette.success)
            };
            let duration = tool
                .duration_ms
                .map(|ms| format!(" ({ms} ms)"))
                .unwrap_or_default();
            let style = if selected {
                Style::default().bg(self.theme.palette.selection_bg)
            } else {
                Style::default()
            };
            lines.push(Line::from(vec![
                Span::styled(format!("{icon} "), style.fg(color)),
                Span::styled(
                    format!("{}{duration}", tool.name),
                    style.fg(self.theme.palette.fg),
                ),
                Span::styled(
                    format!("  {}", one_line(&tool.arguments, 120)),
                    style.fg(self.theme.palette.dim),
                ),
            ]));
            if selected || tool.expanded {
                let output = if tool.output.is_empty() {
                    "running…"
                } else {
                    &tool.output
                };
                for line in output
                    .lines()
                    .take(if tool.expanded { usize::MAX } else { 24 })
                {
                    lines.push(Line::from(Span::styled(
                        format!(
                            "  {}",
                            truncate_chars(line, inner.width.saturating_sub(4) as usize)
                        ),
                        Style::default().fg(if tool.is_error {
                            self.theme.palette.error
                        } else {
                            self.theme.palette.dim
                        }),
                    )));
                }
                lines.push(Line::from(""));
            }
        }

        let height = inner.height as usize;
        let offset = selected_line.saturating_sub(height.saturating_sub(1));
        frame.render_widget(
            Paragraph::new(Text::from(lines)).scroll((offset.min(u16::MAX as usize) as u16, 0)),
            inner,
        );
    }

    fn render_plan_edit(&self, frame: &mut Frame, state: &PlanEditState) {
        let popup = popup_rect(frame.area(), 70, 26);
        let title = format!("Edit step {}", state.index + 1);
        let inner = popup_block(frame, popup, &title, &self.theme);
        let sections = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(1),
        ])
        .split(inner);
        let (visible, cursor_col) = state
            .input
            .window(sections[0].width.saturating_sub(1) as usize);
        frame.render_widget(
            Paragraph::new(visible).style(self.theme.text()),
            sections[0],
        );
        frame.set_cursor_position(Position::new(
            (sections[0].x + cursor_col as u16).min(sections[0].right().saturating_sub(1)),
            sections[0].y,
        ));
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "Enter save · Esc cancel",
                self.theme.dim(),
            ))),
            sections[2],
        );
    }

    fn render_settings(&self, frame: &mut Frame, state: &SettingsState) {
        let popup = popup_rect(frame.area(), 76, 44);
        let inner = popup_block(frame, popup, "Settings", &self.theme);
        let rows = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(1),
        ])
        .split(inner);

        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "OpenCode API key",
                Style::default()
                    .fg(self.theme.palette.accent)
                    .add_modifier(Modifier::BOLD),
            ))),
            rows[0],
        );

        let path = credentials::path()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|_| "credentials.toml".to_string());
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                truncate_chars(
                    &format!("Stored at: {path} (mode 0600, outside config.toml)"),
                    rows[1].width as usize,
                ),
                self.theme.dim(),
            ))),
            rows[1],
        );

        let env_set = std::env::var("OPENCODE_API_KEY")
            .map(|value| !value.trim().is_empty())
            .unwrap_or(false);
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                if env_set {
                    "OPENCODE_API_KEY is set and takes precedence over the stored key."
                } else {
                    "Paste a key below and press Enter. It is never shown in config output."
                },
                self.theme.dim(),
            ))),
            rows[2],
        );

        if let Some((status, color)) = &state.status {
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    truncate_chars(status, rows[3].width as usize),
                    Style::default().fg(*color),
                ))),
                rows[3],
            );
        }

        let (visible, cursor_col) = state.input.window(rows[5].width.saturating_sub(5) as usize);
        let masked = "•".repeat(visible.chars().count());
        let mut spans = vec![Span::styled("Key: ", self.theme.dim())];
        if masked.is_empty() {
            spans.push(Span::styled("paste or type an API key…", self.theme.dim()));
        } else {
            spans.push(Span::styled(masked, self.theme.text()));
        }
        frame.render_widget(Paragraph::new(Line::from(spans)), rows[5]);
        frame.set_cursor_position(Position::new(
            (rows[5].x + 5 + cursor_col as u16).min(rows[5].right().saturating_sub(1)),
            rows[5].y,
        ));

        frame.render_widget(
            Paragraph::new(Text::from(vec![
                Line::from(Span::styled(
                    "Enter save · Esc cancel · Ctrl+D remove stored key",
                    self.theme.dim(),
                )),
                Line::from(""),
                Line::from(Span::styled(
                    "After saving, use Ctrl+K → “Switch provider: opencode”.",
                    self.theme.dim(),
                )),
            ])),
            rows[6],
        );
    }

    fn render_sessions(&self, frame: &mut Frame, state: &SessionListState) {
        let popup = popup_rect(frame.area(), 68, 62);
        let inner = popup_block(frame, popup, "Sessions", &self.theme);
        let sections = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(inner);
        let height = sections[0].height as usize;
        let offset = state.selected.saturating_sub(height.saturating_sub(1));
        let mut lines = Vec::new();
        if self.session_meta.is_empty() {
            lines.push(Line::from(Span::styled("No sessions", self.theme.dim())));
        }
        for (index, meta) in self
            .session_meta
            .iter()
            .enumerate()
            .skip(offset)
            .take(height)
        {
            let current = meta.id == self.current_session;
            let selected = index == state.selected;
            let style = if selected {
                Style::default()
                    .bg(self.theme.palette.selection_bg)
                    .fg(self.theme.palette.fg)
            } else if current {
                Style::default()
                    .fg(self.theme.palette.accent)
                    .add_modifier(Modifier::BOLD)
            } else {
                self.theme.text()
            };
            let marker = if current { "*" } else { " " };
            let age = meta.updated.elapsed().as_secs();
            let when = if age < 60 {
                format!("{age}s")
            } else if age < 3_600 {
                format!("{}m", age / 60)
            } else {
                format!("{}h", age / 3_600)
            };
            lines.push(Line::from(Span::styled(
                format!(
                    " {marker} {}  ({} turns, {when} ago)",
                    truncate_chars(&meta.title, 36),
                    meta.turns
                ),
                style,
            )));
        }
        frame.render_widget(Paragraph::new(Text::from(lines)), sections[0]);
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "Enter switch · n new · d delete · Esc close",
                self.theme.dim(),
            ))),
            sections[1],
        );
    }

    fn render_approval(&self, frame: &mut Frame) {
        let Some(approval) = &self.approval else {
            return;
        };
        let popup = popup_rect(frame.area(), 72, 56);
        let inner = popup_block(frame, popup, "Approval required", &self.theme);
        let request = &approval.request;

        let mut lines = vec![
            Line::from(vec![
                Span::styled("tool: ", self.theme.dim()),
                Span::styled(
                    request.tool.clone(),
                    Style::default()
                        .fg(self.theme.palette.accent)
                        .add_modifier(Modifier::BOLD),
                ),
            ]),
            Line::from(vec![
                Span::styled("subject: ", self.theme.dim()),
                Span::styled(
                    request.subject.clone(),
                    Style::default().fg(self.theme.palette.fg),
                ),
            ]),
            Line::from(vec![
                Span::styled("reason: ", self.theme.dim()),
                Span::styled(
                    request.reason.clone(),
                    Style::default().fg(self.theme.palette.warning),
                ),
            ]),
        ];
        if let Some(step) = request.plan_step {
            lines.push(Line::from(vec![
                Span::styled("plan step: ", self.theme.dim()),
                Span::styled(step.to_string(), self.theme.text()),
            ]));
        }
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled("arguments:", self.theme.dim())));
        let pretty = serde_json::to_string_pretty(&request.arguments)
            .unwrap_or_else(|_| request.arguments.to_string());
        for line in pretty.lines().take(40) {
            lines.push(Line::from(Span::styled(
                format!(
                    "  {}",
                    slice_chars(line, 0, inner.width.saturating_sub(4) as usize)
                ),
                self.theme.text(),
            )));
        }
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::styled(
                "y",
                Style::default()
                    .fg(self.theme.palette.success)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" allow once   ", self.theme.text()),
            Span::styled(
                "a",
                Style::default()
                    .fg(self.theme.palette.success)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" always this tool   ", self.theme.text()),
            Span::styled(
                "n",
                Style::default()
                    .fg(self.theme.palette.error)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" deny   ", self.theme.text()),
            Span::styled(
                "Esc",
                Style::default()
                    .fg(self.theme.palette.error)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" deny", self.theme.text()),
        ]));

        frame.render_widget(
            Paragraph::new(Text::from(lines))
                .style(self.theme.text())
                .scroll((approval.scroll, 0)),
            inner,
        );
    }
}

fn pane(frame: &mut Frame, area: Rect, title: &str, focused: bool, theme: &Theme) -> Rect {
    let border = if focused {
        theme.palette.border_focus
    } else {
        theme.palette.border
    };
    let title_color = if focused {
        theme.palette.accent
    } else {
        theme.palette.dim
    };
    let block = Block::default()
        .borders(Borders::TOP)
        .border_style(Style::default().fg(border))
        .title(Span::styled(
            format!(" {title} "),
            Style::default().fg(title_color),
        ));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    inner
}

fn popup_block(frame: &mut Frame, popup: Rect, title: &str, theme: &Theme) -> Rect {
    frame.render_widget(Clear, popup);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.palette.border_focus))
        .style(Style::default().bg(theme.palette.bg))
        .title(Span::styled(
            format!(" {title} "),
            Style::default()
                .fg(theme.palette.accent)
                .add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    inner
}

fn help_line<'a>(section: &'a str, text: &'a str, theme: &Theme) -> Line<'a> {
    Line::from(vec![
        Span::styled(
            format!("{section:<13}"),
            Style::default()
                .fg(theme.palette.accent)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(text, theme.text()),
    ])
}

fn filtered_models(picker: &ModelPicker) -> Vec<String> {
    let query = picker.input.value().to_ascii_lowercase();
    picker
        .models
        .iter()
        .filter(|model| query.is_empty() || model.to_ascii_lowercase().contains(&query))
        .cloned()
        .collect()
}

fn fuzzy_score(haystack: &str, needle: &str) -> Option<i32> {
    if needle.is_empty() {
        return Some(0);
    }
    let mut score = 0;
    let mut position = 0;
    for ch in needle.chars() {
        let index = haystack[position..].find(ch)?;
        score += if index == 0 { 4 } else { 1 };
        position += index + ch.len_utf8();
    }
    Some(score)
}

fn rect_contains(rect: Rect, x: u16, y: u16) -> bool {
    x >= rect.x && x < rect.right() && y >= rect.y && y < rect.bottom()
}

fn format_tokens(tokens: u64) -> String {
    if tokens >= 1_000_000 {
        format!("{:.1}M", tokens as f64 / 1_000_000.0)
    } else if tokens >= 1_000 {
        format!("{:.1}k", tokens as f64 / 1_000.0)
    } else {
        tokens.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::plan::{Plan, PlanStep, Risk, StepStatus};
    use crate::ui::chat::ChatItem;
    use ratatui::backend::TestBackend;
    use tempfile::tempdir;
    use tokio::sync::mpsc;

    fn test_app(root: &std::path::Path) -> (App, mpsc::UnboundedReceiver<AgentCommand>) {
        let config = Config::default();
        let ctx = ToolContext::from_config(root, &config.tools).unwrap();
        let (command_tx, command_rx) = mpsc::unbounded_channel();
        let (_ui_tx, ui_rx) = mpsc::unbounded_channel();
        let app = App::new(
            config,
            root.join("config.toml"),
            ctx,
            "ollama".to_string(),
            "qwen3.5:9b".to_string(),
            command_tx,
            ui_rx,
        );
        (app, command_rx)
    }

    #[test]
    fn renders_all_panes() {
        let dir = tempdir().unwrap();
        let (mut app, _rx) = test_app(dir.path());
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();

        let mut plan = Plan {
            title: "Test plan".to_string(),
            goal: "Do the task".to_string(),
            steps: vec![PlanStep {
                description: "First step".to_string(),
                files: vec!["src/main.rs".to_string()],
                risk: Risk::Low,
                status: StepStatus::Pending,
            }],
            notes: None,
        };
        app.on_bus_event(Event::Plan(plan.clone()));
        plan.steps[0].status = StepStatus::Done;
        app.on_bus_event(Event::Plan(plan));
        terminal.draw(|frame| app.draw(frame)).unwrap();
        assert!(app.plan.review || !app.plan.running);
    }

    #[test]
    fn creates_and_switches_sessions() {
        let dir = tempdir().unwrap();
        let (mut app, mut rx) = test_app(dir.path());
        let first = app.current_session.clone();
        app.chat.push_user("hello session");
        app.record_prompt("hello session");
        assert_eq!(app.session_meta[0].title, "hello session");

        app.new_session();
        assert_ne!(app.current_session, first);
        assert_eq!(app.session_meta.len(), 2);
        match rx.try_recv() {
            Ok(AgentCommand::NewSession(id)) => assert_eq!(id, app.current_session),
            _ => panic!("expected NewSession"),
        }
        assert!(
            matches!(app.chat.items.last(), Some(ChatItem::Info(text)) if text.contains("New session"))
        );

        app.switch_session(first.clone());
        match rx.try_recv() {
            Ok(AgentCommand::SwitchSession(id)) => assert_eq!(id, first),
            _ => panic!("expected SwitchSession"),
        }
        assert_eq!(app.current_session, first);
        assert!(app.chat.items.iter().any(|item| matches!(
            item,
            ChatItem::User(text) if text == "hello session"
        )));
    }

    #[test]
    fn mouse_click_focuses_panes() {
        let dir = tempdir().unwrap();
        let (mut app, _rx) = test_app(dir.path());
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();

        let plan = app.layout.plan;
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: plan.x + 2,
            row: plan.y + 2,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(app.focus, Focus::Plan);

        let input = app.layout.input;
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: input.x + 2,
            row: input.y + 1,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(app.focus, Focus::Chat);
        assert!(app.chat_insert);
    }

    #[test]
    fn typing_and_enter_submits_a_prompt() {
        let dir = tempdir().unwrap();
        let (mut app, mut rx) = test_app(dir.path());
        for ch in "hello".chars() {
            app.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
        }
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        match rx.try_recv() {
            Ok(AgentCommand::Submit(prompt)) => assert_eq!(prompt, "hello"),
            _ => panic!("expected Submit command"),
        }
        assert!(app.busy);
        assert!(!app.chat.items.is_empty());
    }

    #[test]
    fn slash_plan_opens_plan_mode() {
        let dir = tempdir().unwrap();
        let (mut app, mut rx) = test_app(dir.path());
        for ch in "/plan create x".chars() {
            app.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
        }
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(app.plan.pending);
        match rx.try_recv() {
            Ok(AgentCommand::GeneratePlan(task)) => assert_eq!(task, "create x"),
            _ => panic!("expected GeneratePlan command"),
        }
    }

    #[test]
    fn diff_reject_key_reverts_file_and_renders() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "one\nTWO\n").unwrap();
        let (mut app, _rx) = test_app(dir.path());
        app.on_agent_event(AgentEvent::Diff {
            tool: "edit_file".to_string(),
            diff: "--- a/a.txt\n+++ b/a.txt\n@@ -1,2 +1,2 @@\n one\n-two\n+TWO\n".to_string(),
        });
        app.focus_pane(Focus::Diff, false);
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        app.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "one\ntwo\n");
        assert_eq!(
            app.diff.files[0].status,
            crate::ui::diff::FileStatus::Rejected
        );
        app.diff.toggle_mode();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        assert_eq!(app.diff.mode, crate::ui::diff::DiffMode::Split);
    }

    #[test]
    fn fuzzy_matches_subsequences() {
        assert!(fuzzy_score("plan a task", "pat").is_some());
        assert!(fuzzy_score("plan a task", "zzz").is_none());
        assert_eq!(fuzzy_score("anything", ""), Some(0));
    }

    #[test]
    fn formats_token_counts() {
        assert_eq!(format_tokens(12), "12");
        assert_eq!(format_tokens(1_500), "1.5k");
        assert_eq!(format_tokens(2_000_000), "2.0M");
    }
}
