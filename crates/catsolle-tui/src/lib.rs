use anyhow::Result;
use catsolle_config::{AiConfig, AppConfig, I18n};
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
    StreamExt,
};
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Wrap};
use ratatui::Terminal;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::io::{self, Stdout};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use uuid::Uuid;
use vt100::Parser;
use zeroize::Zeroizing;

pub async fn run(
    store: ConnectionStore,
    sessions: Arc<SessionManager>,
    queue: TransferQueue,
    bus: EventBus,
    config: AppConfig,
    i18n: I18n,
) -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = ratatui::backend::CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let res = run_app(&mut terminal, store, sessions, queue, bus, config, i18n).await;

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    res
}

async fn run_app(
    terminal: &mut Terminal<ratatui::backend::CrosstermBackend<Stdout>>,
    store: ConnectionStore,
    sessions: Arc<SessionManager>,
    queue: TransferQueue,
    bus: EventBus,
    config: AppConfig,
    i18n: I18n,
) -> Result<()> {
    let mut event_stream = EventStream::new();
    let mut event_rx = bus.subscribe();
    let (assistant_tx, mut assistant_rx) = mpsc::channel::<AssistantEvent>(16);
    let mut app = AppState::new(store, sessions, queue, config, i18n, assistant_tx).await?;

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
}

#[derive(Clone, Debug)]
struct AssistantMessage {
    role: AssistantRole,
    content: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AssistantRole {
    User,
    Assistant,
    System,
    Error,
}

#[derive(Clone, Debug)]
enum AssistantEvent {
    Response(String),
    Error(String),
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
            format!("{} · {}", self.i18n.tr("ai-title"), self.i18n.tr("status-ai-busy"))
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
        let border = if active { theme.accent_alt } else { theme.muted };
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
                Span::styled(self.assistant.input.clone(), Style::default().fg(theme.text)),
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
            spans.push(Span::styled(
                status,
                Style::default().fg(theme.accent_soft),
            ));
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

    fn footer_text(&self) -> Text<'_> {
        match &self.overlay {
            Overlay::QuickAdd { .. } => Text::from(self.i18n.tr("footer-quick-add")),
            Overlay::Edit { .. } => Text::from(self.i18n.tr("footer-edit")),
            Overlay::Password { .. } => Text::from(self.i18n.tr("footer-password")),
            Overlay::Help => Text::from(self.i18n.tr("footer-help")),
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
                    lines.push(Line::from(Span::styled(
                        line.to_string(),
                        text_style,
                    )));
                }
            }
            lines.push(Line::from(""));
        }
        if lines.is_empty() {
            lines.push(Line::from(self.i18n.tr("ai-empty")));
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
        if self.assistant.busy {
            self.i18n.tr("status-ai-busy")
        } else {
            self.i18n.tr("status-ai-on")
        }
    }

    fn ai_config_error(&self) -> Option<String> {
        if !self.config.ai.enabled {
            return None;
        }
        let provider = self.config.ai.provider.trim().to_lowercase();
        if provider != "ollama" && provider != "openai-compatible" && provider != "openai" {
            return Some(self.i18n.tr("ai-config-provider"));
        }
        if self.config.ai.endpoint.trim().is_empty() {
            return Some(self.i18n.tr("ai-config-endpoint"));
        }
        if self.config.ai.model.trim().is_empty() {
            return Some(self.i18n.tr("ai-config-model"));
        }
        if (provider == "openai-compatible" || provider == "openai")
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

    async fn handle_bus_event(&mut self, event: CoreEvent) -> Result<()> {
        if let CoreEvent::TransferProgress { job_id, progress } = event {
            self.transfer_status = Some(TransferStatus {
                progress: progress.clone(),
            });
            if progress.files_total > 0 && progress.files_completed >= progress.files_total {
                if self.completed_transfers.insert(job_id) {
                    if matches!(self.mode, AppMode::Session { .. }) {
                        self.refresh_panels().await?;
                    }
                }
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
                        Err(err) => error = Some(err),
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
                        Err(err) => error = Some(err),
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
                                        Err(err) => state.error = Some(err),
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
                                        Err(err) => state.error = Some(err),
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
            _ => Ok(false),
        }
    }

    async fn handle_session_key(&mut self, key: KeyEvent) -> Result<bool> {
        match key.code {
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
        let cfg = self.config.ai.clone();
        let client = self.ai_client.clone();
        let tx = self.assistant_tx.clone();
        self.assistant.busy = true;
        tokio::spawn(async move {
            let result = request_ai(client, &cfg, messages).await;
            let event = match result {
                Ok(content) => AssistantEvent::Response(content),
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
            .filter(|m| matches!(m.role, AssistantRole::User | AssistantRole::Assistant))
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
        prompt
    }

    fn handle_assistant_event(&mut self, event: AssistantEvent) {
        self.assistant.busy = false;
        match event {
            AssistantEvent::Response(content) => {
                self.assistant.push_message(
                    AssistantMessage {
                        role: AssistantRole::Assistant,
                        content,
                    },
                    self.config.ai.history_max,
                );
            }
            AssistantEvent::Error(error) => {
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
        }
    }

    async fn enter_session(&mut self, session_id: Uuid, conn: Connection) -> Result<()> {
        self.mode = AppMode::Session { id: session_id };
        self.active_connection = Some(conn);
        self.input_focus = InputFocus::Files;
        self.transfer_status = None;
        self.assistant = AssistantState::new(&self.i18n);
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

async fn request_ai(
    client: reqwest::Client,
    cfg: &AiConfig,
    messages: Vec<ChatMessage>,
) -> Result<String> {
    let provider = cfg.provider.trim().to_lowercase();
    if provider == "ollama" {
        request_ollama(client, cfg, messages).await
    } else if provider == "openai-compatible" || provider == "openai" {
        request_openai(client, cfg, messages).await
    } else {
        Err(anyhow::anyhow!("unknown ai provider: {}", cfg.provider))
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

async fn request_openai(
    client: reqwest::Client,
    cfg: &AiConfig,
    messages: Vec<ChatMessage>,
) -> Result<String> {
    let url = format!(
        "{}/v1/chat/completions",
        cfg.endpoint.trim_end_matches('/')
    );
    let body = OpenAiRequest {
        model: cfg.model.clone(),
        messages,
        temperature: cfg.temperature,
        max_tokens: cfg.max_tokens,
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
