use anyhow::Result;
use catsolle_config::{AiConfig, AppConfig, ConfigManager, I18n};
use catsolle_core::{
    AuthMethod, Connection, ConnectionStore, Event as CoreEvent, EventBus, SessionManager,
    TransferEndpoint, TransferFile, TransferJob, TransferOptions, TransferProgress, TransferQueue,
    TransferState,
};
use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use directories::UserDirs;
use fluent_bundle::FluentArgs;
use futures::{
    future::{pending, Either},
    StreamExt, TryStreamExt,
};
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Wrap};
use ratatui::Terminal;
use serde::{Deserialize, Serialize};
use std::collections::{HashSet, VecDeque};
use std::io::{self, Stdout};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio::time::timeout;
use uuid::Uuid;
use vt100::Parser;
use walkdir::WalkDir;
use zeroize::Zeroizing;

struct AppRunContext {
    store: ConnectionStore,
    sessions: Arc<SessionManager>,
    queue: TransferQueue,
    bus: EventBus,
    config_manager: ConfigManager,
    config: AppConfig,
    i18n: I18n,
}

pub async fn run(
    store: ConnectionStore,
    sessions: Arc<SessionManager>,
    queue: TransferQueue,
    bus: EventBus,
    config_manager: ConfigManager,
    config: AppConfig,
    i18n: I18n,
) -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = ratatui::backend::CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let ctx = AppRunContext {
        store,
        sessions,
        queue,
        bus,
        config_manager,
        config,
        i18n,
    };
    let res = run_app(&mut terminal, ctx).await;

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    res
}

async fn run_app(
    terminal: &mut Terminal<ratatui::backend::CrosstermBackend<Stdout>>,
    ctx: AppRunContext,
) -> Result<()> {
    let mut event_stream = EventStream::new();
    let mut event_rx = ctx.bus.subscribe();
    let (assistant_tx, mut assistant_rx) = mpsc::channel::<AssistantEvent>(16);
    let mut app = AppState::new(
        ctx.store,
        ctx.sessions,
        ctx.queue,
        ctx.config_manager,
        ctx.config,
        ctx.i18n,
        assistant_tx,
    )
    .await?;

    loop {
        terminal.draw(|f| app.draw(f))?;
        app.apply_pending_resize().await?;

        let output_fut = if app.shell.is_some() {
            Either::Left(async { app.shell.as_mut().unwrap().read().await })
        } else {
            Either::Right(pending::<Option<Vec<u8>>>())
        };

        tokio::select! {
            maybe_event = event_stream.next() => {
                if let Some(Ok(event)) = maybe_event {
                    if app.handle_event(event).await? {
                        break;
                    }
                }
            }
            maybe_output = output_fut => {
                if let Some(data) = maybe_output {
                    app.terminal_parser.process(&data);
                }
            }
            maybe_bus = event_rx.recv() => {
                match maybe_bus {
                    Ok(event) => {
                        app.handle_bus_event(event).await?;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {}
                }
            }
            maybe_assistant = assistant_rx.recv() => {
                if let Some(event) = maybe_assistant {
                    app.handle_assistant_event(event);
                }
            }
        }
    }

    Ok(())
}

struct AppState {
    store: ConnectionStore,
    sessions: Arc<SessionManager>,
    queue: TransferQueue,
    config_manager: ConfigManager,
    config: AppConfig,
    i18n: I18n,
    theme: Theme,
    ai_client: reqwest::Client,
    assistant: AssistantState,
    assistant_tx: mpsc::Sender<AssistantEvent>,
    connections: Vec<Connection>,
    selected: usize,
    mode: AppMode,
    terminal_parser: Parser,
    shell: Option<catsolle_ssh::SshShell>,
    left_panel: PanelState,
    right_panel: PanelState,
    active_panel_left: bool,
    input_focus: InputFocus,
    show_file_manager: bool,
    show_ai_panel: bool,
    status_message: Option<String>,
    transfer_status: Option<TransferStatus>,
    completed_transfers: HashSet<Uuid>,
    pending_tools: VecDeque<ToolCall>,
    tool_busy: bool,
    agent_steps_remaining: u32,
    overlay: Overlay,
    active_connection: Option<Connection>,
    terminal_size: Option<(u16, u16)>,
    pending_shell_resize: Option<(u16, u16)>,
}

#[derive(Clone, Debug)]
enum AppMode {
    Connections,
    Session { id: Uuid },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum InputFocus {
    Terminal,
    Files,
    Assistant,
}

#[derive(Clone, Copy, Debug)]
struct Theme {
    accent: Color,
    accent_soft: Color,
    accent_alt: Color,
    text: Color,
    muted: Color,
    error: Color,
    selection_bg: Color,
    selection_fg: Color,
    selection_inactive_bg: Color,
    selection_inactive_fg: Color,
}

impl Theme {
    fn kawaii() -> Self {
        Self {
            accent: Color::LightMagenta,
            accent_soft: Color::LightCyan,
            accent_alt: Color::LightYellow,
            text: Color::White,
            muted: Color::Gray,
            error: Color::LightRed,
            selection_bg: Color::LightMagenta,
            selection_fg: Color::Black,
            selection_inactive_bg: Color::LightCyan,
            selection_inactive_fg: Color::Black,
        }
    }
}

#[derive(Clone, Debug)]
enum Overlay {
    None,
    Help,
    QuickAdd {
        input: String,
        error: Option<String>,
    },
    Edit {
        id: Uuid,
        input: String,
        error: Option<String>,
    },
    Password {
        id: Uuid,
        state: PasswordOverlayState,
    },
    AiSettings {
        state: Box<AiSettingsState>,
    },
}

#[derive(Clone, Copy, Debug)]
enum PasswordFocus {
    Password,
    Master,
}

#[derive(Clone, Copy, Debug)]
enum PasswordMode {
    Connect,
    SaveOnly,
}

#[derive(Clone, Debug)]
struct PasswordOverlayState {
    password: String,
    master: String,
    save: bool,
    focus: PasswordFocus,
    mode: PasswordMode,
    error: Option<String>,
}

#[derive(Clone, Debug)]
struct AiSettingsState {
    selected: usize,
    editing: bool,
    input: String,
    error: Option<String>,
    draft: AiConfigDraft,
}

#[derive(Clone, Debug)]
struct AiConfigDraft {
    enabled: bool,
    provider: String,
    endpoint: String,
    api_key: String,
    model: String,
    temperature: String,
    max_tokens: String,
    history_max: String,
    timeout_ms: String,
    streaming: bool,
    agent_enabled: bool,
    auto_mode: bool,
    max_steps: String,
    tools_enabled: bool,
    system_prompt: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AiSettingsField {
    Enabled,
    Provider,
    Endpoint,
    ApiKey,
    Model,
    Temperature,
    MaxTokens,
    History,
    Timeout,
    Streaming,
    Agent,
    AutoMode,
    MaxSteps,
    Tools,
    SystemPrompt,
}

const AI_SETTINGS_FIELDS: [AiSettingsField; 15] = [
    AiSettingsField::Enabled,
    AiSettingsField::Provider,
    AiSettingsField::Endpoint,
    AiSettingsField::ApiKey,
    AiSettingsField::Model,
    AiSettingsField::Temperature,
    AiSettingsField::MaxTokens,
    AiSettingsField::History,
    AiSettingsField::Timeout,
    AiSettingsField::Streaming,
    AiSettingsField::Agent,
    AiSettingsField::AutoMode,
    AiSettingsField::MaxSteps,
    AiSettingsField::Tools,
    AiSettingsField::SystemPrompt,
];

#[derive(Clone, Debug)]
struct PanelState {
    kind: PanelKind,
    path: String,
    entries: Vec<FileEntry>,
    selected: usize,
    scroll: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PanelKind {
    Local,
    Remote,
}

#[derive(Clone, Debug)]
struct FileEntry {
    name: String,
    is_dir: bool,
    size: u64,
}

#[derive(Clone, Debug)]
struct AssistantState {
    input: String,
    messages: Vec<AssistantMessage>,
    scroll: usize,
    busy: bool,
    stream_index: Option<usize>,
}

#[derive(Clone, Debug)]
struct AssistantMessage {
    role: AssistantRole,
    content: String,
}

#[derive(Clone, Debug, Deserialize)]
struct ToolCall {
    name: String,
    args: serde_json::Value,
}

#[derive(Clone, Debug)]
struct ToolResult {
    call: ToolCall,
    success: bool,
    output: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AssistantRole {
    User,
    Assistant,
    System,
    Error,
    Tool,
}

#[derive(Clone, Debug)]
enum AssistantEvent {
    Start,
    Delta(String),
    Done(String),
    Error(String),
    ToolResult(ToolResult),
}

#[derive(Clone, Debug)]
struct TransferStatus {
    progress: TransferProgress,
}

impl AppState {
    async fn new(
        store: ConnectionStore,
        sessions: Arc<SessionManager>,
        queue: TransferQueue,
        config_manager: ConfigManager,
        config: AppConfig,
        i18n: I18n,
        assistant_tx: mpsc::Sender<AssistantEvent>,
    ) -> Result<Self> {
        let connections = store.list_connections().unwrap_or_default();
        let mut parser = Parser::new(24, 80, 0);
        parser.process(b"");
        let mut client_builder = reqwest::Client::builder();
        if config.ai.timeout_ms > 0 {
            client_builder = client_builder.timeout(Duration::from_millis(config.ai.timeout_ms));
        }
        let ai_client = client_builder.build()?;
        let assistant = AssistantState::new(&i18n);
        let mut state = Self {
            store,
            sessions,
            queue,
            config_manager,
            config,
            i18n,
            theme: Theme::kawaii(),
            ai_client,
            assistant,
            assistant_tx,
            connections,
            selected: 0,
            mode: AppMode::Connections,
            terminal_parser: parser,
            shell: None,
            left_panel: PanelState::local_default(),
            right_panel: PanelState::remote_default(),
            active_panel_left: true,
            input_focus: InputFocus::Files,
            show_file_manager: true,
            show_ai_panel: false,
            status_message: None,
            transfer_status: None,
            completed_transfers: HashSet::new(),
            pending_tools: VecDeque::new(),
            tool_busy: false,
            agent_steps_remaining: 0,
            overlay: Overlay::None,
            active_connection: None,
            terminal_size: None,
            pending_shell_resize: None,
        };
        state.auto_import_if_empty()?;
        Ok(state)
    }

    fn draw(&mut self, f: &mut ratatui::Frame<'_>) {
        match self.mode {
            AppMode::Connections => self.draw_connections(f),
            AppMode::Session { .. } => self.draw_session(f),
        }
        if !matches!(self.overlay, Overlay::None) {
            self.draw_overlay(f);
        }
    }

    fn draw_connections(&mut self, f: &mut ratatui::Frame<'_>) {
        let size = f.area();
        let layout = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Min(5),
                Constraint::Length(3),
            ])
            .split(size);
        self.draw_header(f, layout[0], self.connections_title());
        if self.connections.is_empty() {
            self.draw_empty_connections(f, layout[1]);
        } else {
            let body = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(55), Constraint::Percentage(45)])
                .split(layout[1]);
            self.draw_connection_list(f, body[0]);
            self.draw_connection_details(f, body[1]);
        }
        self.draw_footer(f, layout[2]);
    }

    fn draw_session(&mut self, f: &mut ratatui::Frame<'_>) {
        let size = f.area();
        let layout = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Min(5),
                Constraint::Length(3),
            ])
            .split(size);
        self.draw_header(f, layout[0], self.session_title());
        if self.show_ai_panel {
            let body = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(70), Constraint::Percentage(30)])
                .split(layout[1]);
            self.draw_session_main(f, body[0]);
            self.draw_ai_panel(f, body[1]);
        } else {
            self.draw_session_main(f, layout[1]);
        }
        self.draw_footer(f, layout[2]);
    }

    fn draw_session_main(&mut self, f: &mut ratatui::Frame<'_>, area: Rect) {
        if self.show_file_manager {
            let body = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Percentage(60), Constraint::Percentage(40)])
                .split(area);
            self.draw_terminal(f, body[0]);
            self.draw_file_manager(f, body[1]);
        } else {
            self.draw_terminal(f, area);
        }
    }

    fn draw_terminal(&mut self, f: &mut ratatui::Frame<'_>, area: Rect) {
        let theme = self.theme;
        let mut block = Block::default().borders(Borders::ALL).title("terminal");
        let border = if matches!(self.input_focus, InputFocus::Terminal) {
            theme.accent
        } else {
            theme.muted
        };
        block = block.border_style(Style::default().fg(border));
        let inner = block.inner(area);
        self.update_terminal_size(inner);
        let text = self.terminal_text(inner);
        let paragraph = Paragraph::new(text).block(block);
        f.render_widget(paragraph, area);
    }

    fn draw_file_manager(&mut self, f: &mut ratatui::Frame<'_>, area: Rect) {
        let chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(area);
        let focus = matches!(self.input_focus, InputFocus::Files);
        let left_visible = panel_visible_rows(chunks[0]);
        let right_visible = panel_visible_rows(chunks[1]);
        self.left_panel.ensure_visible(left_visible);
        self.right_panel.ensure_visible(right_visible);
        self.draw_panel(
            f,
            &self.left_panel,
            chunks[0],
            self.active_panel_left,
            focus,
        );
        self.draw_panel(
            f,
            &self.right_panel,
            chunks[1],
            !self.active_panel_left,
            focus,
        );
    }

    fn draw_ai_panel(&mut self, f: &mut ratatui::Frame<'_>, area: Rect) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(5), Constraint::Length(3)])
            .split(area);
        self.draw_ai_messages(f, chunks[0]);
        self.draw_ai_input(f, chunks[1]);
    }

    fn draw_ai_messages(&mut self, f: &mut ratatui::Frame<'_>, area: Rect) {
        let theme = self.theme;
        let title = if self.assistant.busy {
            format!(
                "{} · {}",
                self.i18n.tr("ai-title"),
                self.i18n.tr("status-ai-busy")
            )
        } else {
            self.i18n.tr("ai-title")
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .title(title)
            .border_style(Style::default().fg(theme.accent));
        if !self.config.ai.enabled {
            let text = Text::from(Line::from(Span::styled(
                self.i18n.tr("ai-disabled"),
                Style::default().fg(theme.muted),
            )));
            let paragraph = Paragraph::new(text)
                .block(block)
                .wrap(Wrap { trim: false })
                .style(Style::default().fg(theme.text));
            f.render_widget(paragraph, area);
            return;
        }
        if let Some(error) = self.ai_config_error() {
            let text = Text::from(Line::from(Span::styled(
                error,
                Style::default().fg(theme.error),
            )));
            let paragraph = Paragraph::new(text)
                .block(block)
                .wrap(Wrap { trim: false })
                .style(Style::default().fg(theme.text));
            f.render_widget(paragraph, area);
            return;
        }

        let lines = self.assistant_lines();
        let visible = area.height.saturating_sub(2) as usize;
        let max_scroll = lines.len().saturating_sub(visible);
        if self.assistant.scroll > max_scroll {
            self.assistant.scroll = max_scroll;
        }
        let paragraph = Paragraph::new(Text::from(lines))
            .block(block)
            .wrap(Wrap { trim: false })
            .scroll((self.assistant.scroll.min(u16::MAX as usize) as u16, 0))
            .style(Style::default().fg(theme.text));
        f.render_widget(paragraph, area);
    }

    fn draw_ai_input(&self, f: &mut ratatui::Frame<'_>, area: Rect) {
        let theme = self.theme;
        let active = matches!(self.input_focus, InputFocus::Assistant);
        let border = if active {
            theme.accent_alt
        } else {
            theme.muted
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .title(self.i18n.tr("ai-input-title"))
            .border_style(Style::default().fg(border));
        let text = if !self.config.ai.enabled {
            Line::from(Span::styled(
                self.i18n.tr("ai-disabled"),
                Style::default().fg(theme.muted),
            ))
        } else if self.assistant.input.is_empty() {
            Line::from(Span::styled(
                self.i18n.tr("ai-placeholder"),
                Style::default().fg(theme.muted),
            ))
        } else {
            Line::from(vec![
                Span::styled("> ", Style::default().fg(theme.accent_alt)),
                Span::styled(
                    self.assistant.input.clone(),
                    Style::default().fg(theme.text),
                ),
            ])
        };
        let paragraph = Paragraph::new(Text::from(text)).block(block);
        f.render_widget(paragraph, area);
    }

    fn draw_panel(
        &self,
        f: &mut ratatui::Frame<'_>,
        panel: &PanelState,
        area: Rect,
        active: bool,
        focus: bool,
    ) {
        let theme = self.theme;
        let title = format!("{}: {}", panel.kind.label(), panel.path);
        let mut block = Block::default().borders(Borders::ALL).title(title);
        let border = if active {
            if focus {
                theme.accent
            } else {
                theme.accent_soft
            }
        } else {
            theme.muted
        };
        block = block.border_style(Style::default().fg(border));
        let visible = panel_visible_rows(area);
        let start = panel.scroll;
        let end = (start + visible).min(panel.entries.len());
        let items: Vec<ListItem> = panel
            .entries
            .iter()
            .enumerate()
            .skip(start)
            .take(end.saturating_sub(start))
            .map(|(i, e)| {
                let mut name = e.name.clone();
                if e.is_dir {
                    name.push('/');
                }
                let style = if active && i == panel.selected {
                    if focus {
                        Style::default()
                            .fg(theme.selection_fg)
                            .bg(theme.selection_bg)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default()
                            .fg(theme.selection_inactive_fg)
                            .bg(theme.selection_inactive_bg)
                    }
                } else {
                    Style::default().fg(theme.text)
                };
                ListItem::new(Line::from(Span::styled(name, style)))
            })
            .collect();
        let list = List::new(items).block(block);
        f.render_widget(list, area);
    }

    fn draw_header(&self, f: &mut ratatui::Frame<'_>, area: Rect, title: String) {
        let theme = self.theme;
        let mut spans = vec![Span::styled(
            title,
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        )];
        if let Some(status) = self.header_status_line() {
            spans.push(Span::raw("  "));
            spans.push(Span::styled(status, Style::default().fg(theme.accent_soft)));
        }
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme.accent_soft));
        let paragraph = Paragraph::new(Text::from(Line::from(spans))).block(block);
        f.render_widget(paragraph, area);
    }

    fn draw_footer(&self, f: &mut ratatui::Frame<'_>, area: Rect) {
        let theme = self.theme;
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme.accent_soft));
        let paragraph = Paragraph::new(self.footer_text())
            .block(block)
            .wrap(Wrap { trim: true })
            .style(Style::default().fg(theme.accent_soft));
        f.render_widget(paragraph, area);
    }

    fn draw_connection_list(&self, f: &mut ratatui::Frame<'_>, area: Rect) {
        let theme = self.theme;
        let block = Block::default()
            .borders(Borders::ALL)
            .title(self.i18n.tr("connections"))
            .border_style(Style::default().fg(theme.accent_soft));
        let items: Vec<ListItem> = self
            .connections
            .iter()
            .enumerate()
            .map(|(i, c)| {
                let style = if i == self.selected {
                    Style::default()
                        .fg(theme.selection_fg)
                        .bg(theme.selection_bg)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(theme.text)
                };
                ListItem::new(Line::from(vec![Span::styled(
                    format!("{}@{}:{}", c.username, c.host, c.port),
                    style,
                )]))
            })
            .collect();
        let list = List::new(items).block(block);
        f.render_widget(list, area);
    }

    fn draw_connection_details(&self, f: &mut ratatui::Frame<'_>, area: Rect) {
        let theme = self.theme;
        let block = Block::default()
            .borders(Borders::ALL)
            .title(self.i18n.tr("label-details"))
            .border_style(Style::default().fg(theme.accent_soft));
        let lines = if let Some(conn) = self.connections.get(self.selected) {
            let last = conn
                .last_connected_at
                .map(|v| v.to_rfc3339())
                .unwrap_or_else(|| self.i18n.tr("label-never"));
            let tags = if conn.tags.is_empty() {
                self.i18n.tr("label-none")
            } else {
                conn.tags
                    .iter()
                    .map(|t| t.name.clone())
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            vec![
                Line::from(Span::styled(
                    conn.name.clone(),
                    Style::default()
                        .fg(theme.accent)
                        .add_modifier(Modifier::BOLD),
                )),
                Line::from(""),
                Line::from(format!("{}: {}", self.i18n.tr("label-host"), conn.host)),
                Line::from(format!("{}: {}", self.i18n.tr("label-port"), conn.port)),
                Line::from(format!("{}: {}", self.i18n.tr("label-user"), conn.username)),
                Line::from(format!(
                    "{}: {}",
                    self.i18n.tr("label-auth"),
                    self.auth_label(&conn.auth_method)
                )),
                Line::from(format!("{}: {}", self.i18n.tr("label-last"), last)),
                Line::from(format!("{}: {}", self.i18n.tr("label-tags"), tags)),
            ]
        } else {
            vec![Line::from(self.i18n.tr("empty-details"))]
        };
        let paragraph = Paragraph::new(Text::from(lines))
            .block(block)
            .style(Style::default().fg(theme.text));
        f.render_widget(paragraph, area);
    }

    fn draw_empty_connections(&self, f: &mut ratatui::Frame<'_>, area: Rect) {
        let theme = self.theme;
        let text = Text::from(vec![
            Line::from(Span::styled(
                self.i18n.tr("empty-connections-title"),
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
            Line::from(self.i18n.tr("empty-connections-body")),
            Line::from(self.i18n.tr("empty-connections-actions")),
        ]);
        let paragraph = Paragraph::new(text)
            .alignment(Alignment::Center)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(theme.accent_soft)),
            )
            .style(Style::default().fg(theme.text));
        f.render_widget(paragraph, area);
    }

    fn draw_overlay(&self, f: &mut ratatui::Frame<'_>) {
        match &self.overlay {
            Overlay::None => {}
            Overlay::Help => {
                let area = centered_rect(70, 60, f.area());
                self.draw_help_overlay(f, area);
            }
            Overlay::QuickAdd { input, error } => {
                let area = centered_rect(70, 40, f.area());
                self.draw_quick_add_overlay(f, area, input, error.as_deref());
            }
            Overlay::Edit { input, error, .. } => {
                let area = centered_rect(70, 40, f.area());
                self.draw_edit_overlay(f, area, input, error.as_deref());
            }
            Overlay::Password { state, .. } => {
                let area = centered_rect(70, 45, f.area());
                self.draw_password_overlay(f, area, state);
            }
            Overlay::AiSettings { state } => {
                let area = centered_rect(80, 70, f.area());
                self.draw_ai_settings_overlay(f, area, state);
            }
        }
    }

    fn draw_help_overlay(&self, f: &mut ratatui::Frame<'_>, area: Rect) {
        let theme = self.theme;
        let block = Block::default()
            .borders(Borders::ALL)
            .title(self.i18n.tr("help-title"))
            .border_style(Style::default().fg(theme.accent));
        let text = Text::from(vec![
            Line::from(self.i18n.tr("help-connections")),
            Line::from(""),
            Line::from(self.i18n.tr("help-session")),
            Line::from(self.i18n.tr("help-assistant")),
            Line::from(self.i18n.tr("help-ai-settings")),
            Line::from(""),
            Line::from(self.i18n.tr("help-quick-add")),
            Line::from(""),
            Line::from(self.i18n.tr("help-edit")),
            Line::from(self.i18n.tr("help-password")),
        ]);
        f.render_widget(Clear, area);
        let paragraph = Paragraph::new(text)
            .block(block)
            .wrap(Wrap { trim: true })
            .style(Style::default().fg(theme.text));
        f.render_widget(paragraph, area);
    }

    fn draw_quick_add_overlay(
        &self,
        f: &mut ratatui::Frame<'_>,
        area: Rect,
        input: &str,
        error: Option<&str>,
    ) {
        let theme = self.theme;
        let block = Block::default()
            .borders(Borders::ALL)
            .title(self.i18n.tr("prompt-new-connection"))
            .border_style(Style::default().fg(theme.accent));
        let mut lines = vec![
            Line::from(self.i18n.tr("prompt-new-connection-hint")),
            Line::from(""),
            Line::from(Span::styled(
                format!("> {}", input),
                Style::default().fg(theme.accent_alt),
            )),
        ];
        if let Some(error) = error {
            lines.push(Line::from(Span::styled(
                error.to_string(),
                Style::default().fg(theme.error),
            )));
        }
        f.render_widget(Clear, area);
        let paragraph = Paragraph::new(Text::from(lines))
            .block(block)
            .wrap(Wrap { trim: true })
            .style(Style::default().fg(theme.text));
        f.render_widget(paragraph, area);
    }

    fn draw_edit_overlay(
        &self,
        f: &mut ratatui::Frame<'_>,
        area: Rect,
        input: &str,
        error: Option<&str>,
    ) {
        let theme = self.theme;
        let block = Block::default()
            .borders(Borders::ALL)
            .title(self.i18n.tr("prompt-edit-connection"))
            .border_style(Style::default().fg(theme.accent));
        let mut lines = vec![
            Line::from(self.i18n.tr("prompt-edit-connection-hint")),
            Line::from(""),
            Line::from(Span::styled(
                format!("> {}", input),
                Style::default().fg(theme.accent_alt),
            )),
        ];
        if let Some(error) = error {
            lines.push(Line::from(Span::styled(
                error.to_string(),
                Style::default().fg(theme.error),
            )));
        }
        f.render_widget(Clear, area);
        let paragraph = Paragraph::new(Text::from(lines))
            .block(block)
            .wrap(Wrap { trim: true })
            .style(Style::default().fg(theme.text));
        f.render_widget(paragraph, area);
    }

    fn draw_password_overlay(
        &self,
        f: &mut ratatui::Frame<'_>,
        area: Rect,
        state: &PasswordOverlayState,
    ) {
        let theme = self.theme;
        let block = Block::default()
            .borders(Borders::ALL)
            .title(self.i18n.tr("prompt-password-title"))
            .border_style(Style::default().fg(theme.accent));
        let title = match state.mode {
            PasswordMode::Connect => self.i18n.tr("prompt-password-connect"),
            PasswordMode::SaveOnly => self.i18n.tr("prompt-password-save"),
        };
        let mask = "*".repeat(state.password.chars().count());
        let master_mask = "*".repeat(state.master.chars().count());
        let pw_style = if matches!(state.focus, PasswordFocus::Password) {
            Style::default().fg(theme.accent_alt)
        } else {
            Style::default().fg(theme.muted)
        };
        let master_style = if matches!(state.focus, PasswordFocus::Master) {
            Style::default().fg(theme.accent_alt)
        } else {
            Style::default().fg(theme.muted)
        };
        let save_label = if state.save {
            self.i18n.tr("prompt-password-save-on")
        } else {
            self.i18n.tr("prompt-password-save-off")
        };
        let mut lines = vec![
            Line::from(title),
            Line::from(""),
            Line::from(Span::styled(
                format!("{}: {}", self.i18n.tr("prompt-password-label"), mask),
                pw_style,
            )),
            Line::from(Span::styled(
                format!(
                    "{}: {}",
                    self.i18n.tr("prompt-password-master-label"),
                    master_mask
                ),
                master_style,
            )),
            Line::from(format!(
                "{}: {}",
                self.i18n.tr("prompt-password-save-label"),
                save_label
            )),
            Line::from(self.i18n.tr("prompt-password-hint")),
        ];
        if let Some(error) = state.error.as_deref() {
            lines.push(Line::from(Span::styled(
                error.to_string(),
                Style::default().fg(theme.error),
            )));
        }
        f.render_widget(Clear, area);
        let paragraph = Paragraph::new(Text::from(lines))
            .block(block)
            .wrap(Wrap { trim: true })
            .style(Style::default().fg(theme.text));
        f.render_widget(paragraph, area);
    }

    fn draw_ai_settings_overlay(
        &self,
        f: &mut ratatui::Frame<'_>,
        area: Rect,
        state: &AiSettingsState,
    ) {
        let theme = self.theme;
        let block = Block::default()
            .borders(Borders::ALL)
            .title(self.i18n.tr("ai-settings-title"))
            .border_style(Style::default().fg(theme.accent));
        let mut lines = Vec::new();
        for (idx, field) in AI_SETTINGS_FIELDS.iter().enumerate() {
            let label = self.ai_settings_label(*field);
            let value = self.ai_settings_value(state, *field);
            let active = idx == state.selected;
            let label_style = if active {
                Style::default()
                    .fg(theme.accent_alt)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(theme.text)
            };
            let value_style = if active {
                Style::default().fg(theme.accent)
            } else {
                Style::default().fg(theme.muted)
            };
            lines.push(Line::from(vec![
                Span::styled(format!("{label}: "), label_style),
                Span::styled(value, value_style),
            ]));
        }
        lines.push(Line::from(""));
        lines.push(Line::from(self.i18n.tr("ai-settings-hint")));
        if state.editing {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                format!("> {}", state.input),
                Style::default().fg(theme.accent_alt),
            )));
        }
        if let Some(error) = state.error.as_deref() {
            lines.push(Line::from(Span::styled(
                error.to_string(),
                Style::default().fg(theme.error),
            )));
        }
        f.render_widget(Clear, area);
        let paragraph = Paragraph::new(Text::from(lines))
            .block(block)
            .wrap(Wrap { trim: false })
            .style(Style::default().fg(theme.text));
        f.render_widget(paragraph, area);
    }

    fn footer_text(&self) -> Text<'_> {
        match &self.overlay {
            Overlay::QuickAdd { .. } => Text::from(self.i18n.tr("footer-quick-add")),
            Overlay::Edit { .. } => Text::from(self.i18n.tr("footer-edit")),
            Overlay::Password { .. } => Text::from(self.i18n.tr("footer-password")),
            Overlay::Help => Text::from(self.i18n.tr("footer-help")),
            Overlay::AiSettings { .. } => Text::from(self.i18n.tr("footer-ai-settings")),
            Overlay::None => match self.mode {
                AppMode::Connections => Text::from(self.i18n.tr("footer-connections")),
                AppMode::Session { .. } => {
                    if matches!(self.input_focus, InputFocus::Assistant) {
                        Text::from(self.i18n.tr("footer-assistant"))
                    } else {
                        Text::from(self.i18n.tr("footer-session"))
                    }
                }
            },
        }
    }

    fn session_title(&self) -> String {
        let base = format!("{} · {}", self.i18n.tr("app-name"), self.i18n.tr("session"));
        if let Some(conn) = &self.active_connection {
            format!("{} · {}@{}:{}", base, conn.username, conn.host, conn.port)
        } else {
            base
        }
    }

    fn connections_title(&self) -> String {
        format!(
            "{} · {}",
            self.i18n.tr("app-name"),
            self.i18n.tr("connections")
        )
    }

    fn auth_label(&self, auth: &AuthMethod) -> String {
        match auth {
            AuthMethod::Agent => self.i18n.tr("auth-agent"),
            AuthMethod::Password { .. } => self.i18n.tr("auth-password"),
            AuthMethod::Key { .. } => self.i18n.tr("auth-key"),
            AuthMethod::KeyboardInteractive => self.i18n.tr("auth-keyboard"),
            AuthMethod::Certificate { .. } => self.i18n.tr("auth-certificate"),
        }
    }

    fn set_status(&mut self, message: String) {
        self.status_message = Some(message);
    }

    fn header_status_line(&self) -> Option<String> {
        let mut parts = Vec::new();
        if let Some(status) = &self.status_message {
            parts.push(status.clone());
        }
        if let Some(transfer) = &self.transfer_status {
            parts.push(self.format_transfer_status(&transfer.progress));
        }
        if matches!(self.mode, AppMode::Session { .. }) {
            parts.push(self.sftp_status_label());
            parts.push(self.ai_status_label());
        }
        if parts.is_empty() {
            None
        } else {
            Some(parts.join(" | "))
        }
    }

    fn assistant_lines(&self) -> Vec<Line<'static>> {
        let theme = self.theme;
        let mut lines = Vec::new();
        for message in &self.assistant.messages {
            let (label, label_style, text_style) = match message.role {
                AssistantRole::User => (
                    self.i18n.tr("ai-role-user"),
                    Style::default()
                        .fg(theme.accent_alt)
                        .add_modifier(Modifier::BOLD),
                    Style::default().fg(theme.text),
                ),
                AssistantRole::Assistant => (
                    self.i18n.tr("ai-role-assistant"),
                    Style::default()
                        .fg(theme.accent)
                        .add_modifier(Modifier::BOLD),
                    Style::default().fg(theme.text),
                ),
                AssistantRole::System => (
                    self.i18n.tr("ai-role-system"),
                    Style::default().fg(theme.muted),
                    Style::default().fg(theme.muted),
                ),
                AssistantRole::Error => (
                    self.i18n.tr("ai-role-error"),
                    Style::default()
                        .fg(theme.error)
                        .add_modifier(Modifier::BOLD),
                    Style::default().fg(theme.error),
                ),
                AssistantRole::Tool => (
                    self.i18n.tr("ai-role-tool"),
                    Style::default()
                        .fg(theme.accent_alt)
                        .add_modifier(Modifier::BOLD),
                    Style::default().fg(theme.muted),
                ),
            };
            let mut first = true;
            for line in message.content.lines() {
                if first {
                    lines.push(Line::from(vec![
                        Span::styled(format!("{label}: "), label_style),
                        Span::styled(line.to_string(), text_style),
                    ]));
                    first = false;
                } else {
                    lines.push(Line::from(Span::styled(line.to_string(), text_style)));
                }
            }
            lines.push(Line::from(""));
        }
        if lines.is_empty() {
            lines.push(Line::from(self.i18n.tr("ai-empty")));
        }
        if let Some(call) = self.pending_tools.front() {
            let mut args = FluentArgs::new();
            args.set("name", call.name.clone());
            lines.push(Line::from(self.i18n.tr_args("ai-tool-pending", &args)));
        }
        lines
    }

    fn update_terminal_size(&mut self, area: Rect) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let size = (area.width, area.height);
        if self.terminal_size != Some(size) {
            self.terminal_size = Some(size);
            self.terminal_parser.set_size(area.height, area.width);
            self.pending_shell_resize = Some(size);
        }
    }

    async fn apply_pending_resize(&mut self) -> Result<()> {
        let Some((width, height)) = self.pending_shell_resize.take() else {
            return Ok(());
        };
        if let Some(shell) = self.shell.as_mut() {
            let _ = shell.resize(width.into(), height.into()).await;
        }
        Ok(())
    }

    fn format_transfer_status(&self, progress: &TransferProgress) -> String {
        let label = self.i18n.tr("status-transfer");
        let files = if progress.files_total > 0 {
            format!("{}/{}", progress.files_completed, progress.files_total)
        } else {
            "0/0".to_string()
        };
        let bytes = if progress.bytes_total > 0 {
            format!(
                "{}/{}",
                format_bytes(progress.bytes_transferred),
                format_bytes(progress.bytes_total)
            )
        } else {
            format_bytes(progress.bytes_transferred)
        };
        let percent = if progress.bytes_total > 0 {
            let pct = (progress.bytes_transferred as f64 / progress.bytes_total as f64) * 100.0;
            format!("{:.0}%", pct.min(100.0))
        } else {
            "0%".to_string()
        };
        let mut parts = vec![label, files, bytes, percent];
        if progress.speed_bps > 0 {
            parts.push(format!("{}/s", format_bytes(progress.speed_bps)));
        }
        if let Some(eta) = progress.eta_seconds {
            parts.push(format!("ETA {}", format_duration(eta)));
        }
        parts.join(" ")
    }

    fn sftp_status_label(&self) -> String {
        if self.show_file_manager {
            self.i18n.tr("status-sftp-on")
        } else {
            self.i18n.tr("status-sftp-off")
        }
    }

    fn ai_status_label(&self) -> String {
        if !self.config.ai.enabled {
            return self.i18n.tr("status-ai-disabled");
        }
        if !self.show_ai_panel {
            return self.i18n.tr("status-ai-off");
        }
        if self.assistant.busy || self.tool_busy {
            return self.i18n.tr("status-ai-busy");
        }
        let mut parts = vec![self.i18n.tr("status-ai-on")];
        if self.config.ai.tools_enabled {
            parts.push(self.i18n.tr("status-ai-tools"));
        }
        if self.config.ai.agent_enabled {
            parts.push(self.i18n.tr("status-ai-agent"));
        }
        if self.config.ai.auto_mode {
            parts.push(self.i18n.tr("status-ai-auto"));
        }
        if self.config.ai.streaming {
            parts.push(self.i18n.tr("status-ai-stream"));
        }
        parts.join(" ")
    }

    fn ai_settings_label(&self, field: AiSettingsField) -> String {
        match field {
            AiSettingsField::Enabled => self.i18n.tr("ai-settings-enabled"),
            AiSettingsField::Provider => self.i18n.tr("ai-settings-provider"),
            AiSettingsField::Endpoint => self.i18n.tr("ai-settings-endpoint"),
            AiSettingsField::ApiKey => self.i18n.tr("ai-settings-api-key"),
            AiSettingsField::Model => self.i18n.tr("ai-settings-model"),
            AiSettingsField::Temperature => self.i18n.tr("ai-settings-temperature"),
            AiSettingsField::MaxTokens => self.i18n.tr("ai-settings-max-tokens"),
            AiSettingsField::History => self.i18n.tr("ai-settings-history"),
            AiSettingsField::Timeout => self.i18n.tr("ai-settings-timeout"),
            AiSettingsField::Streaming => self.i18n.tr("ai-settings-streaming"),
            AiSettingsField::Agent => self.i18n.tr("ai-settings-agent"),
            AiSettingsField::AutoMode => self.i18n.tr("ai-settings-auto"),
            AiSettingsField::MaxSteps => self.i18n.tr("ai-settings-max-steps"),
            AiSettingsField::Tools => self.i18n.tr("ai-settings-tools"),
            AiSettingsField::SystemPrompt => self.i18n.tr("ai-settings-system"),
        }
    }

    fn ai_settings_value(&self, state: &AiSettingsState, field: AiSettingsField) -> String {
        match field {
            AiSettingsField::Enabled => self.bool_label(state.draft.enabled),
            AiSettingsField::Provider => self.provider_label(&state.draft.provider),
            AiSettingsField::Endpoint => state.draft.endpoint.clone(),
            AiSettingsField::ApiKey => self.mask_value(&state.draft.api_key),
            AiSettingsField::Model => state.draft.model.clone(),
            AiSettingsField::Temperature => state.draft.temperature.clone(),
            AiSettingsField::MaxTokens => state.draft.max_tokens.clone(),
            AiSettingsField::History => state.draft.history_max.clone(),
            AiSettingsField::Timeout => state.draft.timeout_ms.clone(),
            AiSettingsField::Streaming => self.bool_label(state.draft.streaming),
            AiSettingsField::Agent => self.bool_label(state.draft.agent_enabled),
            AiSettingsField::AutoMode => self.bool_label(state.draft.auto_mode),
            AiSettingsField::MaxSteps => state.draft.max_steps.clone(),
            AiSettingsField::Tools => self.bool_label(state.draft.tools_enabled),
            AiSettingsField::SystemPrompt => self.truncate_text(&state.draft.system_prompt, 80),
        }
    }

    fn bool_label(&self, value: bool) -> String {
        if value {
            self.i18n.tr("value-on")
        } else {
            self.i18n.tr("value-off")
        }
    }

    fn provider_label(&self, value: &str) -> String {
        match value.trim().to_lowercase().as_str() {
            "ollama" => self.i18n.tr("ai-settings-provider-ollama"),
            "openai" | "openai-compatible" => self.i18n.tr("ai-settings-provider-openai"),
            "openrouter" => self.i18n.tr("ai-settings-provider-openrouter"),
            "anthropic" => self.i18n.tr("ai-settings-provider-anthropic"),
            _ => value.to_string(),
        }
    }

    fn mask_value(&self, value: &str) -> String {
        if value.trim().is_empty() {
            "".to_string()
        } else {
            "*".repeat(value.chars().count().min(24))
        }
    }

    fn truncate_text(&self, value: &str, max: usize) -> String {
        let count = value.chars().count();
        if count <= max {
            return value.to_string();
        }
        let trimmed: String = value.chars().take(max.saturating_sub(3)).collect();
        format!("{trimmed}...")
    }

    fn ai_config_error(&self) -> Option<String> {
        if !self.config.ai.enabled {
            return None;
        }
        let provider = self.config.ai.provider.trim().to_lowercase();
        if provider != "ollama"
            && provider != "openai"
            && provider != "openai-compatible"
            && provider != "openrouter"
            && provider != "anthropic"
        {
            return Some(self.i18n.tr("ai-config-provider"));
        }
        if self.config.ai.endpoint.trim().is_empty() {
            return Some(self.i18n.tr("ai-config-endpoint"));
        }
        if self.config.ai.model.trim().is_empty() {
            return Some(self.i18n.tr("ai-config-model"));
        }
        if (provider == "openai"
            || provider == "openai-compatible"
            || provider == "openrouter"
            || provider == "anthropic")
            && self
                .config
                .ai
                .api_key
                .as_ref()
                .map(|v| v.trim().is_empty())
                .unwrap_or(true)
        {
            return Some(self.i18n.tr("ai-config-token"));
        }
        None
    }

    fn apply_ai_settings(&mut self, state: &mut AiSettingsState) -> Result<()> {
        let temperature: f32 = state
            .draft
            .temperature
            .trim()
            .parse()
            .map_err(|_| anyhow::anyhow!(self.i18n.tr("ai-settings-error-number")))?;
        let max_tokens: u32 = state
            .draft
            .max_tokens
            .trim()
            .parse()
            .map_err(|_| anyhow::anyhow!(self.i18n.tr("ai-settings-error-number")))?;
        let history_max: usize = state
            .draft
            .history_max
            .trim()
            .parse()
            .map_err(|_| anyhow::anyhow!(self.i18n.tr("ai-settings-error-number")))?;
        let timeout_ms: u64 = state
            .draft
            .timeout_ms
            .trim()
            .parse()
            .map_err(|_| anyhow::anyhow!(self.i18n.tr("ai-settings-error-number")))?;
        let max_steps: u32 = state
            .draft
            .max_steps
            .trim()
            .parse()
            .map_err(|_| anyhow::anyhow!(self.i18n.tr("ai-settings-error-number")))?;
        self.config.ai.enabled = state.draft.enabled;
        self.config.ai.provider = state.draft.provider.trim().to_string();
        self.config.ai.endpoint = state.draft.endpoint.trim().to_string();
        self.config.ai.api_key = if state.draft.api_key.trim().is_empty() {
            None
        } else {
            Some(state.draft.api_key.clone())
        };
        self.config.ai.model = state.draft.model.trim().to_string();
        self.config.ai.temperature = temperature;
        self.config.ai.max_tokens = max_tokens;
        self.config.ai.history_max = history_max;
        self.config.ai.timeout_ms = timeout_ms;
        self.config.ai.system_prompt = state.draft.system_prompt.clone();
        self.config.ai.streaming = state.draft.streaming;
        self.config.ai.agent_enabled = state.draft.agent_enabled;
        self.config.ai.auto_mode = state.draft.auto_mode;
        self.config.ai.max_steps = max_steps;
        self.config.ai.tools_enabled = state.draft.tools_enabled;
        if !self.config.ai.tools_enabled {
            self.pending_tools.clear();
            self.tool_busy = false;
        }
        self.rebuild_ai_client()?;
        self.config_manager.save_config(&self.config)?;
        Ok(())
    }

    fn rebuild_ai_client(&mut self) -> Result<()> {
        let mut builder = reqwest::Client::builder();
        if self.config.ai.timeout_ms > 0 {
            builder = builder.timeout(Duration::from_millis(self.config.ai.timeout_ms));
        }
        self.ai_client = builder.build()?;
        Ok(())
    }

    async fn handle_bus_event(&mut self, event: CoreEvent) -> Result<()> {
        if let CoreEvent::TransferProgress { job_id, progress } = event {
            self.transfer_status = Some(TransferStatus {
                progress: progress.clone(),
            });
            if progress.files_total > 0
                && progress.files_completed >= progress.files_total
                && self.completed_transfers.insert(job_id)
                && matches!(self.mode, AppMode::Session { .. })
            {
                self.refresh_panels().await?;
            }
        }
        Ok(())
    }

    fn terminal_text(&self, area: Rect) -> Text<'static> {
        let screen = self.terminal_parser.screen();
        let (rows, cols) = screen.size();
        let rows = rows.min(area.height);
        let cols = cols.min(area.width);
        let mut lines = Vec::with_capacity(rows as usize);
        for row in 0..rows {
            let mut spans = Vec::new();
            let mut current_style = Style::default();
            let mut current_text = String::new();
            let mut col = 0;
            while col < cols {
                if let Some(cell) = screen.cell(row, col) {
                    if cell.is_wide_continuation() {
                        col += 1;
                        continue;
                    }
                    let mut text = cell.contents();
                    if text.is_empty() {
                        text.push(' ');
                    }
                    let style = style_for_cell(cell);
                    if current_text.is_empty() {
                        current_style = style;
                        current_text.push_str(&text);
                    } else if style == current_style {
                        current_text.push_str(&text);
                    } else {
                        spans.push(Span::styled(current_text, current_style));
                        current_text = text;
                        current_style = style;
                    }
                    if cell.is_wide() {
                        col += 2;
                    } else {
                        col += 1;
                    }
                } else {
                    let style = Style::default();
                    if current_text.is_empty() {
                        current_style = style;
                        current_text.push(' ');
                    } else if current_style == style {
                        current_text.push(' ');
                    } else {
                        spans.push(Span::styled(current_text, current_style));
                        current_text = " ".to_string();
                        current_style = style;
                    }
                    col += 1;
                }
            }
            if !current_text.is_empty() {
                spans.push(Span::styled(current_text, current_style));
            }
            lines.push(Line::from(spans));
        }
        Text::from(lines)
    }

    fn reload_connections(&mut self) {
        self.connections = self.store.list_connections().unwrap_or_default();
        if self.connections.is_empty() {
            self.selected = 0;
        } else if self.selected >= self.connections.len() {
            self.selected = self.connections.len() - 1;
        }
    }

    async fn start_connection(&mut self, conn: Connection) -> Result<()> {
        if matches!(conn.auth_method, AuthMethod::Agent) && !agent_available() {
            self.set_status(self.i18n.tr("status-agent-missing"));
            self.open_password_overlay(conn.id, PasswordMode::Connect);
            return Ok(());
        }
        match self.sessions.connect(conn.clone(), None, None).await {
            Ok(session_id) => {
                self.enter_session(session_id, conn).await?;
            }
            Err(err) => {
                let msg = err.to_string();
                if should_prompt_password(&msg) {
                    self.open_password_overlay(conn.id, PasswordMode::Connect);
                } else {
                    let mut args = FluentArgs::new();
                    args.set("error", msg);
                    self.set_status(self.i18n.tr_args("status-connection-failed", &args));
                }
            }
        }
        Ok(())
    }

    fn open_password_overlay(&mut self, id: Uuid, mode: PasswordMode) {
        self.overlay = Overlay::Password {
            id,
            state: PasswordOverlayState {
                password: String::new(),
                master: String::new(),
                save: true,
                focus: PasswordFocus::Password,
                mode,
                error: None,
            },
        };
    }

    fn open_edit_overlay(&mut self, id: Uuid) {
        if let Some(conn) = self.connections.iter().find(|c| c.id == id) {
            let input = format!(
                "{}|{}@{}:{}",
                conn.name, conn.username, conn.host, conn.port
            );
            self.overlay = Overlay::Edit {
                id,
                input,
                error: None,
            };
        }
    }

    fn open_ai_settings(&mut self) {
        let state = AiSettingsState::from_config(&self.config.ai);
        self.overlay = Overlay::AiSettings {
            state: Box::new(state),
        };
    }

    fn save_edit_connection(&mut self, id: Uuid, input: &str) -> Result<(), String> {
        let default_user = whoami::username();
        let default_port = self.config.ssh.port;
        let (name, username, host, port) =
            parse_named_target(input, &default_user, default_port)
                .map_err(|_| self.i18n.tr("prompt-edit-connection-error"))?;
        let mut conn = self
            .store
            .get_connection(id)
            .map_err(|_| self.i18n.tr("status-connection-error"))?;
        conn.name = name.unwrap_or_else(|| host.clone());
        conn.username = username;
        conn.host = host;
        conn.port = port;
        conn.updated_at = chrono::Utc::now();
        self.store
            .update_connection(&conn)
            .map_err(|_| self.i18n.tr("status-connection-error"))?;
        self.reload_connections();
        let mut args = FluentArgs::new();
        args.set("name", conn.name.clone());
        self.set_status(self.i18n.tr_args("status-connection-updated", &args));
        Ok(())
    }

    fn save_password_for_connection(
        &mut self,
        id: Uuid,
        password: Zeroizing<String>,
        master: Option<&str>,
    ) -> Result<(), String> {
        let mut conn = self
            .store
            .get_connection(id)
            .map_err(|_| self.i18n.tr("status-connection-error"))?;
        self.sessions
            .set_connection_password(&mut conn, &password, master)
            .map_err(|e| e.to_string())?;
        self.reload_connections();
        self.set_status(self.i18n.tr("status-password-saved"));
        Ok(())
    }

    async fn connect_with_password(
        &mut self,
        id: Uuid,
        password: Zeroizing<String>,
        save: bool,
        master: Option<&str>,
    ) -> Result<(), String> {
        let conn = self
            .store
            .get_connection(id)
            .map_err(|_| self.i18n.tr("status-connection-error"))?;
        let session_id = self
            .sessions
            .connect_with_password(conn.clone(), password, save, master)
            .await
            .map_err(|e| e.to_string())?;
        self.enter_session(session_id, conn)
            .await
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    fn auto_import_if_empty(&mut self) -> Result<()> {
        if !self.connections.is_empty() {
            return Ok(());
        }
        let paths = default_ssh_config_paths();
        match self.import_from_paths(&paths) {
            Ok((imported, path)) => {
                if imported > 0 {
                    let mut args = FluentArgs::new();
                    args.set("count", imported.to_string());
                    args.set(
                        "path",
                        path.map(|p| p.to_string_lossy().to_string())
                            .unwrap_or_default(),
                    );
                    self.set_status(self.i18n.tr_args("status-imported", &args));
                }
            }
            Err(err) => {
                let mut args = FluentArgs::new();
                args.set("error", err.to_string());
                self.set_status(self.i18n.tr_args("status-import-error", &args));
            }
        }
        Ok(())
    }

    fn handle_import(&mut self) -> Result<()> {
        let paths = default_ssh_config_paths();
        match self.import_from_paths(&paths) {
            Ok((imported, path)) => {
                if imported > 0 {
                    let mut args = FluentArgs::new();
                    args.set("count", imported.to_string());
                    args.set(
                        "path",
                        path.map(|p| p.to_string_lossy().to_string())
                            .unwrap_or_default(),
                    );
                    self.set_status(self.i18n.tr_args("status-imported", &args));
                } else if path.is_some() {
                    self.set_status(self.i18n.tr("status-import-none"));
                } else {
                    self.set_status(self.i18n.tr("status-import-missing"));
                }
            }
            Err(err) => {
                let mut args = FluentArgs::new();
                args.set("error", err.to_string());
                self.set_status(self.i18n.tr_args("status-import-error", &args));
            }
        }
        Ok(())
    }

    fn import_from_paths(&mut self, paths: &[PathBuf]) -> Result<(usize, Option<PathBuf>)> {
        let mut total = 0usize;
        let mut found = None;
        for path in paths {
            if path.exists() {
                if found.is_none() {
                    found = Some(path.clone());
                }
                let imported = self
                    .store
                    .import_from_ssh_config(path)
                    .map_err(|e| anyhow::anyhow!(e.to_string()))?;
                total += imported.len();
            }
        }
        self.reload_connections();
        Ok((total, found))
    }

    fn quick_add_connection(&mut self, input: &str) -> Result<(), String> {
        let default_user = whoami::username();
        let default_port = self.config.ssh.port;
        let (name, username, host, port) =
            parse_named_target(input, &default_user, default_port)
                .map_err(|_| self.i18n.tr("prompt-new-connection-error"))?;
        let now = chrono::Utc::now();
        let id = Uuid::new_v4();
        let name = name.unwrap_or_else(|| host.clone());
        let conn = Connection {
            id,
            name: name.clone(),
            host,
            port,
            username,
            auth_method: AuthMethod::Agent,
            jump_hosts: Vec::new(),
            proxy: None,
            startup_commands: Vec::new(),
            env_vars: Vec::new(),
            group_id: None,
            tags: Vec::new(),
            color: None,
            icon: None,
            notes: None,
            created_at: now,
            updated_at: now,
            last_connected_at: None,
            is_favorite: false,
        };
        self.store
            .create_connection(&conn)
            .map_err(|_| self.i18n.tr("status-connection-create-error"))?;
        self.reload_connections();
        if let Some(idx) = self.connections.iter().position(|c| c.id == id) {
            self.selected = idx;
        }
        let mut args = FluentArgs::new();
        args.set("name", name);
        self.set_status(self.i18n.tr_args("status-connection-added", &args));
        Ok(())
    }

    async fn handle_event(&mut self, event: Event) -> Result<bool> {
        match event {
            Event::Key(key) => {
                if matches!(key.kind, KeyEventKind::Release) {
                    return Ok(false);
                }
                self.handle_key(key).await
            }
            Event::Resize(w, h) => {
                let _ = (w, h);
                Ok(false)
            }
            _ => Ok(false),
        }
    }

    async fn handle_key(&mut self, key: KeyEvent) -> Result<bool> {
        if !matches!(self.overlay, Overlay::None) {
            return self.handle_overlay_key(key).await;
        }
        match self.mode {
            AppMode::Connections => self.handle_connections_key(key).await,
            AppMode::Session { .. } => self.handle_session_key(key).await,
        }
    }

    async fn handle_overlay_key(&mut self, key: KeyEvent) -> Result<bool> {
        if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('q')) {
            return Ok(true);
        }
        let overlay = std::mem::replace(&mut self.overlay, Overlay::None);
        match overlay {
            Overlay::Help => {
                if matches!(key.code, KeyCode::Esc | KeyCode::Char('?')) {
                    self.overlay = Overlay::None;
                } else {
                    self.overlay = Overlay::Help;
                }
                Ok(false)
            }
            Overlay::QuickAdd {
                mut input,
                mut error,
            } => {
                let mut close = false;
                match key.code {
                    KeyCode::Esc => {
                        close = true;
                    }
                    KeyCode::Enter => match self.quick_add_connection(&input) {
                        Ok(_) => close = true,
                        Err(err) => error = Some(err.to_string()),
                    },
                    KeyCode::Backspace => {
                        input.pop();
                    }
                    KeyCode::Char(c) => {
                        if !key.modifiers.contains(KeyModifiers::CONTROL) {
                            input.push(c);
                        }
                    }
                    _ => {}
                }
                if close {
                    self.overlay = Overlay::None;
                } else {
                    self.overlay = Overlay::QuickAdd { input, error };
                }
                Ok(false)
            }
            Overlay::Edit {
                id,
                mut input,
                mut error,
            } => {
                let mut close = false;
                match key.code {
                    KeyCode::Esc => {
                        close = true;
                    }
                    KeyCode::Enter => match self.save_edit_connection(id, &input) {
                        Ok(_) => close = true,
                        Err(err) => error = Some(err.to_string()),
                    },
                    KeyCode::Backspace => {
                        input.pop();
                    }
                    KeyCode::Char(c) => {
                        if !key.modifiers.contains(KeyModifiers::CONTROL) {
                            input.push(c);
                        }
                    }
                    _ => {}
                }
                if close {
                    self.overlay = Overlay::None;
                } else {
                    self.overlay = Overlay::Edit { id, input, error };
                }
                Ok(false)
            }
            Overlay::Password { id, mut state } => {
                let mut close = false;
                match key.code {
                    KeyCode::Esc => {
                        close = true;
                    }
                    KeyCode::Tab => {
                        state.focus = match state.focus {
                            PasswordFocus::Password => PasswordFocus::Master,
                            PasswordFocus::Master => PasswordFocus::Password,
                        };
                    }
                    KeyCode::F(2) => {
                        state.save = !state.save;
                    }
                    KeyCode::Enter => {
                        if state.password.is_empty() {
                            state.error = Some(self.i18n.tr("prompt-password-error"));
                        } else {
                            let master_opt = if state.master.is_empty() {
                                None
                            } else {
                                Some(state.master.as_str())
                            };
                            match state.mode {
                                PasswordMode::SaveOnly => {
                                    match self.save_password_for_connection(
                                        id,
                                        Zeroizing::new(state.password.clone()),
                                        master_opt,
                                    ) {
                                        Ok(_) => close = true,
                                        Err(err) => state.error = Some(err.to_string()),
                                    }
                                }
                                PasswordMode::Connect => {
                                    match self
                                        .connect_with_password(
                                            id,
                                            Zeroizing::new(state.password.clone()),
                                            state.save,
                                            master_opt,
                                        )
                                        .await
                                    {
                                        Ok(_) => close = true,
                                        Err(err) => state.error = Some(err.to_string()),
                                    }
                                }
                            }
                        }
                    }
                    KeyCode::Backspace => match state.focus {
                        PasswordFocus::Password => {
                            state.password.pop();
                        }
                        PasswordFocus::Master => {
                            state.master.pop();
                        }
                    },
                    KeyCode::Char(c) => {
                        if !key.modifiers.contains(KeyModifiers::CONTROL) {
                            match state.focus {
                                PasswordFocus::Password => state.password.push(c),
                                PasswordFocus::Master => state.master.push(c),
                            }
                        }
                    }
                    _ => {}
                }
                if close {
                    self.overlay = Overlay::None;
                } else {
                    self.overlay = Overlay::Password { id, state };
                }
                Ok(false)
            }
            Overlay::AiSettings { mut state } => {
                let mut close = false;
                if key.modifiers.contains(KeyModifiers::CONTROL)
                    && matches!(key.code, KeyCode::Char('s'))
                {
                    match self.apply_ai_settings(&mut state) {
                        Ok(_) => {
                            self.set_status(self.i18n.tr("ai-settings-saved"));
                            close = true;
                        }
                        Err(err) => {
                            state.error = Some(err.to_string());
                        }
                    }
                } else if state.editing {
                    match key.code {
                        KeyCode::Esc => {
                            state.editing = false;
                            state.input.clear();
                        }
                        KeyCode::Enter => {
                            let field = state.selected_field();
                            state.commit_edit(field);
                        }
                        KeyCode::Backspace => {
                            state.input.pop();
                        }
                        KeyCode::Char(c) => {
                            if !key.modifiers.contains(KeyModifiers::CONTROL) {
                                state.input.push(c);
                            }
                        }
                        _ => {}
                    }
                } else {
                    match key.code {
                        KeyCode::Esc => {
                            close = true;
                        }
                        KeyCode::Up => {
                            if state.selected == 0 {
                                state.selected = AI_SETTINGS_FIELDS.len().saturating_sub(1);
                            } else {
                                state.selected = state.selected.saturating_sub(1);
                            }
                        }
                        KeyCode::Down => {
                            state.selected = (state.selected + 1) % AI_SETTINGS_FIELDS.len();
                        }
                        KeyCode::Left => {
                            let field = state.selected_field();
                            if field == AiSettingsField::Provider {
                                let next = cycle_provider(&state.draft.provider, -1);
                                let (endpoint, model) = provider_defaults(&next);
                                state.draft.provider = next;
                                state.draft.endpoint = endpoint;
                                state.draft.model = model;
                            } else {
                                state.toggle_field(field);
                            }
                        }
                        KeyCode::Right => {
                            let field = state.selected_field();
                            if field == AiSettingsField::Provider {
                                let next = cycle_provider(&state.draft.provider, 1);
                                let (endpoint, model) = provider_defaults(&next);
                                state.draft.provider = next;
                                state.draft.endpoint = endpoint;
                                state.draft.model = model;
                            } else {
                                state.toggle_field(field);
                            }
                        }
                        KeyCode::Enter => {
                            let field = state.selected_field();
                            if field == AiSettingsField::Provider {
                                let next = cycle_provider(&state.draft.provider, 1);
                                let (endpoint, model) = provider_defaults(&next);
                                state.draft.provider = next;
                                state.draft.endpoint = endpoint;
                                state.draft.model = model;
                            } else if matches!(
                                field,
                                AiSettingsField::Enabled
                                    | AiSettingsField::Streaming
                                    | AiSettingsField::Agent
                                    | AiSettingsField::AutoMode
                                    | AiSettingsField::Tools
                            ) {
                                state.toggle_field(field);
                            } else {
                                state.start_edit(field);
                            }
                        }
                        KeyCode::Char(' ') => {
                            let field = state.selected_field();
                            state.toggle_field(field);
                        }
                        _ => {}
                    }
                }
                if close {
                    self.overlay = Overlay::None;
                } else {
                    self.overlay = Overlay::AiSettings { state };
                }
                Ok(false)
            }
            Overlay::None => Ok(false),
        }
    }

    async fn handle_connections_key(&mut self, key: KeyEvent) -> Result<bool> {
        if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('q')) {
            return Ok(true);
        }
        match key.code {
            KeyCode::Char(c) => match c.to_ascii_lowercase() {
                'q' => Ok(true),
                'i' => {
                    self.handle_import()?;
                    Ok(false)
                }
                'n' => {
                    self.overlay = Overlay::QuickAdd {
                        input: String::new(),
                        error: None,
                    };
                    Ok(false)
                }
                'a' => {
                    self.open_ai_settings();
                    Ok(false)
                }
                'e' => {
                    if let Some(conn) = self.connections.get(self.selected) {
                        self.open_edit_overlay(conn.id);
                    }
                    Ok(false)
                }
                'p' => {
                    if let Some(conn) = self.connections.get(self.selected) {
                        self.open_password_overlay(conn.id, PasswordMode::SaveOnly);
                    }
                    Ok(false)
                }
                'r' => {
                    self.reload_connections();
                    Ok(false)
                }
                '?' => {
                    self.overlay = Overlay::Help;
                    Ok(false)
                }
                _ => Ok(false),
            },
            KeyCode::Down => {
                if self.selected + 1 < self.connections.len() {
                    self.selected += 1;
                }
                Ok(false)
            }
            KeyCode::Up => {
                if self.selected > 0 {
                    self.selected -= 1;
                }
                Ok(false)
            }
            KeyCode::Enter => {
                if let Some(conn) = self.connections.get(self.selected).cloned() {
                    self.start_connection(conn).await?;
                }
                Ok(false)
            }
            KeyCode::F(9) => {
                self.open_ai_settings();
                Ok(false)
            }
            _ => Ok(false),
        }
    }

    async fn handle_session_key(&mut self, key: KeyEvent) -> Result<bool> {
        match key.code {
            KeyCode::F(9) => {
                self.open_ai_settings();
                return Ok(false);
            }
            KeyCode::F(10) => {
                self.toggle_ai_panel();
                return Ok(false);
            }
            KeyCode::F(12) => {
                self.toggle_file_manager();
                return Ok(false);
            }
            _ => {}
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('q') => return Ok(true),
                KeyCode::Char('t') => {
                    self.toggle_focus();
                    return Ok(false);
                }
                _ => {}
            }
        }
        match self.input_focus {
            InputFocus::Files => self.handle_files_key(key).await,
            InputFocus::Terminal => self.handle_terminal_key(key).await,
            InputFocus::Assistant => self.handle_assistant_key(key).await,
        }
    }

    async fn handle_files_key(&mut self, key: KeyEvent) -> Result<bool> {
        match key.code {
            KeyCode::Esc => {
                self.mode = AppMode::Connections;
                self.shell = None;
                self.active_connection = None;
                self.pending_tools.clear();
                self.tool_busy = false;
                self.agent_steps_remaining = 0;
                Ok(false)
            }
            KeyCode::Char('?') => {
                self.overlay = Overlay::Help;
                Ok(false)
            }
            KeyCode::Tab => {
                self.active_panel_left = !self.active_panel_left;
                Ok(false)
            }
            KeyCode::Left => {
                self.active_panel_left = true;
                Ok(false)
            }
            KeyCode::Right => {
                self.active_panel_left = false;
                Ok(false)
            }
            KeyCode::Up => {
                self.move_selection(-1);
                Ok(false)
            }
            KeyCode::Down => {
                self.move_selection(1);
                Ok(false)
            }
            KeyCode::Enter => {
                self.open_selected().await?;
                Ok(false)
            }
            KeyCode::Backspace => {
                self.navigate_up().await?;
                Ok(false)
            }
            KeyCode::F(5) => {
                self.copy_selected().await?;
                Ok(false)
            }
            _ => Ok(false),
        }
    }

    async fn handle_terminal_key(&mut self, key: KeyEvent) -> Result<bool> {
        if let Some(shell) = self.shell.as_mut() {
            if let Some(bytes) = key_to_bytes(key) {
                shell.write(&bytes).await?;
            }
        }
        Ok(false)
    }

    async fn handle_assistant_key(&mut self, key: KeyEvent) -> Result<bool> {
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('y') => {
                    if !self.pending_tools.is_empty() {
                        self.start_next_tool();
                    }
                    return Ok(false);
                }
                KeyCode::Char('n') => {
                    if let Some(skipped) = self.pending_tools.pop_front() {
                        let mut args = FluentArgs::new();
                        args.set("name", skipped.name);
                        self.set_status(self.i18n.tr_args("ai-tool-skipped", &args));
                        if self.config.ai.auto_mode {
                            self.start_next_tool();
                        }
                    }
                    return Ok(false);
                }
                _ => {}
            }
        }
        match key.code {
            KeyCode::Esc => {
                self.exit_assistant_focus();
            }
            KeyCode::Enter => {
                self.submit_assistant_request();
            }
            KeyCode::Backspace => {
                self.assistant.input.pop();
            }
            KeyCode::PageUp => {
                self.assistant.scroll = self.assistant.scroll.saturating_sub(5);
            }
            KeyCode::PageDown => {
                self.assistant.scroll = self.assistant.scroll.saturating_add(5);
            }
            KeyCode::Up => {
                self.assistant.scroll = self.assistant.scroll.saturating_sub(1);
            }
            KeyCode::Down => {
                self.assistant.scroll = self.assistant.scroll.saturating_add(1);
            }
            KeyCode::Char(c) => {
                if !key.modifiers.contains(KeyModifiers::CONTROL) {
                    self.assistant.input.push(c);
                }
            }
            _ => {}
        }
        Ok(false)
    }

    fn toggle_focus(&mut self) {
        let mut order = Vec::new();
        order.push(InputFocus::Terminal);
        if self.show_file_manager {
            order.push(InputFocus::Files);
        }
        if self.show_ai_panel {
            order.push(InputFocus::Assistant);
        }
        let idx = order
            .iter()
            .position(|f| *f == self.input_focus)
            .unwrap_or(0);
        let next = (idx + 1) % order.len();
        self.input_focus = order[next];
    }

    fn exit_assistant_focus(&mut self) {
        if self.show_file_manager {
            self.input_focus = InputFocus::Files;
        } else {
            self.input_focus = InputFocus::Terminal;
        }
    }

    fn ensure_focus_valid(&mut self) {
        if matches!(self.input_focus, InputFocus::Files) && !self.show_file_manager {
            self.input_focus = InputFocus::Terminal;
        }
        if matches!(self.input_focus, InputFocus::Assistant) && !self.show_ai_panel {
            self.input_focus = InputFocus::Terminal;
        }
    }

    fn toggle_file_manager(&mut self) {
        self.show_file_manager = !self.show_file_manager;
        if self.show_file_manager {
            self.input_focus = InputFocus::Files;
        } else if matches!(self.input_focus, InputFocus::Files) {
            self.input_focus = InputFocus::Terminal;
        }
        self.ensure_focus_valid();
    }

    fn toggle_ai_panel(&mut self) {
        self.show_ai_panel = !self.show_ai_panel;
        if self.show_ai_panel {
            self.input_focus = InputFocus::Assistant;
        } else if matches!(self.input_focus, InputFocus::Assistant) {
            self.input_focus = InputFocus::Terminal;
        }
        self.ensure_focus_valid();
    }

    fn move_selection(&mut self, delta: i32) {
        let panel = if self.active_panel_left {
            &mut self.left_panel
        } else {
            &mut self.right_panel
        };
        let len = panel.entries.len();
        if len == 0 {
            panel.selected = 0;
            return;
        }
        let current = panel.selected as i32;
        let mut next = current + delta;
        if next < 0 {
            next = 0;
        }
        let max = len as i32 - 1;
        if next > max {
            next = max;
        }
        panel.selected = next as usize;
    }

    fn submit_assistant_request(&mut self) {
        if !self.config.ai.enabled {
            self.assistant.push_message(
                AssistantMessage {
                    role: AssistantRole::Error,
                    content: self.i18n.tr("ai-disabled"),
                },
                self.config.ai.history_max,
            );
            return;
        }
        if let Some(error) = self.ai_config_error() {
            self.assistant.push_message(
                AssistantMessage {
                    role: AssistantRole::Error,
                    content: error,
                },
                self.config.ai.history_max,
            );
            return;
        }
        if self.assistant.busy {
            return;
        }
        let input = self.assistant.input.trim().to_string();
        if input.is_empty() {
            return;
        }
        self.assistant.input.clear();
        self.assistant.push_message(
            AssistantMessage {
                role: AssistantRole::User,
                content: input.clone(),
            },
            self.config.ai.history_max,
        );
        let messages = self.build_ai_request_messages();
        if self.config.ai.agent_enabled {
            self.agent_steps_remaining = self.config.ai.max_steps;
        } else {
            self.agent_steps_remaining = 0;
        }
        self.start_ai_request(messages);
    }

    fn start_ai_request(&mut self, messages: Vec<ChatMessage>) {
        if self.config.ai.agent_enabled && self.agent_steps_remaining > 0 {
            self.agent_steps_remaining = self.agent_steps_remaining.saturating_sub(1);
        }
        let cfg = self.config.ai.clone();
        let client = self.ai_client.clone();
        let tx = self.assistant_tx.clone();
        self.assistant.busy = true;
        tokio::spawn(async move {
            let _ = tx.send(AssistantEvent::Start).await;
            let result = if cfg.streaming {
                request_ai_stream(client, &cfg, messages, tx.clone()).await
            } else {
                request_ai(client, &cfg, messages).await
            };
            let event = match result {
                Ok(content) => AssistantEvent::Done(content),
                Err(err) => AssistantEvent::Error(err.to_string()),
            };
            let _ = tx.send(event).await;
        });
    }

    fn build_ai_request_messages(&self) -> Vec<ChatMessage> {
        let mut out = Vec::new();
        let system_prompt = self.build_ai_system_prompt();
        if !system_prompt.is_empty() {
            out.push(ChatMessage {
                role: "system".to_string(),
                content: system_prompt,
            });
        }
        let history: Vec<&AssistantMessage> = self
            .assistant
            .messages
            .iter()
            .filter(|m| {
                matches!(
                    m.role,
                    AssistantRole::User | AssistantRole::Assistant | AssistantRole::Tool
                )
            })
            .collect();
        let max = self.config.ai.history_max;
        let start = if max > 0 && history.len() > max {
            history.len() - max
        } else {
            0
        };
        for message in history.into_iter().skip(start) {
            let role = match message.role {
                AssistantRole::User => "user",
                AssistantRole::Assistant => "assistant",
                AssistantRole::Tool => "user",
                _ => "user",
            };
            out.push(ChatMessage {
                role: role.to_string(),
                content: message.content.clone(),
            });
        }
        out
    }

    fn build_ai_system_prompt(&self) -> String {
        let mut prompt = self.config.ai.system_prompt.trim().to_string();
        if let Some(conn) = &self.active_connection {
            prompt.push_str(&format!(
                "\nTarget: {}@{}:{}",
                conn.username, conn.host, conn.port
            ));
        }
        prompt.push_str(&format!("\nLocal path: {}", self.left_panel.path));
        prompt.push_str(&format!("\nRemote path: {}", self.right_panel.path));
        if self.config.ai.tools_enabled {
            for line in tool_definitions() {
                prompt.push('\n');
                prompt.push_str(&line);
            }
        }
        prompt
    }

    fn handle_assistant_event(&mut self, event: AssistantEvent) {
        match event {
            AssistantEvent::Start => {
                self.assistant.busy = true;
                self.assistant.push_message(
                    AssistantMessage {
                        role: AssistantRole::Assistant,
                        content: String::new(),
                    },
                    self.config.ai.history_max,
                );
                self.assistant.stream_index = Some(self.assistant.messages.len().saturating_sub(1));
            }
            AssistantEvent::Delta(chunk) => {
                if let Some(idx) = self.assistant.stream_index {
                    if let Some(message) = self.assistant.messages.get_mut(idx) {
                        message.content.push_str(&chunk);
                    }
                }
                self.assistant.scroll = usize::MAX;
            }
            AssistantEvent::Done(content) => {
                self.assistant.busy = false;
                let (cleaned, calls) = extract_tool_calls(&content);
                if let Some(idx) = self.assistant.stream_index {
                    if let Some(message) = self.assistant.messages.get_mut(idx) {
                        message.content = cleaned.clone();
                    }
                } else {
                    self.assistant.push_message(
                        AssistantMessage {
                            role: AssistantRole::Assistant,
                            content: cleaned.clone(),
                        },
                        self.config.ai.history_max,
                    );
                }
                if self.config.ai.tools_enabled && !calls.is_empty() {
                    self.pending_tools.extend(calls);
                    if let Some(next) = self.pending_tools.front() {
                        let mut args = FluentArgs::new();
                        args.set("name", next.name.clone());
                        self.set_status(self.i18n.tr_args("ai-tool-pending", &args));
                    }
                    if self.config.ai.auto_mode {
                        self.start_next_tool();
                    }
                }
                self.assistant.stream_index = None;
            }
            AssistantEvent::Error(error) => {
                self.assistant.busy = false;
                self.assistant.stream_index = None;
                let mut args = FluentArgs::new();
                args.set("error", error);
                self.assistant.push_message(
                    AssistantMessage {
                        role: AssistantRole::Error,
                        content: self.i18n.tr_args("ai-error", &args),
                    },
                    self.config.ai.history_max,
                );
            }
            AssistantEvent::ToolResult(result) => {
                self.tool_busy = false;
                let status_key = if result.success {
                    "ai-tool-approved"
                } else {
                    "ai-tool-error"
                };
                if result.success {
                    self.set_status(self.i18n.tr(status_key));
                } else {
                    let mut args = FluentArgs::new();
                    args.set("error", result.output.clone());
                    self.set_status(self.i18n.tr_args(status_key, &args));
                }
                let content = format!("Tool result ({})\n{}", result.call.name, result.output);
                self.assistant.push_message(
                    AssistantMessage {
                        role: AssistantRole::Tool,
                        content,
                    },
                    self.config.ai.history_max,
                );
                if !self.pending_tools.is_empty() {
                    if self.config.ai.auto_mode {
                        self.start_next_tool();
                    }
                } else if self.config.ai.agent_enabled && self.agent_steps_remaining > 0 {
                    self.start_agent_followup();
                }
            }
        }
    }

    fn start_agent_followup(&mut self) {
        if !self.config.ai.enabled || self.assistant.busy || self.tool_busy {
            return;
        }
        let messages = self.build_ai_request_messages();
        self.start_ai_request(messages);
    }

    fn start_next_tool(&mut self) {
        if self.tool_busy || !self.config.ai.tools_enabled {
            return;
        }
        let Some(call) = self.pending_tools.pop_front() else {
            return;
        };
        self.tool_busy = true;
        self.set_status(self.i18n.tr("ai-tool-running"));
        let tx = self.assistant_tx.clone();
        let sessions = self.sessions.clone();
        let queue = self.queue.clone();
        let config = self.config.clone();
        let local_base = self.left_panel.path.clone();
        let remote_base = self.right_panel.path.clone();
        let session_id = match self.mode {
            AppMode::Session { id } => Some(id),
            _ => None,
        };
        let connections = self.connections.clone();
        tokio::spawn(async move {
            let ctx = ToolContext {
                sessions,
                queue,
                config,
                local_base,
                remote_base,
                session_id,
                connections,
            };
            let result = execute_tool_call(call, ctx).await;
            let _ = tx.send(AssistantEvent::ToolResult(result)).await;
        });
    }

    async fn enter_session(&mut self, session_id: Uuid, conn: Connection) -> Result<()> {
        self.mode = AppMode::Session { id: session_id };
        self.active_connection = Some(conn);
        self.input_focus = InputFocus::Files;
        self.transfer_status = None;
        self.assistant = AssistantState::new(&self.i18n);
        self.pending_tools.clear();
        self.tool_busy = false;
        self.agent_steps_remaining = 0;
        self.ensure_focus_valid();
        if let Some(handle) = self.sessions.get_session(session_id) {
            let shell = handle
                .session
                .open_shell()
                .await
                .map_err(|e| anyhow::anyhow!(e.to_string()))?;
            self.shell = Some(shell);
            self.refresh_panels().await?;
        }
        Ok(())
    }

    async fn refresh_panels(&mut self) -> Result<()> {
        self.left_panel.refresh(None).await?;
        if let AppMode::Session { id } = self.mode {
            if let Some(handle) = self.sessions.get_session(id) {
                self.right_panel.refresh(Some(&handle.session)).await?;
            }
        }
        Ok(())
    }

    async fn open_selected(&mut self) -> Result<()> {
        let panel = if self.active_panel_left {
            &mut self.left_panel
        } else {
            &mut self.right_panel
        };
        if let Some(entry) = panel.entries.get(panel.selected).cloned() {
            if entry.is_dir {
                panel.path = join_path(&panel.path, &entry.name, panel.kind == PanelKind::Remote);
                panel.selected = 0;
                panel.scroll = 0;
                if let AppMode::Session { id } = self.mode {
                    let session = self.sessions.get_session(id).map(|h| h.session);
                    panel.refresh(session.as_ref()).await?;
                }
            }
        }
        Ok(())
    }

    async fn navigate_up(&mut self) -> Result<()> {
        let panel = if self.active_panel_left {
            &mut self.left_panel
        } else {
            &mut self.right_panel
        };
        if let Some(parent) = parent_path(&panel.path) {
            panel.path = parent;
            panel.selected = 0;
            panel.scroll = 0;
            if let AppMode::Session { id } = self.mode {
                let session = self.sessions.get_session(id).map(|h| h.session);
                panel.refresh(session.as_ref()).await?;
            }
        }
        Ok(())
    }

    async fn copy_selected(&mut self) -> Result<()> {
        let (src, dst) = if self.active_panel_left {
            (&self.left_panel, &self.right_panel)
        } else {
            (&self.right_panel, &self.left_panel)
        };
        let Some(entry) = src.entries.get(src.selected) else {
            return Ok(());
        };

        let (source, dest, session_id) = match (src.kind, dst.kind, self.mode.clone()) {
            (PanelKind::Local, PanelKind::Remote, AppMode::Session { id }) => (
                TransferEndpoint::Local {
                    path: PathBuf::from(&src.path),
                },
                TransferEndpoint::Remote {
                    session_id: id,
                    path: dst.path.clone(),
                },
                Some(id),
            ),
            (PanelKind::Remote, PanelKind::Local, AppMode::Session { id }) => (
                TransferEndpoint::Remote {
                    session_id: id,
                    path: src.path.clone(),
                },
                TransferEndpoint::Local {
                    path: PathBuf::from(&dst.path),
                },
                Some(id),
            ),
            _ => (
                TransferEndpoint::Local {
                    path: PathBuf::new(),
                },
                TransferEndpoint::Local {
                    path: PathBuf::new(),
                },
                None,
            ),
        };

        if session_id.is_none() {
            return Ok(());
        }

        let job = TransferJob {
            id: Uuid::new_v4(),
            source,
            dest,
            files: vec![TransferFile {
                source_path: join_path(&src.path, &entry.name, src.kind == PanelKind::Remote),
                dest_path: join_path(&dst.path, &entry.name, dst.kind == PanelKind::Remote),
                size: entry.size,
                is_dir: entry.is_dir,
            }],
            options: TransferOptions {
                overwrite: catsolle_core::transfer::OverwriteMode::Replace,
                preserve_permissions: true,
                preserve_times: true,
                verify_checksum: self.config.transfer.verify_checksum,
                resume: self.config.transfer.resume,
                buffer_size: self.config.transfer.buffer_size,
            },
            state: TransferState::Queued,
            progress: Default::default(),
            created_at: chrono::Utc::now(),
        };

        self.queue
            .enqueue(job)
            .await
            .map_err(|e| anyhow::anyhow!(e.to_string()))?;
        Ok(())
    }
}

impl AssistantState {
    fn new(i18n: &I18n) -> Self {
        Self {
            input: String::new(),
            messages: vec![AssistantMessage {
                role: AssistantRole::System,
                content: i18n.tr("ai-hint"),
            }],
            scroll: 0,
            busy: false,
            stream_index: None,
        }
    }

    fn push_message(&mut self, message: AssistantMessage, max: usize) {
        self.messages.push(message);
        if max > 0 && self.messages.len() > max {
            let excess = self.messages.len() - max;
            self.messages.drain(0..excess);
        }
        self.scroll = usize::MAX;
    }
}

impl AiSettingsState {
    fn from_config(cfg: &AiConfig) -> Self {
        Self {
            selected: 0,
            editing: false,
            input: String::new(),
            error: None,
            draft: AiConfigDraft {
                enabled: cfg.enabled,
                provider: cfg.provider.clone(),
                endpoint: cfg.endpoint.clone(),
                api_key: cfg.api_key.clone().unwrap_or_default(),
                model: cfg.model.clone(),
                temperature: cfg.temperature.to_string(),
                max_tokens: cfg.max_tokens.to_string(),
                history_max: cfg.history_max.to_string(),
                timeout_ms: cfg.timeout_ms.to_string(),
                streaming: cfg.streaming,
                agent_enabled: cfg.agent_enabled,
                auto_mode: cfg.auto_mode,
                max_steps: cfg.max_steps.to_string(),
                tools_enabled: cfg.tools_enabled,
                system_prompt: cfg.system_prompt.clone(),
            },
        }
    }

    fn selected_field(&self) -> AiSettingsField {
        AI_SETTINGS_FIELDS
            .get(self.selected)
            .copied()
            .unwrap_or(AiSettingsField::Enabled)
    }

    fn start_edit(&mut self, field: AiSettingsField) {
        self.editing = true;
        self.error = None;
        self.input = match field {
            AiSettingsField::Endpoint => self.draft.endpoint.clone(),
            AiSettingsField::ApiKey => self.draft.api_key.clone(),
            AiSettingsField::Model => self.draft.model.clone(),
            AiSettingsField::Temperature => self.draft.temperature.clone(),
            AiSettingsField::MaxTokens => self.draft.max_tokens.clone(),
            AiSettingsField::History => self.draft.history_max.clone(),
            AiSettingsField::Timeout => self.draft.timeout_ms.clone(),
            AiSettingsField::MaxSteps => self.draft.max_steps.clone(),
            AiSettingsField::SystemPrompt => self.draft.system_prompt.clone(),
            _ => String::new(),
        };
    }

    fn commit_edit(&mut self, field: AiSettingsField) {
        let value = self.input.clone();
        match field {
            AiSettingsField::Endpoint => self.draft.endpoint = value,
            AiSettingsField::ApiKey => self.draft.api_key = value,
            AiSettingsField::Model => self.draft.model = value,
            AiSettingsField::Temperature => self.draft.temperature = value,
            AiSettingsField::MaxTokens => self.draft.max_tokens = value,
            AiSettingsField::History => self.draft.history_max = value,
            AiSettingsField::Timeout => self.draft.timeout_ms = value,
            AiSettingsField::MaxSteps => self.draft.max_steps = value,
            AiSettingsField::SystemPrompt => self.draft.system_prompt = value,
            _ => {}
        }
        self.editing = false;
        self.input.clear();
    }

    fn toggle_field(&mut self, field: AiSettingsField) {
        match field {
            AiSettingsField::Enabled => self.draft.enabled = !self.draft.enabled,
            AiSettingsField::Streaming => self.draft.streaming = !self.draft.streaming,
            AiSettingsField::Agent => self.draft.agent_enabled = !self.draft.agent_enabled,
            AiSettingsField::AutoMode => self.draft.auto_mode = !self.draft.auto_mode,
            AiSettingsField::Tools => self.draft.tools_enabled = !self.draft.tools_enabled,
            _ => {}
        }
    }
}

impl PanelState {
    fn local_default() -> Self {
        let path = std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .to_string_lossy()
            .to_string();
        Self {
            kind: PanelKind::Local,
            path,
            entries: Vec::new(),
            selected: 0,
            scroll: 0,
        }
    }

    fn remote_default() -> Self {
        Self {
            kind: PanelKind::Remote,
            path: "/".to_string(),
            entries: Vec::new(),
            selected: 0,
            scroll: 0,
        }
    }

    async fn refresh(&mut self, session: Option<&catsolle_ssh::SshSession>) -> Result<()> {
        self.entries = match self.kind {
            PanelKind::Local => list_local(&self.path).await?,
            PanelKind::Remote => {
                let session = match session {
                    Some(session) => session,
                    None => {
                        self.entries = Vec::new();
                        return Ok(());
                    }
                };
                let sftp = session
                    .open_sftp()
                    .await
                    .map_err(|e| anyhow::anyhow!(e.to_string()))?;
                list_remote(&sftp, &self.path).await?
            }
        };
        if self.entries.is_empty() {
            self.selected = 0;
            self.scroll = 0;
        } else if self.selected >= self.entries.len() {
            self.selected = self.entries.len() - 1;
        }
        Ok(())
    }

    fn ensure_visible(&mut self, visible: usize) {
        if visible == 0 || self.entries.is_empty() {
            self.scroll = 0;
            return;
        }
        if self.selected < self.scroll {
            self.scroll = self.selected;
            return;
        }
        let end = self.scroll.saturating_add(visible);
        if self.selected >= end {
            self.scroll = self.selected.saturating_sub(visible.saturating_sub(1));
        }
    }
}

impl PanelKind {
    fn label(&self) -> &str {
        match self {
            PanelKind::Local => "Local",
            PanelKind::Remote => "Remote",
        }
    }
}

async fn list_local(path: &str) -> Result<Vec<FileEntry>> {
    let mut out = Vec::new();
    let mut entries = tokio::fs::read_dir(path).await?;
    while let Some(entry) = entries.next_entry().await? {
        let meta = entry.metadata().await?;
        out.push(FileEntry {
            name: entry.file_name().to_string_lossy().to_string(),
            is_dir: meta.is_dir(),
            size: meta.len(),
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

async fn list_remote(sftp: &catsolle_ssh::SftpClient, path: &str) -> Result<Vec<FileEntry>> {
    let mut out = Vec::new();
    for entry in sftp.read_dir(path).await? {
        out.push(FileEntry {
            name: entry.name,
            is_dir: entry.is_dir,
            size: entry.size,
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

fn join_path(base: &str, name: &str, remote: bool) -> String {
    if remote {
        if base.ends_with('/') {
            format!("{}{}", base, name)
        } else {
            format!("{}/{}", base, name)
        }
    } else {
        let mut p = PathBuf::from(base);
        p.push(name);
        p.to_string_lossy().to_string()
    }
}

fn parent_path(path: &str) -> Option<String> {
    let p = PathBuf::from(path);
    p.parent().map(|p| p.to_string_lossy().to_string())
}

fn key_to_bytes(key: KeyEvent) -> Option<Vec<u8>> {
    match key.code {
        KeyCode::Char(c) => {
            if key.modifiers.contains(KeyModifiers::CONTROL) {
                Some(vec![(c as u8) - 0x60])
            } else {
                Some(vec![c as u8])
            }
        }
        KeyCode::Enter => Some(vec![b'\n']),
        KeyCode::Backspace => Some(vec![0x7f]),
        KeyCode::Tab => Some(vec![b'\t']),
        KeyCode::Up => Some(b"\x1b[A".to_vec()),
        KeyCode::Down => Some(b"\x1b[B".to_vec()),
        KeyCode::Left => Some(b"\x1b[D".to_vec()),
        KeyCode::Right => Some(b"\x1b[C".to_vec()),
        _ => None,
    }
}

fn style_for_cell(cell: &vt100::Cell) -> Style {
    let mut style = Style::default();
    if let Some(color) = map_vt_color(cell.fgcolor()) {
        style = style.fg(color);
    }
    if let Some(color) = map_vt_color(cell.bgcolor()) {
        style = style.bg(color);
    }
    if cell.bold() {
        style = style.add_modifier(Modifier::BOLD);
    }
    if cell.italic() {
        style = style.add_modifier(Modifier::ITALIC);
    }
    if cell.underline() {
        style = style.add_modifier(Modifier::UNDERLINED);
    }
    if cell.inverse() {
        style = style.add_modifier(Modifier::REVERSED);
    }
    style
}

fn map_vt_color(color: vt100::Color) -> Option<Color> {
    match color {
        vt100::Color::Default => None,
        vt100::Color::Idx(idx) => Some(Color::Indexed(idx)),
        vt100::Color::Rgb(r, g, b) => Some(Color::Rgb(r, g, b)),
    }
}

#[derive(Clone, Debug, Serialize)]
struct ChatMessage {
    role: String,
    content: String,
}

#[derive(Serialize)]
struct OpenAiRequest {
    model: String,
    messages: Vec<ChatMessage>,
    temperature: f32,
    max_tokens: u32,
    stream: bool,
}

#[derive(Deserialize)]
struct OpenAiResponse {
    choices: Vec<OpenAiChoice>,
}

#[derive(Deserialize)]
struct OpenAiChoice {
    message: OpenAiMessage,
}

#[derive(Deserialize)]
struct OpenAiMessage {
    content: String,
}

#[derive(Deserialize)]
struct OpenAiStreamResponse {
    choices: Vec<OpenAiStreamChoice>,
}

#[derive(Deserialize)]
struct OpenAiStreamChoice {
    delta: OpenAiStreamDelta,
}

#[derive(Deserialize)]
struct OpenAiStreamDelta {
    content: Option<String>,
}

#[derive(Serialize)]
struct OllamaRequest {
    model: String,
    messages: Vec<ChatMessage>,
    stream: bool,
    options: OllamaOptions,
}

#[derive(Serialize)]
struct OllamaOptions {
    temperature: f32,
    num_predict: u32,
}

#[derive(Deserialize)]
struct OllamaResponse {
    message: Option<OllamaMessage>,
}

#[derive(Deserialize)]
struct OllamaMessage {
    content: String,
}

#[derive(Deserialize)]
struct OllamaStreamChunk {
    message: Option<OllamaMessage>,
    done: Option<bool>,
}

#[derive(Serialize)]
struct AnthropicRequest {
    model: String,
    max_tokens: u32,
    temperature: f32,
    messages: Vec<ChatMessage>,
    system: Option<String>,
    stream: bool,
}

#[derive(Deserialize)]
struct AnthropicResponse {
    content: Vec<AnthropicContent>,
}

#[derive(Deserialize)]
struct AnthropicContent {
    text: String,
}

#[derive(Deserialize)]
struct AnthropicStreamChunk {
    #[serde(rename = "type")]
    kind: String,
    delta: Option<AnthropicDelta>,
    content_block: Option<AnthropicContentBlock>,
    error: Option<AnthropicError>,
}

#[derive(Deserialize)]
struct AnthropicDelta {
    text: Option<String>,
}

#[derive(Deserialize)]
struct AnthropicContentBlock {
    text: Option<String>,
}

#[derive(Deserialize)]
struct AnthropicError {
    message: String,
}

async fn request_ai_stream(
    client: reqwest::Client,
    cfg: &AiConfig,
    messages: Vec<ChatMessage>,
    tx: mpsc::Sender<AssistantEvent>,
) -> Result<String> {
    let provider = cfg.provider.trim().to_lowercase();
    match provider.as_str() {
        "ollama" => request_ollama_stream(client, cfg, messages, tx).await,
        "openai" | "openai-compatible" => request_openai_stream(client, cfg, messages, tx).await,
        "openrouter" => request_openai_stream(client, cfg, messages, tx).await,
        "anthropic" => request_anthropic_stream(client, cfg, messages, tx).await,
        _ => Err(anyhow::anyhow!("unknown ai provider: {}", cfg.provider)),
    }
}

async fn request_ai(
    client: reqwest::Client,
    cfg: &AiConfig,
    messages: Vec<ChatMessage>,
) -> Result<String> {
    let provider = cfg.provider.trim().to_lowercase();
    match provider.as_str() {
        "ollama" => request_ollama(client, cfg, messages).await,
        "openai" | "openai-compatible" => request_openai(client, cfg, messages).await,
        "openrouter" => request_openai(client, cfg, messages).await,
        "anthropic" => request_anthropic(client, cfg, messages).await,
        _ => Err(anyhow::anyhow!("unknown ai provider: {}", cfg.provider)),
    }
}

async fn request_ollama(
    client: reqwest::Client,
    cfg: &AiConfig,
    messages: Vec<ChatMessage>,
) -> Result<String> {
    let url = format!("{}/api/chat", cfg.endpoint.trim_end_matches('/'));
    let body = OllamaRequest {
        model: cfg.model.clone(),
        messages,
        stream: false,
        options: OllamaOptions {
            temperature: cfg.temperature,
            num_predict: cfg.max_tokens,
        },
    };
    let resp = client.post(url).json(&body).send().await?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(anyhow::anyhow!("ollama error {status}: {text}"));
    }
    let data: OllamaResponse = resp.json().await?;
    let content = data
        .message
        .map(|m| m.content)
        .ok_or_else(|| anyhow::anyhow!("ollama empty response"))?;
    Ok(content)
}

async fn request_ollama_stream(
    client: reqwest::Client,
    cfg: &AiConfig,
    messages: Vec<ChatMessage>,
    tx: mpsc::Sender<AssistantEvent>,
) -> Result<String> {
    let url = format!("{}/api/chat", cfg.endpoint.trim_end_matches('/'));
    let body = OllamaRequest {
        model: cfg.model.clone(),
        messages,
        stream: true,
        options: OllamaOptions {
            temperature: cfg.temperature,
            num_predict: cfg.max_tokens,
        },
    };
    let resp = client.post(url).json(&body).send().await?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(anyhow::anyhow!("ollama error {status}: {text}"));
    }
    let mut out = String::new();
    let stream = resp.bytes_stream().map_err(io::Error::other);
    let reader = tokio_util::io::StreamReader::new(stream);
    let mut lines = tokio::io::BufReader::new(reader).lines();
    while let Some(line) = lines.next_line().await? {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let chunk: OllamaStreamChunk = serde_json::from_str(line)?;
        if let Some(message) = chunk.message {
            if !message.content.is_empty() {
                out.push_str(&message.content);
                let _ = tx.send(AssistantEvent::Delta(message.content)).await;
            }
        }
        if chunk.done.unwrap_or(false) {
            break;
        }
    }
    Ok(out)
}

async fn request_openai(
    client: reqwest::Client,
    cfg: &AiConfig,
    messages: Vec<ChatMessage>,
) -> Result<String> {
    let url = format!("{}/v1/chat/completions", cfg.endpoint.trim_end_matches('/'));
    let body = OpenAiRequest {
        model: cfg.model.clone(),
        messages,
        temperature: cfg.temperature,
        max_tokens: cfg.max_tokens,
        stream: false,
    };
    let mut req = client.post(url).json(&body);
    if let Some(key) = cfg.api_key.as_ref().filter(|v| !v.trim().is_empty()) {
        req = req.bearer_auth(key);
    }
    let resp = req.send().await?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(anyhow::anyhow!("ai error {status}: {text}"));
    }
    let data: OpenAiResponse = resp.json().await?;
    let content = data
        .choices
        .first()
        .map(|c| c.message.content.clone())
        .ok_or_else(|| anyhow::anyhow!("ai empty response"))?;
    Ok(content)
}

async fn request_openai_stream(
    client: reqwest::Client,
    cfg: &AiConfig,
    messages: Vec<ChatMessage>,
    tx: mpsc::Sender<AssistantEvent>,
) -> Result<String> {
    let url = format!("{}/v1/chat/completions", cfg.endpoint.trim_end_matches('/'));
    let body = OpenAiRequest {
        model: cfg.model.clone(),
        messages,
        temperature: cfg.temperature,
        max_tokens: cfg.max_tokens,
        stream: true,
    };
    let mut req = client.post(url).json(&body);
    if let Some(key) = cfg.api_key.as_ref().filter(|v| !v.trim().is_empty()) {
        req = req.bearer_auth(key);
    }
    let resp = req.send().await?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(anyhow::anyhow!("ai error {status}: {text}"));
    }
    let mut out = String::new();
    let stream = resp.bytes_stream().map_err(io::Error::other);
    let reader = tokio_util::io::StreamReader::new(stream);
    let mut lines = tokio::io::BufReader::new(reader).lines();
    while let Some(line) = lines.next_line().await? {
        let line = line.trim();
        if line.is_empty() || !line.starts_with("data:") {
            continue;
        }
        let data = line.trim_start_matches("data:").trim();
        if data == "[DONE]" {
            break;
        }
        let chunk: OpenAiStreamResponse = serde_json::from_str(data)?;
        if let Some(delta) = chunk.choices.first().and_then(|c| c.delta.content.clone()) {
            if !delta.is_empty() {
                out.push_str(&delta);
                let _ = tx.send(AssistantEvent::Delta(delta)).await;
            }
        }
    }
    Ok(out)
}

async fn request_anthropic(
    client: reqwest::Client,
    cfg: &AiConfig,
    messages: Vec<ChatMessage>,
) -> Result<String> {
    let url = format!("{}/v1/messages", cfg.endpoint.trim_end_matches('/'));
    let (system, messages) = split_system_prompt(messages);
    let body = AnthropicRequest {
        model: cfg.model.clone(),
        max_tokens: cfg.max_tokens,
        temperature: cfg.temperature,
        messages,
        system,
        stream: false,
    };
    let mut req = client.post(url).json(&body);
    if let Some(key) = cfg.api_key.as_ref().filter(|v| !v.trim().is_empty()) {
        req = req.header("x-api-key", key);
    }
    req = req.header("anthropic-version", "2023-06-01");
    let resp = req.send().await?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(anyhow::anyhow!("ai error {status}: {text}"));
    }
    let data: AnthropicResponse = resp.json().await?;
    let content = data
        .content
        .first()
        .map(|c| c.text.clone())
        .ok_or_else(|| anyhow::anyhow!("ai empty response"))?;
    Ok(content)
}

async fn request_anthropic_stream(
    client: reqwest::Client,
    cfg: &AiConfig,
    messages: Vec<ChatMessage>,
    tx: mpsc::Sender<AssistantEvent>,
) -> Result<String> {
    let url = format!("{}/v1/messages", cfg.endpoint.trim_end_matches('/'));
    let (system, messages) = split_system_prompt(messages);
    let body = AnthropicRequest {
        model: cfg.model.clone(),
        max_tokens: cfg.max_tokens,
        temperature: cfg.temperature,
        messages,
        system,
        stream: true,
    };
    let mut req = client.post(url).json(&body);
    if let Some(key) = cfg.api_key.as_ref().filter(|v| !v.trim().is_empty()) {
        req = req.header("x-api-key", key);
    }
    req = req.header("anthropic-version", "2023-06-01");
    let resp = req.send().await?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(anyhow::anyhow!("ai error {status}: {text}"));
    }
    let mut out = String::new();
    let stream = resp.bytes_stream().map_err(io::Error::other);
    let reader = tokio_util::io::StreamReader::new(stream);
    let mut lines = tokio::io::BufReader::new(reader).lines();
    while let Some(line) = lines.next_line().await? {
        let line = line.trim();
        if line.is_empty() || !line.starts_with("data:") {
            continue;
        }
        let data = line.trim_start_matches("data:").trim();
        if data == "[DONE]" {
            break;
        }
        let chunk: AnthropicStreamChunk = serde_json::from_str(data)?;
        if let Some(err) = chunk.error {
            return Err(anyhow::anyhow!("anthropic error: {}", err.message));
        }
        let text = match chunk.kind.as_str() {
            "content_block_delta" => chunk.delta.and_then(|d| d.text),
            "content_block_start" => chunk.content_block.and_then(|c| c.text),
            _ => None,
        };
        if let Some(text) = text {
            if !text.is_empty() {
                out.push_str(&text);
                let _ = tx.send(AssistantEvent::Delta(text)).await;
            }
        }
    }
    Ok(out)
}

fn split_system_prompt(messages: Vec<ChatMessage>) -> (Option<String>, Vec<ChatMessage>) {
    let mut system = String::new();
    let mut out = Vec::new();
    for message in messages {
        if message.role == "system" {
            if !system.is_empty() {
                system.push('\n');
            }
            system.push_str(&message.content);
        } else {
            out.push(message);
        }
    }
    let system = if system.trim().is_empty() {
        None
    } else {
        Some(system)
    };
    (system, out)
}

fn extract_tool_calls(content: &str) -> (String, Vec<ToolCall>) {
    let mut calls = Vec::new();
    let mut cleaned = Vec::new();
    for line in content.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("@tool") {
            let json = rest.trim();
            if json.starts_with('{') {
                if let Ok(call) = serde_json::from_str::<ToolCall>(json) {
                    calls.push(call);
                    continue;
                }
            }
        }
        cleaned.push(line);
    }
    let cleaned = cleaned.join("\n").trim().to_string();
    (cleaned, calls)
}

struct ToolContext {
    sessions: Arc<SessionManager>,
    queue: TransferQueue,
    config: AppConfig,
    local_base: String,
    remote_base: String,
    session_id: Option<Uuid>,
    connections: Vec<Connection>,
}

const TOOL_OUTPUT_LIMIT: usize = 8000;
const TOOL_DEFAULT_TIMEOUT_MS: u64 = 20000;
const TOOL_DEFAULT_SEARCH_LIMIT: usize = 50;
const TOOL_DEFAULT_MAX_BYTES: usize = 1_000_000;

async fn execute_tool_call(call: ToolCall, ctx: ToolContext) -> ToolResult {
    let result = match call.name.as_str() {
        "local.exec" => tool_local_exec(&call, &ctx).await,
        "remote.exec" => tool_remote_exec(&call, &ctx).await,
        "local.list" => tool_local_list(&call, &ctx).await,
        "remote.list" => tool_remote_list(&call, &ctx).await,
        "local.read" => tool_local_read(&call, &ctx).await,
        "remote.read" => tool_remote_read(&call, &ctx).await,
        "local.write" => tool_local_write(&call, &ctx).await,
        "remote.write" => tool_remote_write(&call, &ctx).await,
        "local.search" => tool_local_search(&call, &ctx).await,
        "remote.search" => tool_remote_search(&call, &ctx).await,
        "local.mkdir" => tool_local_mkdir(&call, &ctx).await,
        "remote.mkdir" => tool_remote_mkdir(&call, &ctx).await,
        "local.remove" => tool_local_remove(&call, &ctx).await,
        "remote.remove" => tool_remote_remove(&call, &ctx).await,
        "local.rename" => tool_local_rename(&call, &ctx).await,
        "remote.rename" => tool_remote_rename(&call, &ctx).await,
        "transfer.copy" => tool_transfer_copy(&call, &ctx).await,
        "connections.list" => tool_connections_list(&call, &ctx).await,
        _ => Err(anyhow::anyhow!("unknown tool: {}", call.name)),
    };
    match result {
        Ok(output) => ToolResult {
            call,
            success: true,
            output,
        },
        Err(err) => ToolResult {
            call,
            success: false,
            output: err.to_string(),
        },
    }
}

fn tool_arg_string(args: &serde_json::Value, key: &str) -> Option<String> {
    args.get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

fn tool_required_string(args: &serde_json::Value, key: &str) -> Result<String> {
    tool_arg_string(args, key).ok_or_else(|| anyhow::anyhow!("{key} is required"))
}

fn tool_arg_bool(args: &serde_json::Value, key: &str) -> Option<bool> {
    match args.get(key) {
        Some(serde_json::Value::Bool(v)) => Some(*v),
        Some(serde_json::Value::String(v)) => v.parse().ok(),
        _ => None,
    }
}

fn tool_arg_u64(args: &serde_json::Value, key: &str) -> Option<u64> {
    match args.get(key) {
        Some(serde_json::Value::Number(v)) => v.as_u64(),
        Some(serde_json::Value::String(v)) => v.parse().ok(),
        _ => None,
    }
}

fn tool_arg_usize(args: &serde_json::Value, key: &str) -> Option<usize> {
    tool_arg_u64(args, key).map(|v| v as usize)
}

fn resolve_local_path(value: Option<String>, base: &str) -> PathBuf {
    let input = value.unwrap_or_else(|| base.to_string());
    let path = PathBuf::from(&input);
    if path.is_absolute() {
        path
    } else {
        let mut base = PathBuf::from(base);
        base.push(path);
        base
    }
}

fn resolve_remote_path(value: Option<String>, base: &str) -> String {
    let input = value.unwrap_or_else(|| base.to_string());
    if input.starts_with('/') {
        input
    } else if base.ends_with('/') {
        format!("{base}{input}")
    } else {
        format!("{base}/{input}")
    }
}

fn remote_parent(path: &str) -> Option<String> {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() || trimmed == "/" {
        return None;
    }
    if let Some(idx) = trimmed.rfind('/') {
        if idx == 0 {
            return Some("/".to_string());
        }
        return Some(trimmed[..idx].to_string());
    }
    None
}

fn trim_output(value: &str) -> String {
    if value.chars().count() <= TOOL_OUTPUT_LIMIT {
        return value.to_string();
    }
    let trimmed: String = value
        .chars()
        .take(TOOL_OUTPUT_LIMIT.saturating_sub(3))
        .collect();
    format!("{trimmed}...")
}

#[derive(Serialize)]
struct ToolExecOutput {
    status: i32,
    output: String,
}

async fn tool_local_exec(call: &ToolCall, ctx: &ToolContext) -> Result<String> {
    let command = tool_required_string(&call.args, "command")?;
    let timeout_ms = tool_arg_u64(&call.args, "timeout_ms").unwrap_or(TOOL_DEFAULT_TIMEOUT_MS);
    let mut cmd = if cfg!(windows) {
        let mut cmd = Command::new("cmd");
        cmd.arg("/C").arg(&command);
        cmd
    } else {
        let mut cmd = Command::new("sh");
        cmd.arg("-lc").arg(&command);
        cmd
    };
    cmd.current_dir(&ctx.local_base);
    let output = if timeout_ms > 0 {
        timeout(Duration::from_millis(timeout_ms), cmd.output()).await??
    } else {
        cmd.output().await?
    };
    let mut combined = String::new();
    combined.push_str(&String::from_utf8_lossy(&output.stdout));
    combined.push_str(&String::from_utf8_lossy(&output.stderr));
    let status = output.status.code().unwrap_or(-1);
    let result = ToolExecOutput {
        status,
        output: trim_output(&combined),
    };
    Ok(serde_json::to_string_pretty(&result)?)
}

async fn tool_remote_exec(call: &ToolCall, ctx: &ToolContext) -> Result<String> {
    let command = tool_required_string(&call.args, "command")?;
    let timeout_ms = tool_arg_u64(&call.args, "timeout_ms").unwrap_or(TOOL_DEFAULT_TIMEOUT_MS);
    let session_id = ctx
        .session_id
        .ok_or_else(|| anyhow::anyhow!("no active session"))?;
    let handle = ctx
        .sessions
        .get_session(session_id)
        .ok_or_else(|| anyhow::anyhow!("session not found"))?;
    let exec_fut = handle.session.exec(&command);
    let (status, output) = if timeout_ms > 0 {
        timeout(Duration::from_millis(timeout_ms), exec_fut).await??
    } else {
        exec_fut.await?
    };
    let text = String::from_utf8_lossy(&output).to_string();
    let result = ToolExecOutput {
        status,
        output: trim_output(&text),
    };
    Ok(serde_json::to_string_pretty(&result)?)
}

#[derive(Serialize)]
struct ToolListEntry {
    name: String,
    is_dir: bool,
    size: u64,
}

#[derive(Serialize)]
struct ToolListOutput {
    path: String,
    entries: Vec<ToolListEntry>,
}

async fn tool_local_list(call: &ToolCall, ctx: &ToolContext) -> Result<String> {
    let path = resolve_local_path(tool_arg_string(&call.args, "path"), &ctx.local_base);
    let include_hidden =
        tool_arg_bool(&call.args, "include_hidden").unwrap_or(ctx.config.ui.show_hidden_files);
    let limit = tool_arg_usize(&call.args, "limit").unwrap_or(TOOL_DEFAULT_SEARCH_LIMIT);
    let entries = list_local(&path.to_string_lossy()).await?;
    let mut out = Vec::new();
    for entry in entries {
        if !include_hidden && entry.name.starts_with('.') {
            continue;
        }
        out.push(ToolListEntry {
            name: entry.name,
            is_dir: entry.is_dir,
            size: entry.size,
        });
        if out.len() >= limit {
            break;
        }
    }
    let result = ToolListOutput {
        path: path.to_string_lossy().to_string(),
        entries: out,
    };
    Ok(serde_json::to_string_pretty(&result)?)
}

async fn tool_remote_list(call: &ToolCall, ctx: &ToolContext) -> Result<String> {
    let path = resolve_remote_path(tool_arg_string(&call.args, "path"), &ctx.remote_base);
    let include_hidden =
        tool_arg_bool(&call.args, "include_hidden").unwrap_or(ctx.config.ui.show_hidden_files);
    let limit = tool_arg_usize(&call.args, "limit").unwrap_or(TOOL_DEFAULT_SEARCH_LIMIT);
    let session_id = ctx
        .session_id
        .ok_or_else(|| anyhow::anyhow!("no active session"))?;
    let handle = ctx
        .sessions
        .get_session(session_id)
        .ok_or_else(|| anyhow::anyhow!("session not found"))?;
    let sftp = handle.session.open_sftp().await?;
    let entries = list_remote(&sftp, &path).await?;
    let mut out = Vec::new();
    for entry in entries {
        if !include_hidden && entry.name.starts_with('.') {
            continue;
        }
        out.push(ToolListEntry {
            name: entry.name,
            is_dir: entry.is_dir,
            size: entry.size,
        });
        if out.len() >= limit {
            break;
        }
    }
    let result = ToolListOutput { path, entries: out };
    Ok(serde_json::to_string_pretty(&result)?)
}

#[derive(Serialize)]
struct ToolReadOutput {
    path: String,
    content: String,
}

async fn tool_local_read(call: &ToolCall, ctx: &ToolContext) -> Result<String> {
    let path = resolve_local_path(
        Some(tool_required_string(&call.args, "path")?),
        &ctx.local_base,
    );
    let max_bytes = tool_arg_usize(&call.args, "max_bytes").unwrap_or(TOOL_DEFAULT_MAX_BYTES);
    let data = tokio::fs::read(&path).await?;
    let mut text = String::from_utf8_lossy(&data).to_string();
    if text.chars().count() > max_bytes {
        text = text.chars().take(max_bytes).collect();
    }
    let result = ToolReadOutput {
        path: path.to_string_lossy().to_string(),
        content: trim_output(&text),
    };
    Ok(serde_json::to_string_pretty(&result)?)
}

async fn tool_remote_read(call: &ToolCall, ctx: &ToolContext) -> Result<String> {
    let path = resolve_remote_path(
        Some(tool_required_string(&call.args, "path")?),
        &ctx.remote_base,
    );
    let max_bytes = tool_arg_usize(&call.args, "max_bytes").unwrap_or(TOOL_DEFAULT_MAX_BYTES);
    let session_id = ctx
        .session_id
        .ok_or_else(|| anyhow::anyhow!("no active session"))?;
    let handle = ctx
        .sessions
        .get_session(session_id)
        .ok_or_else(|| anyhow::anyhow!("session not found"))?;
    let sftp = handle.session.open_sftp().await?;
    let mut file = sftp.open_read(&path).await?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf).await?;
    let mut text = String::from_utf8_lossy(&buf).to_string();
    if text.chars().count() > max_bytes {
        text = text.chars().take(max_bytes).collect();
    }
    let result = ToolReadOutput {
        path,
        content: trim_output(&text),
    };
    Ok(serde_json::to_string_pretty(&result)?)
}

#[derive(Serialize)]
struct ToolWriteOutput {
    path: String,
    bytes: usize,
}

async fn tool_local_write(call: &ToolCall, ctx: &ToolContext) -> Result<String> {
    let path = resolve_local_path(
        Some(tool_required_string(&call.args, "path")?),
        &ctx.local_base,
    );
    let content = tool_required_string(&call.args, "content")?;
    let append = tool_arg_bool(&call.args, "append").unwrap_or(false);
    let create_dirs = tool_arg_bool(&call.args, "create_dirs").unwrap_or(true);
    if create_dirs {
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
    }
    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .append(append)
        .truncate(!append)
        .open(&path)
        .await?;
    file.write_all(content.as_bytes()).await?;
    let result = ToolWriteOutput {
        path: path.to_string_lossy().to_string(),
        bytes: content.len(),
    };
    Ok(serde_json::to_string_pretty(&result)?)
}

async fn tool_remote_write(call: &ToolCall, ctx: &ToolContext) -> Result<String> {
    let path = resolve_remote_path(
        Some(tool_required_string(&call.args, "path")?),
        &ctx.remote_base,
    );
    let content = tool_required_string(&call.args, "content")?;
    let append = tool_arg_bool(&call.args, "append").unwrap_or(false);
    let create_dirs = tool_arg_bool(&call.args, "create_dirs").unwrap_or(true);
    let session_id = ctx
        .session_id
        .ok_or_else(|| anyhow::anyhow!("no active session"))?;
    let handle = ctx
        .sessions
        .get_session(session_id)
        .ok_or_else(|| anyhow::anyhow!("session not found"))?;
    let sftp = handle.session.open_sftp().await?;
    if create_dirs {
        if let Some(parent) = remote_parent(&path) {
            sftp.create_dir_all(&parent).await?;
        }
    }
    if append {
        let mut existing = Vec::new();
        if let Ok(mut file) = sftp.open_read(&path).await {
            let _ = file.read_to_end(&mut existing).await;
        }
        let mut file = sftp.open_write(&path, true).await?;
        if !existing.is_empty() {
            file.write_all(&existing).await?;
        }
        file.write_all(content.as_bytes()).await?;
    } else {
        let mut file = sftp.open_write(&path, true).await?;
        file.write_all(content.as_bytes()).await?;
    }
    let result = ToolWriteOutput {
        path,
        bytes: content.len(),
    };
    Ok(serde_json::to_string_pretty(&result)?)
}

#[derive(Serialize)]
struct ToolSearchOutput {
    path: String,
    query: String,
    matches: Vec<String>,
}

async fn tool_local_search(call: &ToolCall, ctx: &ToolContext) -> Result<String> {
    let base = resolve_local_path(tool_arg_string(&call.args, "path"), &ctx.local_base);
    let query =
        tool_arg_string(&call.args, "query").ok_or_else(|| anyhow::anyhow!("query is required"))?;
    let limit = tool_arg_usize(&call.args, "limit").unwrap_or(TOOL_DEFAULT_SEARCH_LIMIT);
    let max_bytes = tool_arg_usize(&call.args, "max_bytes").unwrap_or(TOOL_DEFAULT_MAX_BYTES);
    let mut matches = Vec::new();
    for entry in WalkDir::new(&base).into_iter().filter_map(Result::ok) {
        if matches.len() >= limit {
            break;
        }
        if !entry.file_type().is_file() {
            continue;
        }
        let meta = entry.metadata().ok();
        if meta.map(|m| m.len() as usize > max_bytes).unwrap_or(false) {
            continue;
        }
        let data = tokio::fs::read(entry.path()).await?;
        if let Ok(text) = String::from_utf8(data) {
            if text.contains(&query) {
                matches.push(entry.path().to_string_lossy().to_string());
            }
        }
    }
    let result = ToolSearchOutput {
        path: base.to_string_lossy().to_string(),
        query,
        matches,
    };
    Ok(serde_json::to_string_pretty(&result)?)
}

async fn tool_remote_search(call: &ToolCall, ctx: &ToolContext) -> Result<String> {
    let base = resolve_remote_path(tool_arg_string(&call.args, "path"), &ctx.remote_base);
    let query =
        tool_arg_string(&call.args, "query").ok_or_else(|| anyhow::anyhow!("query is required"))?;
    let limit = tool_arg_usize(&call.args, "limit").unwrap_or(TOOL_DEFAULT_SEARCH_LIMIT);
    let max_bytes = tool_arg_usize(&call.args, "max_bytes").unwrap_or(TOOL_DEFAULT_MAX_BYTES);
    let session_id = ctx
        .session_id
        .ok_or_else(|| anyhow::anyhow!("no active session"))?;
    let handle = ctx
        .sessions
        .get_session(session_id)
        .ok_or_else(|| anyhow::anyhow!("session not found"))?;
    let sftp = handle.session.open_sftp().await?;
    let mut stack = vec![base.clone()];
    let mut matches = Vec::new();
    while let Some(path) = stack.pop() {
        if matches.len() >= limit {
            break;
        }
        let entries = sftp.read_dir(&path).await?;
        for entry in entries {
            if matches.len() >= limit {
                break;
            }
            if entry.is_dir {
                stack.push(entry.path);
            } else {
                if entry.size > max_bytes as u64 {
                    continue;
                }
                let mut file = sftp.open_read(&entry.path).await?;
                let mut buf = Vec::new();
                file.read_to_end(&mut buf).await?;
                if let Ok(text) = String::from_utf8(buf) {
                    if text.contains(&query) {
                        matches.push(entry.path.clone());
                    }
                }
            }
        }
    }
    let result = ToolSearchOutput {
        path: base,
        query,
        matches,
    };
    Ok(serde_json::to_string_pretty(&result)?)
}

async fn tool_local_mkdir(call: &ToolCall, ctx: &ToolContext) -> Result<String> {
    let path = resolve_local_path(
        Some(tool_required_string(&call.args, "path")?),
        &ctx.local_base,
    );
    tokio::fs::create_dir_all(&path).await?;
    Ok(serde_json::to_string_pretty(&ToolWriteOutput {
        path: path.to_string_lossy().to_string(),
        bytes: 0,
    })?)
}

async fn tool_remote_mkdir(call: &ToolCall, ctx: &ToolContext) -> Result<String> {
    let path = resolve_remote_path(
        Some(tool_required_string(&call.args, "path")?),
        &ctx.remote_base,
    );
    let session_id = ctx
        .session_id
        .ok_or_else(|| anyhow::anyhow!("no active session"))?;
    let handle = ctx
        .sessions
        .get_session(session_id)
        .ok_or_else(|| anyhow::anyhow!("session not found"))?;
    let sftp = handle.session.open_sftp().await?;
    sftp.create_dir_all(&path).await?;
    Ok(serde_json::to_string_pretty(&ToolWriteOutput {
        path,
        bytes: 0,
    })?)
}

async fn tool_local_remove(call: &ToolCall, ctx: &ToolContext) -> Result<String> {
    let path = resolve_local_path(
        Some(tool_required_string(&call.args, "path")?),
        &ctx.local_base,
    );
    let recursive = tool_arg_bool(&call.args, "recursive").unwrap_or(false);
    if recursive {
        let meta = tokio::fs::metadata(&path).await?;
        if meta.is_dir() {
            tokio::fs::remove_dir_all(&path).await?;
        } else {
            tokio::fs::remove_file(&path).await?;
        }
    } else {
        let meta = tokio::fs::metadata(&path).await?;
        if meta.is_dir() {
            tokio::fs::remove_dir(&path).await?;
        } else {
            tokio::fs::remove_file(&path).await?;
        }
    }
    Ok(serde_json::to_string_pretty(&ToolWriteOutput {
        path: path.to_string_lossy().to_string(),
        bytes: 0,
    })?)
}

async fn tool_remote_remove(call: &ToolCall, ctx: &ToolContext) -> Result<String> {
    let path = resolve_remote_path(
        Some(tool_required_string(&call.args, "path")?),
        &ctx.remote_base,
    );
    let recursive = tool_arg_bool(&call.args, "recursive").unwrap_or(false);
    let session_id = ctx
        .session_id
        .ok_or_else(|| anyhow::anyhow!("no active session"))?;
    let handle = ctx
        .sessions
        .get_session(session_id)
        .ok_or_else(|| anyhow::anyhow!("session not found"))?;
    let sftp = handle.session.open_sftp().await?;
    if recursive {
        let meta = sftp.metadata(&path).await?;
        if !meta.file_type().is_dir() {
            sftp.remove_file(&path).await?;
            return Ok(serde_json::to_string_pretty(&ToolWriteOutput {
                path,
                bytes: 0,
            })?);
        }
        let mut stack = vec![(path.clone(), false)];
        while let Some((current, visited)) = stack.pop() {
            if visited {
                let _ = sftp.remove_dir(&current).await;
                continue;
            }
            stack.push((current.clone(), true));
            for entry in sftp.read_dir(&current).await? {
                if entry.is_dir {
                    stack.push((entry.path, false));
                } else {
                    let _ = sftp.remove_file(&entry.path).await;
                }
            }
        }
    } else {
        let meta = sftp.metadata(&path).await?;
        if meta.file_type().is_dir() {
            sftp.remove_dir(&path).await?;
        } else {
            sftp.remove_file(&path).await?;
        }
    }
    Ok(serde_json::to_string_pretty(&ToolWriteOutput {
        path,
        bytes: 0,
    })?)
}

async fn tool_local_rename(call: &ToolCall, ctx: &ToolContext) -> Result<String> {
    let from = resolve_local_path(
        Some(tool_required_string(&call.args, "from")?),
        &ctx.local_base,
    );
    let to = resolve_local_path(
        Some(tool_required_string(&call.args, "to")?),
        &ctx.local_base,
    );
    tokio::fs::rename(&from, &to).await?;
    Ok(serde_json::to_string_pretty(&ToolWriteOutput {
        path: to.to_string_lossy().to_string(),
        bytes: 0,
    })?)
}

async fn tool_remote_rename(call: &ToolCall, ctx: &ToolContext) -> Result<String> {
    let from = resolve_remote_path(
        Some(tool_required_string(&call.args, "from")?),
        &ctx.remote_base,
    );
    let to = resolve_remote_path(
        Some(tool_required_string(&call.args, "to")?),
        &ctx.remote_base,
    );
    let session_id = ctx
        .session_id
        .ok_or_else(|| anyhow::anyhow!("no active session"))?;
    let handle = ctx
        .sessions
        .get_session(session_id)
        .ok_or_else(|| anyhow::anyhow!("session not found"))?;
    let sftp = handle.session.open_sftp().await?;
    sftp.rename(&from, &to).await?;
    Ok(serde_json::to_string_pretty(&ToolWriteOutput {
        path: to,
        bytes: 0,
    })?)
}

#[derive(Serialize)]
struct ToolTransferOutput {
    job_id: String,
}

async fn tool_transfer_copy(call: &ToolCall, ctx: &ToolContext) -> Result<String> {
    let source_raw = tool_required_string(&call.args, "source")?;
    let dest_raw = tool_required_string(&call.args, "dest")?;
    let session_id = ctx.session_id;
    let (source_ep, source_path) =
        parse_transfer_endpoint(&source_raw, &ctx.local_base, &ctx.remote_base, session_id)?;
    let (dest_ep, dest_path) =
        parse_transfer_endpoint(&dest_raw, &ctx.local_base, &ctx.remote_base, session_id)?;
    let (is_dir, size) = resolve_transfer_meta(&source_ep, &source_path, ctx).await?;
    let file = TransferFile {
        source_path: source_path.clone(),
        dest_path: dest_path.clone(),
        size,
        is_dir,
    };
    let job = TransferJob {
        id: Uuid::new_v4(),
        source: source_ep,
        dest: dest_ep,
        files: vec![file],
        options: TransferOptions {
            overwrite: catsolle_core::transfer::OverwriteMode::Replace,
            preserve_permissions: true,
            preserve_times: true,
            verify_checksum: ctx.config.transfer.verify_checksum,
            resume: ctx.config.transfer.resume,
            buffer_size: ctx.config.transfer.buffer_size,
        },
        state: TransferState::Queued,
        progress: Default::default(),
        created_at: chrono::Utc::now(),
    };
    let job_id = job.id;
    ctx.queue
        .enqueue(job)
        .await
        .map_err(|e| anyhow::anyhow!(e.to_string()))?;
    Ok(serde_json::to_string_pretty(&ToolTransferOutput {
        job_id: job_id.to_string(),
    })?)
}

async fn resolve_transfer_meta(
    source: &TransferEndpoint,
    path: &str,
    ctx: &ToolContext,
) -> Result<(bool, u64)> {
    match source {
        TransferEndpoint::Local { .. } => {
            let meta = tokio::fs::metadata(path).await?;
            Ok((meta.is_dir(), meta.len()))
        }
        TransferEndpoint::Remote { session_id, .. } => {
            let handle = ctx
                .sessions
                .get_session(*session_id)
                .ok_or_else(|| anyhow::anyhow!("session not found"))?;
            let sftp = handle.session.open_sftp().await?;
            let meta = sftp.metadata(path).await?;
            let is_dir = meta.file_type().is_dir();
            let size = meta.size.unwrap_or(0);
            Ok((is_dir, size))
        }
    }
}

fn parse_transfer_endpoint(
    raw: &str,
    local_base: &str,
    remote_base: &str,
    session_id: Option<Uuid>,
) -> Result<(TransferEndpoint, String)> {
    let raw = raw.trim();
    if let Some(rest) = raw.strip_prefix("remote:") {
        let session_id = session_id.ok_or_else(|| anyhow::anyhow!("no active session"))?;
        let path = resolve_remote_path(Some(rest.trim().to_string()), remote_base);
        Ok((
            TransferEndpoint::Remote {
                session_id,
                path: path.clone(),
            },
            path,
        ))
    } else {
        let rest = raw.strip_prefix("local:").unwrap_or(raw);
        let path = resolve_local_path(Some(rest.trim().to_string()), local_base)
            .to_string_lossy()
            .to_string();
        Ok((
            TransferEndpoint::Local {
                path: PathBuf::from(&path),
            },
            path,
        ))
    }
}

#[derive(Serialize)]
struct ToolConnectionsOutput {
    connections: Vec<ToolConnectionInfo>,
}

#[derive(Serialize)]
struct ToolConnectionInfo {
    id: String,
    name: String,
    host: String,
    port: u16,
    username: String,
}

async fn tool_connections_list(_call: &ToolCall, ctx: &ToolContext) -> Result<String> {
    let mut out = Vec::new();
    for conn in &ctx.connections {
        out.push(ToolConnectionInfo {
            id: conn.id.to_string(),
            name: conn.name.clone(),
            host: conn.host.clone(),
            port: conn.port,
            username: conn.username.clone(),
        });
    }
    Ok(serde_json::to_string_pretty(&ToolConnectionsOutput {
        connections: out,
    })?)
}

fn panel_visible_rows(area: Rect) -> usize {
    area.height.saturating_sub(2) as usize
}

fn format_bytes(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = KIB * 1024.0;
    const GIB: f64 = MIB * 1024.0;
    let b = bytes as f64;
    if b >= GIB {
        format!("{:.1}GiB", b / GIB)
    } else if b >= MIB {
        format!("{:.1}MiB", b / MIB)
    } else if b >= KIB {
        format!("{:.1}KiB", b / KIB)
    } else {
        format!("{}B", bytes)
    }
}

fn format_duration(secs: u64) -> String {
    let hours = secs / 3600;
    let minutes = (secs % 3600) / 60;
    let seconds = secs % 60;
    if hours > 0 {
        format!("{hours}h{minutes:02}m")
    } else if minutes > 0 {
        format!("{minutes}m{seconds:02}s")
    } else {
        format!("{seconds}s")
    }
}

fn provider_list() -> [&'static str; 4] {
    ["ollama", "openai", "openrouter", "anthropic"]
}

fn cycle_provider(current: &str, delta: i32) -> String {
    let list = provider_list();
    let pos = list
        .iter()
        .position(|p| p.eq_ignore_ascii_case(current))
        .unwrap_or(0);
    let len = list.len() as i32;
    let mut next = pos as i32 + delta;
    if next < 0 {
        next = len - 1;
    }
    if next >= len {
        next = 0;
    }
    list[next as usize].to_string()
}

fn provider_defaults(provider: &str) -> (String, String) {
    match provider.trim().to_lowercase().as_str() {
        "openai" => (
            "https://api.openai.com".to_string(),
            "gpt-4o-mini".to_string(),
        ),
        "openrouter" => (
            "https://openrouter.ai/api".to_string(),
            "anthropic/claude-opus-4-5".to_string(),
        ),
        "anthropic" => (
            "https://api.anthropic.com".to_string(),
            "opus-4.5".to_string(),
        ),
        _ => (
            "http://localhost:11434".to_string(),
            "qwen2.5:3b".to_string(),
        ),
    }
}

fn tool_definitions() -> Vec<String> {
    vec![
        "Tools:",
        "- local.exec {command, timeout_ms?}",
        "- remote.exec {command, timeout_ms?}",
        "- local.list {path?, limit?, include_hidden?}",
        "- remote.list {path?, limit?, include_hidden?}",
        "- local.read {path, max_bytes?}",
        "- remote.read {path, max_bytes?}",
        "- local.write {path, content, append?, create_dirs?}",
        "- remote.write {path, content, append?, create_dirs?}",
        "- local.search {path?, query, limit?, max_bytes?}",
        "- remote.search {path?, query, limit?, max_bytes?}",
        "- local.mkdir {path}",
        "- remote.mkdir {path}",
        "- local.remove {path, recursive?}",
        "- remote.remove {path, recursive?}",
        "- local.rename {from, to}",
        "- remote.rename {from, to}",
        "- transfer.copy {source, dest}",
        "- connections.list {}",
    ]
    .into_iter()
    .map(|s| s.to_string())
    .collect()
}

fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(r);
    let horizontal = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup_layout[1]);
    horizontal[1]
}

fn default_ssh_config_paths() -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    let mut push_path = |path: PathBuf| {
        let key = path.to_string_lossy().to_string();
        if seen.insert(key) {
            out.push(path);
        }
    };
    if let Some(user_dirs) = UserDirs::new() {
        push_path(user_dirs.home_dir().join(".ssh").join("config"));
    }
    if let Ok(home) = std::env::var("HOME") {
        push_path(PathBuf::from(home).join(".ssh").join("config"));
    }
    if let Ok(profile) = std::env::var("USERPROFILE") {
        push_path(PathBuf::from(&profile).join(".ssh").join("config"));
        if cfg!(target_os = "linux") {
            if let Some(wsl) = windows_path_to_wsl(&profile) {
                push_path(wsl.join(".ssh").join("config"));
            }
        }
    }
    if let (Ok(drive), Ok(path)) = (std::env::var("HOMEDRIVE"), std::env::var("HOMEPATH")) {
        let win = format!("{}{}", drive, path);
        push_path(PathBuf::from(&win).join(".ssh").join("config"));
        if cfg!(target_os = "linux") {
            if let Some(wsl) = windows_path_to_wsl(&win) {
                push_path(wsl.join(".ssh").join("config"));
            }
        }
    }
    out
}

fn windows_path_to_wsl(path: &str) -> Option<PathBuf> {
    let (drive, rest) = if let Some((drive, rest)) = path.split_once(":\\") {
        (drive, rest)
    } else if let Some((drive, rest)) = path.split_once(":/") {
        (drive, rest)
    } else {
        return None;
    };
    if drive.len() != 1 {
        return None;
    }
    let drive = drive.chars().next()?.to_ascii_lowercase();
    let rest = rest.replace('\\', "/");
    Some(PathBuf::from(format!("/mnt/{}/{}", drive, rest)))
}

fn parse_named_target(
    input: &str,
    default_user: &str,
    default_port: u16,
) -> Result<(Option<String>, String, String, u16), ()> {
    let raw = input.trim();
    if raw.is_empty() {
        return Err(());
    }
    let (name, target) = if let Some((name, target)) = raw.split_once('|') {
        let name = name.trim();
        let target = target.trim();
        if name.is_empty() || target.is_empty() {
            return Err(());
        }
        (Some(name.to_string()), target)
    } else {
        (None, raw)
    };
    let (user, host, port) = parse_target(target, default_user, default_port)?;
    Ok((name, user, host, port))
}

fn parse_target(
    input: &str,
    default_user: &str,
    default_port: u16,
) -> Result<(String, String, u16), ()> {
    let raw = input.trim();
    if raw.is_empty() {
        return Err(());
    }
    let (user_part, host_part) = if let Some((user, host)) = raw.split_once('@') {
        if user.is_empty() || host.is_empty() {
            return Err(());
        }
        (user, host)
    } else {
        (default_user, raw)
    };
    let (host, port) = split_host_port(host_part, default_port)?;
    Ok((user_part.to_string(), host, port))
}

fn split_host_port(input: &str, default_port: u16) -> Result<(String, u16), ()> {
    let input = input.trim();
    if input.is_empty() {
        return Err(());
    }
    if let Some(rest) = input.strip_prefix('[') {
        if let Some(end) = rest.find(']') {
            let host = &rest[..end];
            let after = &rest[end + 1..];
            if after.is_empty() {
                return Ok((host.to_string(), default_port));
            }
            if let Some(port_str) = after.strip_prefix(':') {
                let port = port_str.parse::<u16>().map_err(|_| ())?;
                return Ok((host.to_string(), port));
            }
            return Err(());
        }
        return Err(());
    }
    if let Some((host, port_str)) = input.rsplit_once(':') {
        if host.is_empty() || port_str.is_empty() {
            return Err(());
        }
        let port = port_str.parse::<u16>().map_err(|_| ())?;
        return Ok((host.to_string(), port));
    }
    Ok((input.to_string(), default_port))
}

fn agent_available() -> bool {
    if cfg!(windows) {
        return true;
    }
    std::env::var("SSH_AUTH_SOCK").is_ok()
}

fn should_prompt_password(err: &str) -> bool {
    let lower = err.to_lowercase();
    lower.contains("missing password")
        || lower.contains("master password required")
        || lower.contains("ssh_auth_sock")
        || lower.contains("agent authentication failed")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MdBlockType {
    Normal,
    Code,
}

struct MdRenderContext {
    block_type: MdBlockType,
    theme: Theme,
    base_style: Style,
}

fn render_markdown_lines(content: &str, theme: Theme, base_style: Style) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let mut ctx = MdRenderContext {
        block_type: MdBlockType::Normal,
        theme,
        base_style,
    };

    for line in content.lines() {
        if line.starts_with("```") {
            ctx.block_type = match ctx.block_type {
                MdBlockType::Normal => MdBlockType::Code,
                MdBlockType::Code => MdBlockType::Normal,
            };
            let lang = line.trim_start_matches('`').trim();
            if !lang.is_empty() && ctx.block_type == MdBlockType::Code {
                lines.push(Line::from(Span::styled(
                    format!("─── {lang} ───"),
                    Style::default().fg(theme.muted),
                )));
            } else {
                lines.push(Line::from(Span::styled(
                    "───────────",
                    Style::default().fg(theme.muted),
                )));
            }
            continue;
        }

        match ctx.block_type {
            MdBlockType::Code => {
                lines.push(Line::from(Span::styled(
                    format!("  {line}"),
                    Style::default().fg(theme.accent_alt),
                )));
            }
            MdBlockType::Normal => {
                lines.push(render_markdown_line(line, &ctx));
            }
        }
    }
    lines
}

fn render_markdown_line(line: &str, ctx: &MdRenderContext) -> Line<'static> {
    let theme = ctx.theme;

    if let Some(rest) = line.strip_prefix("### ") {
        return Line::from(Span::styled(
            rest.to_string(),
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        ));
    }
    if let Some(rest) = line.strip_prefix("## ") {
        return Line::from(Span::styled(
            rest.to_string(),
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        ));
    }
    if let Some(rest) = line.strip_prefix("# ") {
        return Line::from(Span::styled(
            rest.to_string(),
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
        ));
    }

    if line.starts_with("- ") || line.starts_with("* ") {
        let rest = &line[2..];
        let mut spans = vec![Span::styled("• ", Style::default().fg(theme.accent))];
        spans.extend(parse_inline_markdown(rest, ctx));
        return Line::from(spans);
    }

    if line.starts_with("| ") && line.ends_with(" |") {
        return Line::from(Span::styled(
            line.to_string(),
            Style::default().fg(theme.muted),
        ));
    }
    if line.chars().all(|c| c == '-' || c == '|' || c == ' ') && line.contains('-') {
        return Line::from(Span::styled(
            line.to_string(),
            Style::default().fg(theme.muted),
        ));
    }

    Line::from(parse_inline_markdown(line, ctx))
}

fn parse_inline_markdown(text: &str, ctx: &MdRenderContext) -> Vec<Span<'static>> {
    let theme = ctx.theme;
    let base = ctx.base_style;
    let mut spans = Vec::new();
    let mut chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    let mut current = String::new();

    while i < chars.len() {
        if chars[i] == '`' {
            if !current.is_empty() {
                spans.push(Span::styled(current.clone(), base));
                current.clear();
            }
            let start = i + 1;
            i += 1;
            while i < chars.len() && chars[i] != '`' {
                i += 1;
            }
            if i < chars.len() {
                let code: String = chars[start..i].iter().collect();
                spans.push(Span::styled(
                    code,
                    Style::default().fg(theme.accent_alt),
                ));
                i += 1;
            }
            continue;
        }

        if chars[i] == '*' && i + 1 < chars.len() && chars[i + 1] == '*' {
            if !current.is_empty() {
                spans.push(Span::styled(current.clone(), base));
                current.clear();
            }
            let start = i + 2;
            i += 2;
            while i + 1 < chars.len() && !(chars[i] == '*' && chars[i + 1] == '*') {
                i += 1;
            }
            if i + 1 < chars.len() {
                let bold: String = chars[start..i].iter().collect();
                spans.push(Span::styled(
                    bold,
                    base.add_modifier(Modifier::BOLD),
                ));
                i += 2;
            }
            continue;
        }

        if chars[i] == '*' || chars[i] == '_' {
            let marker = chars[i];
            if i + 1 < chars.len() && chars[i + 1] != ' ' && chars[i + 1] != marker {
                if !current.is_empty() {
                    spans.push(Span::styled(current.clone(), base));
                    current.clear();
                }
                let start = i + 1;
                i += 1;
                while i < chars.len() && chars[i] != marker {
                    i += 1;
                }
                if i < chars.len() {
                    let italic: String = chars[start..i].iter().collect();
                    spans.push(Span::styled(
                        italic,
                        base.add_modifier(Modifier::ITALIC),
                    ));
                    i += 1;
                    continue;
                }
            }
        }

        current.push(chars[i]);
        i += 1;
    }

    if !current.is_empty() {
        spans.push(Span::styled(current, base));
    }

    if spans.is_empty() {
        spans.push(Span::styled(String::new(), base));
    }

    spans
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_tool_calls() {
        let input = "Hello\n@tool {\"name\":\"local.exec\",\"args\":{\"command\":\"ls\"}}\nDone";
        let (cleaned, calls) = extract_tool_calls(input);
        assert_eq!(cleaned, "Hello\nDone");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "local.exec");
    }

    #[test]
    fn ignores_invalid_tool_calls() {
        let input = "Hello\n@tool not-json\nDone";
        let (cleaned, calls) = extract_tool_calls(input);
        assert_eq!(cleaned, input);
        assert!(calls.is_empty());
    }
}
