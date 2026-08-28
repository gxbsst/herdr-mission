//! Full-screen Mission control center built on ratatui.
//!
//! The visual and interaction model mirrors the earlier Python
//! `mission-center.py`: a queue table with Chinese stage labels and colors, a
//! live keyword filter, a persistent "current selection" detail box, and a
//! footer of shortcuts. Only the Rust Team Mission surface is exposed (create /
//! resume / dispatch / deliver / doctor); the Python single-agent review,
//! verify, archive, and evidence lifecycle is intentionally out of scope.

use std::{io::Write, path::Path, sync::mpsc, time::Duration};

use ratatui::{
    crossterm::{
        event::{self, DisableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
        execute,
    },
    layout::{Constraint, Layout, Margin, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Cell, Padding, Paragraph, Row, Table, TableState, Wrap},
    DefaultTerminal, Frame,
};
use throbber_widgets_tui::{Throbber, ThrobberState};

use crate::{
    bootstrap_database, create_mission, delete_mission, herdr_bin, kernel_deliver,
    kernel_dispatch_command, launch_mission, make_mission_id, read_mission_overviews, source_cwd,
    workspace_close_argv, CreateMissionRequest, KernelError, LaunchConfig, LaunchMode,
    LaunchOptions, MissionLayout, MissionOverview, ProcessRunner, Provider, RoleOverview,
    SystemProcessRunner, WorkspaceSource,
};

const TICK: Duration = Duration::from_secs(2);
const SPINNER_TICK: Duration = Duration::from_millis(100);
const ROLES: [&str; 4] = ["pm", "worker", "scout", "reviewer"];

/// A long-running action to run off the UI thread so the TUI can keep drawing.
enum Job {
    Deliver,
    Resume {
        mission_id: String,
    },
    New {
        title: String,
        profile: Provider,
        roles: Vec<String>,
        launch_mode: LaunchMode,
        workspace_source: WorkspaceSource,
        worktree_path: String,
    },
    Send {
        mission_id: String,
        target: String,
        body: String,
    },
    Delete {
        mission_id: String,
    },
}

struct JobOutcome {
    message: String,
}

/// Run the interactive TUI until the user quits.
pub fn run_tui(database: &Path) -> Result<(), String> {
    bootstrap_database(database).map_err(|error| error_line(&error))?;

    let mut app = App::new(database)?;
    let mut terminal = ratatui::try_init().map_err(|error| format!("terminal init: {error}"))?;
    let mut output = std::io::stdout();
    if let Err(error) = enable_native_text_selection(&mut output) {
        ratatui::restore();
        return Err(format!("terminal mouse setup: {error}"));
    }
    let result = app.run(&mut terminal);
    ratatui::restore();
    result
}

fn enable_native_text_selection(output: &mut impl Write) -> std::io::Result<()> {
    execute!(output, DisableMouseCapture)
}

enum View {
    List,
    NewPrompt,
    SendForm,
    Help,
}

/// Focusable fields inside the new-Mission form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FormField {
    Layout,
    LaunchMode,
    Workspace,
    WorktreePath,
    Profile,
    Roles,
    Title,
}

struct App {
    database: String,
    catalog: Vec<MissionOverview>,
    missions: Vec<MissionOverview>,
    selected: usize,
    table_state: TableState,
    view: View,
    search: String,
    searching: bool,
    input: String,
    send_role: usize,
    new_layout: MissionLayout,
    new_launch_mode: LaunchMode,
    new_workspace_source: WorkspaceSource,
    new_worktree_path: String,
    new_profile: Provider,
    layout_cursor: usize,
    launch_mode_cursor: usize,
    workspace_cursor: usize,
    profile_cursor: usize,
    new_field: FormField,
    new_roles: [bool; 4],
    new_role_idx: usize,
    message: String,
    busy: Option<String>,
    throbber_state: ThrobberState,
    job_rx: Option<mpsc::Receiver<JobOutcome>>,
    confirm_delete: bool,
    should_quit: bool,
}

impl App {
    fn new(database: &Path) -> Result<Self, String> {
        let catalog = read_mission_overviews(database).map_err(|error| error_line(&error))?;
        let config = LaunchConfig::load();
        let launch_mode_cursor = LaunchMode::ALL
            .iter()
            .position(|mode| *mode == config.launch.launch_mode)
            .unwrap_or(1);
        let mut app = Self {
            database: database.to_string_lossy().into_owned(),
            catalog,
            missions: Vec::new(),
            selected: 0,
            table_state: TableState::default(),
            view: View::List,
            search: String::new(),
            searching: false,
            input: String::new(),
            send_role: 1,
            new_layout: MissionLayout::Team,
            new_launch_mode: config.launch.launch_mode,
            new_workspace_source: WorkspaceSource::Current,
            new_worktree_path: String::new(),
            new_profile: Provider::Codex,
            layout_cursor: 0,
            launch_mode_cursor,
            workspace_cursor: 0,
            profile_cursor: 0,
            new_field: FormField::Layout,
            new_roles: [true; 4],
            new_role_idx: 0,
            message: String::new(),
            busy: None,
            throbber_state: ThrobberState::default(),
            job_rx: None,
            confirm_delete: false,
            should_quit: false,
        };
        app.apply_search();
        Ok(app)
    }

    fn run(&mut self, terminal: &mut DefaultTerminal) -> Result<(), String> {
        while !self.should_quit {
            self.poll_job();
            terminal
                .draw(|frame| self.render(frame))
                .map_err(|error| format!("terminal draw: {error}"))?;
            let tick = if self.busy.is_some() {
                SPINNER_TICK
            } else {
                TICK
            };
            if event::poll(tick).map_err(|error| format!("terminal event: {error}"))? {
                if let Event::Key(key) =
                    event::read().map_err(|error| format!("terminal read: {error}"))?
                {
                    if key.kind == KeyEventKind::Press {
                        if self.busy.is_some() {
                            if key.code == KeyCode::Char('q') {
                                self.should_quit = true;
                            }
                            continue;
                        }
                        self.handle_key(key);
                    }
                }
            } else if self.busy.is_some() {
                self.throbber_state.calc_next();
            } else {
                self.refresh();
            }
        }
        Ok(())
    }

    fn refresh(&mut self) {
        match read_mission_overviews(Path::new(&self.database)) {
            Ok(catalog) => {
                self.catalog = catalog;
                self.apply_search();
            }
            Err(error) => self.message = error_line(&error),
        }
    }

    fn apply_search(&mut self) {
        let terms: Vec<String> = self
            .search
            .to_lowercase()
            .split_whitespace()
            .map(str::to_string)
            .collect();
        if terms.is_empty() {
            self.missions = self.catalog.clone();
        } else {
            self.missions = self
                .catalog
                .iter()
                .filter(|mission| {
                    let haystack = mission_search_text(mission);
                    terms.iter().all(|term| haystack.contains(term.as_str()))
                })
                .cloned()
                .collect();
        }
        if self.selected >= self.missions.len() {
            self.selected = self.missions.len().saturating_sub(1);
        }
    }

    fn selected_mission(&self) -> Option<&MissionOverview> {
        self.missions.get(self.selected)
    }

    fn handle_key(&mut self, key: KeyEvent) {
        match self.view {
            View::List => self.handle_list_key(key),
            View::NewPrompt => self.handle_new_key(key),
            View::SendForm => self.handle_send_key(key),
            View::Help => self.handle_help_key(key),
        }
    }

    fn handle_list_key(&mut self, key: KeyEvent) {
        if self.searching {
            match key.code {
                KeyCode::Esc => {
                    self.searching = false;
                    self.search.clear();
                    self.apply_search();
                }
                KeyCode::Enter => self.searching = false,
                KeyCode::Backspace => {
                    self.search.pop();
                    self.selected = 0;
                    self.apply_search();
                }
                KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.search.clear();
                    self.selected = 0;
                    self.apply_search();
                }
                KeyCode::Char(ch) => {
                    self.search.push(ch);
                    self.selected = 0;
                    self.apply_search();
                }
                _ => {}
            }
            return;
        }
        if self.confirm_delete && !matches!(key.code, KeyCode::Char('x')) {
            self.confirm_delete = false;
            self.message.clear();
        }
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => self.should_quit = true,
            KeyCode::Char('j') | KeyCode::Down => self.move_selection(1),
            KeyCode::Char('k') | KeyCode::Up => self.move_selection(-1),
            KeyCode::Char('/') => self.searching = true,
            KeyCode::Char('n') => {
                self.input.clear();
                self.view = View::NewPrompt;
            }
            KeyCode::Char('s') => {
                self.input.clear();
                self.send_role = 1;
                self.view = View::SendForm;
            }
            KeyCode::Char('?') => self.view = View::Help,
            KeyCode::Char('d') => self.do_deliver(),
            KeyCode::Char('c') => self.do_doctor(),
            KeyCode::Char('r') | KeyCode::Enter => self.do_resume(),
            KeyCode::Char('x') => self.do_delete(),
            _ => {}
        }
    }

    fn handle_new_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.input.clear();
                self.view = View::List;
            }
            KeyCode::Tab | KeyCode::Right | KeyCode::Char('l') => {
                self.new_field =
                    next_form_field(self.new_field, self.new_layout, self.new_workspace_source)
            }
            KeyCode::Left | KeyCode::Char('h') => {
                self.new_field =
                    prev_form_field(self.new_field, self.new_layout, self.new_workspace_source)
            }
            KeyCode::Up | KeyCode::Char('k') => self.move_option(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_option(1),
            KeyCode::Char(' ') => self.toggle_option(),
            KeyCode::Enter => {
                let title = self.input.clone();
                self.input.clear();
                self.view = View::List;
                self.do_new(&title);
            }
            KeyCode::Backspace => {
                if self.new_field == FormField::Title {
                    self.input.pop();
                } else if self.new_field == FormField::WorktreePath {
                    self.new_worktree_path.pop();
                }
            }
            KeyCode::Char(ch) => {
                if self.new_field == FormField::Title {
                    self.input.push(ch);
                } else if self.new_field == FormField::WorktreePath {
                    self.new_worktree_path.push(ch);
                }
            }
            _ => {}
        }
    }

    fn move_option(&mut self, delta: isize) {
        match self.new_field {
            FormField::Layout => self.layout_cursor = move_index(self.layout_cursor, 2, delta),
            FormField::LaunchMode => {
                self.launch_mode_cursor =
                    move_index(self.launch_mode_cursor, LaunchMode::ALL.len(), delta)
            }
            FormField::Workspace => {
                self.workspace_cursor = move_index(self.workspace_cursor, 3, delta)
            }
            FormField::Profile => {
                self.profile_cursor = move_index(self.profile_cursor, Provider::ALL.len(), delta)
            }
            FormField::Roles => {
                self.new_role_idx = move_index(self.new_role_idx, ROLES.len(), delta)
            }
            FormField::Title | FormField::WorktreePath => {}
        }
    }

    fn toggle_option(&mut self) {
        match self.new_field {
            FormField::Layout => self.new_layout = layout_at(self.layout_cursor),
            FormField::LaunchMode => self.new_launch_mode = launch_mode_at(self.launch_mode_cursor),
            FormField::Workspace => {
                self.new_workspace_source = workspace_source_at(self.workspace_cursor)
            }
            FormField::Profile => self.new_profile = profile_at(self.profile_cursor),
            FormField::Roles => {
                self.new_roles[self.new_role_idx] = !self.new_roles[self.new_role_idx]
            }
            FormField::Title | FormField::WorktreePath => {}
        }
    }

    fn handle_send_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => self.view = View::List,
            KeyCode::Tab | KeyCode::Down | KeyCode::Up => {
                self.send_role = (self.send_role + 1) % ROLES.len();
            }
            KeyCode::Enter => {
                let body = self.input.clone();
                let target = ROLES[self.send_role].to_string();
                self.input.clear();
                self.view = View::List;
                self.do_send(&target, &body);
            }
            KeyCode::Backspace => {
                self.input.pop();
            }
            KeyCode::Char(ch) => self.input.push(ch),
            _ => {}
        }
    }

    fn handle_help_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc | KeyCode::Char('?') => self.view = View::List,
            _ => {}
        }
    }

    fn move_selection(&mut self, delta: isize) {
        if self.missions.is_empty() {
            return;
        }
        let len = self.missions.len() as isize;
        self.selected = ((self.selected as isize + delta).rem_euclid(len)) as usize;
    }

    fn start_job(&mut self, job: Job, busy: String) {
        if self.busy.is_some() {
            return;
        }
        let database = self.database.clone();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let outcome = run_job(&database, job);
            let _ = tx.send(outcome);
        });
        self.job_rx = Some(rx);
        self.busy = Some(busy);
        self.message.clear();
    }

    fn poll_job(&mut self) {
        let Some(rx) = &self.job_rx else {
            return;
        };
        match rx.try_recv() {
            Ok(outcome) => {
                self.job_rx = None;
                self.busy = None;
                self.message = outcome.message;
                self.refresh();
            }
            Err(mpsc::TryRecvError::Empty) => {}
            Err(mpsc::TryRecvError::Disconnected) => {
                self.job_rx = None;
                self.busy = None;
                self.message = "后台任务异常结束".to_string();
                self.refresh();
            }
        }
    }

    fn do_deliver(&mut self) {
        self.start_job(Job::Deliver, "正在投递待处理消息…".to_string());
    }

    fn do_doctor(&mut self) {
        match bootstrap_database(Path::new(&self.database)) {
            Ok(outcome) => {
                self.message = format!(
                    "自检通过：schema 就绪，owner={} created={}",
                    outcome.owner, outcome.created
                );
            }
            Err(error) => self.message = error_line(&error),
        }
    }

    fn do_resume(&mut self) {
        let Some(mission) = self.selected_mission() else {
            return;
        };
        let mission_id = mission.mission_id.clone();
        self.start_job(
            Job::Resume {
                mission_id: mission_id.clone(),
            },
            format!("正在恢复 {mission_id} …"),
        );
    }

    fn do_delete(&mut self) {
        let Some((mission_id, brief)) = self
            .selected_mission()
            .map(|mission| (mission.mission_id.clone(), mission.brief.clone()))
        else {
            return;
        };
        if !self.confirm_delete {
            self.confirm_delete = true;
            self.message = format!(
                "再按一次 x 确认删除《{}》，按其他键取消",
                if brief.is_empty() {
                    mission_id.as_str()
                } else {
                    brief.as_str()
                }
            );
            return;
        }
        self.confirm_delete = false;
        self.start_job(
            Job::Delete {
                mission_id: mission_id.clone(),
            },
            format!("正在删除 {mission_id} …"),
        );
    }

    fn do_new(&mut self, title: &str) {
        let roles = match self.new_layout {
            MissionLayout::Team => ROLES
                .iter()
                .enumerate()
                .filter(|(index, _)| self.new_roles[*index])
                .map(|(_, role)| (*role).to_string())
                .collect(),
            MissionLayout::Simple => vec!["worker".to_string()],
        };
        if roles.is_empty() {
            self.message = "未选择任何角色，请至少保留一个。".to_string();
            return;
        }
        if self.new_workspace_source == WorkspaceSource::Import
            && self.new_worktree_path.trim().is_empty()
        {
            self.message = "导入 Worktree 需要填写目标路径。".to_string();
            return;
        }
        self.start_job(
            Job::New {
                title: title.to_string(),
                profile: self.new_profile,
                roles,
                launch_mode: self.new_launch_mode,
                workspace_source: self.new_workspace_source,
                worktree_path: self.new_worktree_path.trim().to_string(),
            },
            format!("正在创建 Mission《{title}》…"),
        );
    }

    fn do_send(&mut self, target: &str, body: &str) {
        let Some(mission) = self.selected_mission() else {
            return;
        };
        let mission_id = mission.mission_id.clone();
        self.start_job(
            Job::Send {
                mission_id,
                target: target.to_string(),
                body: body.to_string(),
            },
            format!("正在派发 task → {target}…"),
        );
    }

    fn render(&mut self, frame: &mut Frame) {
        let area = frame.area().inner(Margin::new(2, 1));
        let chunks = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(area);

        frame.render_widget(Paragraph::new(self.render_title()), chunks[0]);
        frame.render_widget(Paragraph::new(self.render_subtitle()), chunks[1]);

        match self.view {
            View::List => self.render_list_view(frame, chunks[2]),
            View::NewPrompt => self.render_new(frame, chunks[2]),
            View::SendForm => self.render_send(frame, chunks[2]),
            View::Help => self.render_help(frame, chunks[2]),
        }

        if let Some(label) = self.busy.clone() {
            let throbber = Throbber::default()
                .label(label)
                .style(Style::default().fg(Color::Yellow))
                .throbber_style(
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                )
                .throbber_set(throbber_widgets_tui::BRAILLE_EIGHT)
                .use_type(throbber_widgets_tui::WhichUse::Spin);
            frame.render_stateful_widget(throbber, chunks[3], &mut self.throbber_state);
        } else {
            frame.render_widget(
                Paragraph::new(self.message.as_str()).style(Style::default().fg(Color::Yellow)),
                chunks[3],
            );
        }

        frame.render_widget(
            Paragraph::new(self.render_footer()).style(Style::default().fg(Color::DarkGray)),
            chunks[4],
        );
    }

    fn render_title(&self) -> Line<'static> {
        let active = self
            .catalog
            .iter()
            .filter(|m| is_active_stage(&m.stage))
            .count();
        let ready = self.catalog.iter().filter(|m| m.stage == "ready").count();
        let mut spans = vec![Span::styled(
            "MISSION 控制中心",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )];
        spans.push(Span::raw("   "));
        spans.push(Span::styled(
            format!("执行 {active}"),
            Style::default().fg(Color::Yellow),
        ));
        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            format!("可交付 {ready}"),
            Style::default().fg(Color::Green),
        ));
        spans.push(Span::raw("   "));
        spans.push(Span::styled(
            format!("全部 {}", self.catalog.len()),
            Style::default().fg(Color::DarkGray),
        ));
        if !self.search.is_empty() {
            spans.push(Span::raw("   "));
            spans.push(Span::styled(
                format!("匹配 {}/{}", self.missions.len(), self.catalog.len()),
                Style::default().fg(Color::Cyan),
            ));
        }
        Line::from(spans)
    }

    fn render_subtitle(&self) -> Line<'static> {
        if self.searching {
            return Line::from(vec![Span::styled(
                format!("搜索 › {}█", self.search),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )]);
        }
        if !self.search.is_empty() {
            return Line::from(vec![Span::styled(
                format!("筛选 › {}", self.search),
                Style::default().fg(Color::Cyan),
            )]);
        }
        Line::from(Span::styled(
            "创建 Mission → 启动团队 → 派发 Task → 投递消息 → 恢复角色",
            Style::default().fg(Color::DarkGray),
        ))
    }

    fn render_footer(&self) -> String {
        if self.busy.is_some() {
            return "后台执行中…  [q]退出".to_string();
        }
        match self.view {
            View::List if self.searching => {
                "关键词筛选  [↑/↓]选择 [Enter]完成 [Esc]取消 [Ctrl-U]清空".to_string()
            }
            View::List => {
                "[/]搜索 [j/k]选择 [Enter]恢复 [n]新建 [s]派单 [d]投递 [c]自检 [x]删除 [?]帮助 [q]退出"
                    .to_string()
            }
            View::NewPrompt => {
                "[↑/↓]移动 [Tab/←/→]字段 [空格]选中/取消 [Enter]创建 [Esc]取消".to_string()
            }
            View::SendForm => "[Tab]切换目标角色  输入内容  [Enter]派发 [Esc]取消".to_string(),
            View::Help => "[?/Esc]返回 [q]退出".to_string(),
        }
    }

    fn render_list_view(&mut self, frame: &mut Frame, area: Rect) {
        if area.width >= 136 {
            let chunks =
                Layout::horizontal([Constraint::Percentage(48), Constraint::Percentage(52)])
                    .spacing(2)
                    .split(area);
            self.render_list(frame, chunks[0]);
            self.render_detail(frame, chunks[1]);
        } else {
            let desired_detail = if area.width < 72 { 15 } else { 14 };
            let detail_height = desired_detail.min(area.height.saturating_sub(3));
            let chunks = Layout::vertical([Constraint::Min(2), Constraint::Length(detail_height)])
                .spacing(1)
                .split(area);
            self.render_list(frame, chunks[0]);
            self.render_detail(frame, chunks[1]);
        }
    }

    fn render_list(&mut self, frame: &mut Frame, area: Rect) {
        if self.missions.is_empty() {
            let message = if self.search.is_empty() {
                "尚无 Mission。按 n 新建第一条团队交付任务。"
            } else {
                "没有匹配的 Mission。按 / 修改搜索。"
            };
            frame.render_widget(
                Paragraph::new(message).block(box_block().title(" Mission 队列 ")),
                area,
            );
            return;
        }

        let header = Row::new(["状态", "标题", "团队", "角色健康", "创建"]).style(
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        );
        let visible = (area.height as usize).saturating_sub(5).max(1);
        let len = self.missions.len();
        let start = self
            .selected
            .saturating_sub(visible / 2)
            .min(len.saturating_sub(visible));
        let end = (start + visible).min(len);

        let rows: Vec<Row> = self.missions[start..end]
            .iter()
            .map(|mission| {
                let color = stage_color(&mission.stage);
                let (health_text, health_color) = role_health_cell(&mission.roles);
                Row::new(vec![
                    Span::styled(
                        stage_label(&mission.stage).to_string(),
                        Style::default().fg(color),
                    ),
                    Span::raw(mission.brief.clone()),
                    Span::raw(profile_short(&mission.agent_profile_id)),
                    Span::styled(health_text, Style::default().fg(health_color)),
                    Span::raw(short_time(&mission.created_at)),
                ])
            })
            .collect();

        let title = format!(
            " Mission 队列 ({}/{}) ",
            self.missions.len(),
            self.catalog.len()
        );
        let table = Table::new(
            rows,
            [
                Constraint::Length(9),
                Constraint::Min(22),
                Constraint::Length(12),
                Constraint::Length(14),
                Constraint::Length(10),
            ],
        )
        .header(header)
        .block(box_block().title(title))
        .row_highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("> ")
        .column_spacing(1);

        self.table_state.select(Some(self.selected - start));
        frame.render_stateful_widget(table, area, &mut self.table_state);
    }

    fn render_detail(&self, frame: &mut Frame, area: Rect) {
        let Some(mission) = self.selected_mission() else {
            frame.render_widget(
                Paragraph::new("(无 Mission)").block(box_block().title(" 当前选择 ")),
                area,
            );
            return;
        };

        let stage_color = stage_color(&mission.stage);
        let detail_height = if area.width < 72 { 7 } else { 6 };
        let chunks = Layout::vertical([Constraint::Length(detail_height), Constraint::Min(5)])
            .spacing(1)
            .split(area);
        let label = Style::default().fg(Color::DarkGray);
        let metadata = vec![
            Line::from(vec![
                Span::styled("名称       ", label),
                Span::styled(
                    mission.brief.clone(),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
            ]),
            Line::from(vec![
                Span::styled("Mission ID ", label),
                Span::raw(mission.mission_id.clone()),
            ]),
            Line::from(vec![
                Span::styled("状态       ", label),
                Span::styled(
                    stage_label(&mission.stage).to_string(),
                    Style::default().fg(stage_color),
                ),
                Span::styled("   启动模式 ", label),
                Span::raw(mission.launch_mode.as_str()),
                Span::styled("   未结束任务 ", label),
                Span::raw(mission.pending_assignments.to_string()),
            ]),
            Line::from(vec![
                Span::styled("Profile    ", label),
                Span::raw(profile_short(&mission.agent_profile_id)),
                Span::styled("   Generation ", label),
                Span::raw(mission.generation.to_string()),
                Span::styled("   创建 ", label),
                Span::raw(short_time(&mission.created_at)),
            ]),
        ];
        frame.render_widget(
            Paragraph::new(metadata).wrap(Wrap { trim: false }).block(
                detail_block()
                    .title(" Mission 详情 ")
                    .border_style(Style::default().fg(stage_color)),
            ),
            chunks[0],
        );

        self.render_role_table(frame, chunks[1], mission);
    }

    fn render_role_table(&self, frame: &mut Frame, area: Rect, mission: &MissionOverview) {
        let wide = area.width >= 88;
        let header_style = Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::BOLD);
        let rows = mission.roles.iter().map(|role| {
            let (glyph, color) = health_glyph(&role.health);
            let state = Cell::from(Line::from(vec![
                Span::styled(format!("{glyph} "), Style::default().fg(color)),
                Span::styled(
                    health_label(&role.health).to_string(),
                    Style::default().fg(color),
                ),
            ]));
            let role_name = Cell::from(role_short_label(&role.role).to_string());
            let agent = Cell::from(value_or_dash(&role.agent_name));
            let pane = Cell::from(value_or_dash(&role.pane_id));
            if wide {
                Row::new(vec![
                    state,
                    role_name,
                    Cell::from(provider_model(role)),
                    agent,
                    pane,
                ])
            } else {
                Row::new(vec![state, role_name, agent, pane])
            }
        });

        let (header, widths) = if wide {
            (
                Row::new(["状态", "角色", "Provider / Model", "Agent", "Pane"]).style(header_style),
                vec![
                    Constraint::Length(10),
                    Constraint::Length(10),
                    Constraint::Length(18),
                    Constraint::Min(18),
                    Constraint::Length(12),
                ],
            )
        } else {
            (
                Row::new(["状态", "角色", "Agent", "Pane"]).style(header_style),
                vec![
                    Constraint::Length(8),
                    Constraint::Length(8),
                    Constraint::Min(20),
                    Constraint::Length(8),
                ],
            )
        };
        let table = Table::new(rows, widths)
            .header(header)
            .block(detail_block().title(" 角色 "))
            .column_spacing(1);
        frame.render_widget(table, area);
    }

    fn render_new(&self, frame: &mut Frame, area: Rect) {
        let focus = |field: FormField| {
            if self.new_field == field {
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::DarkGray)
            }
        };
        let selected = Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD);
        let muted = Style::default().fg(Color::DarkGray);

        let mut lines = vec![Line::from(Span::styled(
            "新建 Mission",
            Style::default().add_modifier(Modifier::BOLD),
        ))];
        lines.push(Line::from(Span::styled("▶ 布局", focus(FormField::Layout))));
        for (idx, (label, desc)) in [
            ("团队", "PM / Worker / Scout / Reviewer"),
            ("单 Agent", "单个 Worker，独立推进"),
        ]
        .iter()
        .enumerate()
        {
            let is_selected = layout_at(idx) == self.new_layout;
            let is_cursor = self.new_field == FormField::Layout && self.layout_cursor == idx;
            lines.push(option_line(
                &format!("{label}    {desc}"),
                is_selected,
                is_cursor,
                selected,
                muted,
            ));
        }

        if self.new_layout == MissionLayout::Team {
            lines.push(Line::from(Span::styled(
                "▶ 启动模式",
                focus(FormField::LaunchMode),
            )));
            for (idx, (mode, label)) in [
                (LaunchMode::Auto, "Auto    立即启动全部角色"),
                (LaunchMode::Manual, "Manual  仅启动 PM，其他角色按需"),
            ]
            .iter()
            .enumerate()
            {
                let is_selected = *mode == self.new_launch_mode;
                let is_cursor =
                    self.new_field == FormField::LaunchMode && self.launch_mode_cursor == idx;
                lines.push(option_line(label, is_selected, is_cursor, selected, muted));
            }
        }

        lines.push(Line::from(Span::styled(
            "▶ 工作区",
            focus(FormField::Workspace),
        )));
        for (idx, source) in [
            WorkspaceSource::Current,
            WorkspaceSource::Worktree,
            WorkspaceSource::Import,
        ]
        .iter()
        .enumerate()
        {
            let is_selected = *source == self.new_workspace_source;
            let is_cursor = self.new_field == FormField::Workspace && self.workspace_cursor == idx;
            lines.push(option_line(
                source.label(),
                is_selected,
                is_cursor,
                selected,
                muted,
            ));
        }
        if self.new_workspace_source == WorkspaceSource::Import {
            let path_cursor = if self.new_field == FormField::WorktreePath {
                "█"
            } else {
                ""
            };
            lines.push(Line::from(Span::styled(
                "▶ 目标路径",
                focus(FormField::WorktreePath),
            )));
            lines.push(Line::from(Span::styled(
                format!("  {}{}", self.new_worktree_path, path_cursor),
                if self.new_field == FormField::WorktreePath {
                    selected
                } else {
                    muted
                },
            )));
        }

        lines.push(Line::from(Span::styled(
            "▶ 模型",
            focus(FormField::Profile),
        )));
        for (idx, provider) in Provider::ALL.iter().enumerate() {
            let is_selected = *provider == self.new_profile;
            let is_cursor = self.new_field == FormField::Profile && self.profile_cursor == idx;
            lines.push(option_line(
                provider.label(),
                is_selected,
                is_cursor,
                selected,
                muted,
            ));
        }

        if self.new_layout == MissionLayout::Team {
            lines.push(Line::from(Span::styled("▶ 角色", focus(FormField::Roles))));
            for (idx, role) in ROLES.iter().enumerate() {
                let is_selected = self.new_roles[idx];
                let is_cursor = self.new_field == FormField::Roles && self.new_role_idx == idx;
                lines.push(option_line(role, is_selected, is_cursor, selected, muted));
            }
        }
        lines.push(Line::from(Span::styled("▶ 标题", focus(FormField::Title))));
        let title_cursor = if self.new_field == FormField::Title {
            "█"
        } else {
            ""
        };
        lines.push(Line::from(Span::styled(
            format!("  {}{}", self.input, title_cursor),
            if self.new_field == FormField::Title {
                selected
            } else {
                muted
            },
        )));
        frame.render_widget(
            Paragraph::new(lines).block(box_block().title(" 新建 ")),
            area,
        );
    }

    fn render_send(&self, frame: &mut Frame, area: Rect) {
        let target = ROLES[self.send_role];
        let lines = vec![
            Line::from(Span::styled(
                "派发 Task",
                Style::default().add_modifier(Modifier::BOLD),
            )),
            Line::from(Span::raw("")),
            Line::from(Span::raw(format!(
                "来源: pm   →   目标: {}",
                role_short_label(target)
            ))),
            Line::from(Span::raw("")),
            Line::from(Span::styled(
                format!("内容: {}█", self.input),
                Style::default().fg(Color::Cyan),
            )),
        ];
        frame.render_widget(
            Paragraph::new(lines).block(box_block().title(" 派单 ")),
            area,
        );
    }

    fn render_help(&self, frame: &mut Frame, area: Rect) {
        let rows: Vec<&str> = vec![
            "/      搜索（多关键词，实时筛选标题/阶段/团队/角色）",
            "j/k    选择  ·  Enter 或 r 恢复团队角色",
            "n      新建 Team Mission（PM/Worker/Scout/Reviewer）",
            "s      给 pm/worker/scout/reviewer 派发 Task",
            "d      投递待处理消息（deliver）",
            "c      自检数据库与 schema（doctor）",
            "x      删除选中的 Mission（再按一次 x 确认）",
            "?      显示本帮助",
            "q/Esc  退出",
        ];
        frame.render_widget(
            Paragraph::new(rows.join("\n")).block(box_block().title(" 操作说明 ")),
            area,
        );
    }
}

fn box_block() -> Block<'static> {
    Block::bordered().padding(Padding::new(1, 1, 1, 1))
}

fn detail_block() -> Block<'static> {
    Block::bordered().padding(Padding::new(1, 1, 0, 0))
}

fn form_field_cycle(layout: MissionLayout, source: WorkspaceSource) -> &'static [FormField] {
    match (layout, source) {
        (MissionLayout::Team, WorkspaceSource::Import) => &[
            FormField::Layout,
            FormField::LaunchMode,
            FormField::Workspace,
            FormField::WorktreePath,
            FormField::Profile,
            FormField::Roles,
            FormField::Title,
        ],
        (MissionLayout::Simple, WorkspaceSource::Import) => &[
            FormField::Layout,
            FormField::Workspace,
            FormField::WorktreePath,
            FormField::Profile,
            FormField::Title,
        ],
        (MissionLayout::Team, _) => &[
            FormField::Layout,
            FormField::LaunchMode,
            FormField::Workspace,
            FormField::Profile,
            FormField::Roles,
            FormField::Title,
        ],
        (MissionLayout::Simple, _) => &[
            FormField::Layout,
            FormField::Workspace,
            FormField::Profile,
            FormField::Title,
        ],
    }
}

fn next_form_field(field: FormField, layout: MissionLayout, source: WorkspaceSource) -> FormField {
    let cycle = form_field_cycle(layout, source);
    let index = cycle.iter().position(|item| *item == field).unwrap_or(0);
    cycle[(index + 1) % cycle.len()]
}

fn prev_form_field(field: FormField, layout: MissionLayout, source: WorkspaceSource) -> FormField {
    let cycle = form_field_cycle(layout, source);
    let index = cycle.iter().position(|item| *item == field).unwrap_or(0);
    cycle[(index + cycle.len() - 1) % cycle.len()]
}

fn move_index(index: usize, len: usize, delta: isize) -> usize {
    let base = index as isize;
    ((base + delta).rem_euclid(len as isize)) as usize
}

fn layout_at(index: usize) -> MissionLayout {
    const ALL: [MissionLayout; 2] = [MissionLayout::Team, MissionLayout::Simple];
    ALL[index % ALL.len()]
}

fn launch_mode_at(index: usize) -> LaunchMode {
    LaunchMode::ALL[index % LaunchMode::ALL.len()]
}

fn profile_at(index: usize) -> Provider {
    Provider::ALL[index % Provider::ALL.len()]
}

fn workspace_source_at(index: usize) -> WorkspaceSource {
    const ALL: [WorkspaceSource; 3] = [
        WorkspaceSource::Current,
        WorkspaceSource::Worktree,
        WorkspaceSource::Import,
    ];
    ALL[index % ALL.len()]
}

fn option_line(
    label: &str,
    selected: bool,
    cursor: bool,
    selected_style: Style,
    muted: Style,
) -> Line<'static> {
    let style = if cursor { selected_style } else { muted };
    let mark = if selected { "●" } else { "○" };
    let arrow = if cursor { "▶" } else { " " };
    Line::from(vec![
        Span::styled(format!(" {arrow} "), style),
        Span::styled(format!("{mark} "), style),
        Span::styled(label.to_string(), style),
    ])
}

fn stage_label(stage: &str) -> &str {
    match stage {
        "preparing" => "准备中",
        "active" => "执行中",
        "blocked" => "阻塞",
        "review" => "待审查",
        "verifying" => "验证中",
        "ready" => "可交付",
        "draft" => "草稿",
        "archived" => "已归档",
        other => other,
    }
}

fn stage_color(stage: &str) -> Color {
    match stage {
        "preparing" | "verifying" => Color::Yellow,
        "active" => Color::Cyan,
        "blocked" => Color::Red,
        "review" => Color::Magenta,
        "ready" => Color::Green,
        "draft" | "archived" => Color::DarkGray,
        _ => Color::White,
    }
}

fn is_active_stage(stage: &str) -> bool {
    matches!(
        stage,
        "preparing" | "active" | "blocked" | "review" | "verifying"
    )
}

fn health_label(health: &str) -> &str {
    match health {
        "working" | "running" => "运行中",
        "idle" => "空闲",
        "blocked" => "阻塞",
        "done" => "已完成",
        "restorable" => "可恢复",
        "exited" => "已退出",
        "missing" => "缺失",
        "unbound" => "未绑定",
        "unknown" => "未知",
        other => other,
    }
}

fn role_short_label(role: &str) -> &str {
    match role {
        "pm" => "PM",
        "worker" => "Worker",
        "scout" => "Scout",
        "reviewer" => "Reviewer",
        other => other,
    }
}

fn profile_short(profile: &str) -> String {
    match profile {
        "codex-default-v1" => "codex".to_string(),
        "pi-quality-v1" => "pi".to_string(),
        other => other.to_string(),
    }
}

fn short_time(timestamp: &str) -> String {
    if timestamp.len() >= 19 {
        timestamp[11..19].to_string()
    } else if timestamp.is_empty() {
        "-".to_string()
    } else {
        timestamp.to_string()
    }
}

fn role_health_cell(roles: &[RoleOverview]) -> (String, Color) {
    if roles.is_empty() {
        return ("-".to_string(), Color::DarkGray);
    }
    let total = roles.len();
    let running = roles
        .iter()
        .filter(|role| matches!(role.health.as_str(), "working" | "running"))
        .count();
    let broken = roles
        .iter()
        .filter(|role| matches!(role.health.as_str(), "missing" | "blocked"))
        .count();
    let color = if broken > 0 {
        Color::Red
    } else if running == total {
        Color::Green
    } else if running > 0 {
        Color::Yellow
    } else {
        Color::DarkGray
    };
    (format!("{running}/{total} 运行"), color)
}

fn health_glyph(health: &str) -> (&'static str, Color) {
    match health {
        "working" | "running" => ("●", Color::Green),
        "idle" => ("○", Color::DarkGray),
        "blocked" => ("!", Color::Red),
        "done" => ("✓", Color::Green),
        "restorable" => ("◐", Color::Yellow),
        "exited" => ("◌", Color::DarkGray),
        "missing" => ("✕", Color::Red),
        "unbound" => ("-", Color::DarkGray),
        "unknown" => ("?", Color::DarkGray),
        _ => ("?", Color::DarkGray),
    }
}

fn provider_model(role: &RoleOverview) -> String {
    if role.model.is_empty() {
        role.provider.clone()
    } else {
        format!("{}/{}", role.provider, role.model)
    }
}

fn value_or_dash(value: &str) -> String {
    if value.is_empty() {
        "-".to_string()
    } else {
        value.to_string()
    }
}

fn mission_search_text(mission: &MissionOverview) -> String {
    let mut fields = vec![
        mission.brief.clone(),
        mission.mission_id.clone(),
        mission.stage.clone(),
        stage_label(&mission.stage).to_string(),
        mission.agent_profile_id.clone(),
        profile_short(&mission.agent_profile_id),
    ];
    for role in &mission.roles {
        fields.push(role.role.clone());
        fields.push(role.provider.clone());
        fields.push(role.model.clone());
        fields.push(role.thinking.clone());
        fields.push(role.health.clone());
        fields.push(health_label(&role.health).to_string());
        fields.push(role.agent_name.clone());
    }
    fields.join(" ").to_lowercase()
}

fn error_line(error: &KernelError) -> String {
    let base = if error.code.is_empty() {
        error.message.clone()
    } else {
        format!("{} ({})", error.message, error.code)
    };
    let operation = error
        .details
        .get("operation")
        .and_then(|value| value.as_str());
    let reason = error
        .details
        .get("reason")
        .and_then(|value| value.as_str())
        .map(str::to_owned)
        .or_else(|| {
            error
                .details
                .get("stderr")
                .and_then(|value| value.as_str())
                .and_then(structured_error_message)
        });
    match (operation, reason) {
        (Some(operation), Some(reason)) => format!("{base} · {operation}: {reason}"),
        (Some(operation), None) => format!("{base} · {operation}"),
        (None, Some(reason)) => format!("{base} · {reason}"),
        (None, None) => base,
    }
}

fn structured_error_message(raw: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(raw)
        .ok()
        .and_then(|value| {
            value
                .get("error")
                .and_then(|error| error.get("message"))
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        })
        .or_else(|| {
            let trimmed = raw.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_string())
        })
}

fn launch_options(
    workspace_source: WorkspaceSource,
    worktree_path: Option<String>,
) -> LaunchOptions {
    LaunchOptions {
        direction: "right".into(),
        cwd: source_cwd(),
        prompts_dir: None,
        tab_mode: LaunchConfig::load().launch.tab_mode,
        workspace_source,
        worktree_path,
    }
}

fn run_job(database: &str, job: Job) -> JobOutcome {
    let path = Path::new(database);
    let herdr = herdr_bin();
    let runner = SystemProcessRunner;
    let message = match job {
        Job::Deliver => match kernel_deliver(path, &runner, &herdr) {
            Ok(report) => format!(
                "投递完成：delivered={} failed={}",
                report.delivered, report.failed
            ),
            Err(error) => error_line(&error),
        },
        Job::Resume { mission_id } => {
            let options = launch_options(WorkspaceSource::Current, None);
            match launch_mission(path, &mission_id, &options, &runner, &herdr, &mut |_| {}) {
                Ok(launch) => format!("已恢复 {mission_id} → {}", launch.stage),
                Err(error) => format!("恢复失败：{}", error_line(&error)),
            }
        }
        Job::New {
            title,
            profile,
            roles,
            launch_mode,
            workspace_source,
            worktree_path,
        } => {
            let request = build_new_mission_request(&title, profile, &roles, launch_mode);
            match create_mission(path, &request) {
                Ok(outcome) => {
                    let worktree_target = if worktree_path.is_empty() {
                        None
                    } else {
                        Some(worktree_path)
                    };
                    let options = launch_options(workspace_source, worktree_target);
                    match launch_mission(
                        path,
                        &outcome.mission_id,
                        &options,
                        &runner,
                        &herdr,
                        &mut |_| {},
                    ) {
                        Ok(launch) => format!("Mission 已创建并进入 {}", launch.stage),
                        Err(error) => format!("启动失败：{}", error_line(&error)),
                    }
                }
                Err(error) => {
                    crate::log_error(
                        path,
                        &format!("mission={} create failed", request.mission_id),
                        &error,
                    );
                    error_line(&error)
                }
            }
        }
        Job::Send {
            mission_id,
            target,
            body,
        } => match kernel_dispatch_command(path, &mission_id, "pm", &target, "task", &body) {
            Ok(outcome) => match outcome.assignment_id {
                Some(id) => format!("已派发 task → {target}：assignment {id}"),
                None => format!("已发送通知 → {target}"),
            },
            Err(error) => error_line(&error),
        },
        Job::Delete { mission_id } => match delete_mission(path, &mission_id) {
            Ok(outcome) => {
                let workspace_closed = match &outcome.workspace_id {
                    Some(workspace_id) if !workspace_id.is_empty() => matches!(
                        runner.run(&herdr, &workspace_close_argv(workspace_id)),
                        Ok(output) if output.exit_code == 0
                    ),
                    _ => false,
                };
                if outcome.deleted {
                    let mut text = format!("已删除 {mission_id}");
                    if let Some(workspace_id) = &outcome.workspace_id {
                        if workspace_closed {
                            text.push_str(&format!("，已关闭 workspace {workspace_id}"));
                        } else {
                            text.push_str(&format!("（workspace {workspace_id} 关闭失败）"));
                        }
                    }
                    text
                } else {
                    format!("(no mission {mission_id})")
                }
            }
            Err(error) => error_line(&error),
        },
    };
    JobOutcome { message }
}

fn build_new_mission_request(
    title: &str,
    profile: Provider,
    roles: &[String],
    launch_mode: LaunchMode,
) -> CreateMissionRequest {
    CreateMissionRequest {
        mission_id: make_mission_id(title),
        brief: title.to_string(),
        template: "general".into(),
        agent_profile_id: profile.profile_id(),
        agent_profile_version: profile.profile_version(),
        launch_mode,
        roles: roles.iter().map(|role| profile.role_config(role)).collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{backend::TestBackend, Terminal};

    fn role(role: &str, health: &str) -> RoleOverview {
        RoleOverview {
            role: role.to_string(),
            provider: "codex".to_string(),
            model: String::new(),
            thinking: String::new(),
            health: health.to_string(),
            pane_id: String::new(),
            agent_name: String::new(),
        }
    }

    fn overview(id: &str, brief: &str, stage: &str, roles: Vec<RoleOverview>) -> MissionOverview {
        MissionOverview {
            mission_id: id.to_string(),
            brief: brief.to_string(),
            stage: stage.to_string(),
            launch_mode: LaunchMode::Manual,
            created_at: "2026-08-15T00:00:00Z".to_string(),
            agent_profile_id: "codex-default-v1".to_string(),
            roles,
            pending_assignments: 0,
            generation: 1,
        }
    }

    fn make_app(catalog: Vec<MissionOverview>, search: &str) -> App {
        let mut app = App {
            database: "/tmp/x.sqlite3".to_string(),
            catalog,
            missions: Vec::new(),
            selected: 0,
            table_state: TableState::default(),
            view: View::List,
            search: search.to_string(),
            searching: false,
            input: String::new(),
            send_role: 1,
            new_layout: MissionLayout::Team,
            new_launch_mode: LaunchMode::Manual,
            new_workspace_source: WorkspaceSource::Current,
            new_worktree_path: String::new(),
            new_profile: Provider::Codex,
            layout_cursor: 0,
            launch_mode_cursor: 1,
            workspace_cursor: 0,
            profile_cursor: 0,
            new_field: FormField::Layout,
            new_roles: [true; 4],
            new_role_idx: 0,
            message: String::new(),
            busy: None,
            throbber_state: ThrobberState::default(),
            job_rx: None,
            confirm_delete: false,
            should_quit: false,
        };
        app.apply_search();
        app
    }

    #[test]
    fn search_matches_title_and_stage_label() {
        let catalog = vec![
            overview("msn-a", "集成验证", "active", vec![role("pm", "running")]),
            overview("msn-b", "其他任务", "blocked", vec![]),
        ];
        let app = make_app(catalog, "集成");
        assert_eq!(app.missions.len(), 1);
        assert_eq!(app.missions[0].mission_id, "msn-a");

        let catalog = vec![
            overview("msn-a", "集成验证", "active", vec![]),
            overview("msn-b", "其他任务", "blocked", vec![]),
        ];
        let app = make_app(catalog, "阻塞");
        assert_eq!(app.missions.len(), 1);
        assert_eq!(app.missions[0].mission_id, "msn-b");
    }

    #[test]
    fn search_matches_role_provider() {
        let catalog = vec![
            overview(
                "msn-a",
                "a",
                "active",
                vec![RoleOverview {
                    role: "pm".to_string(),
                    provider: "pi".to_string(),
                    model: String::new(),
                    thinking: String::new(),
                    health: "running".to_string(),
                    pane_id: String::new(),
                    agent_name: String::new(),
                }],
            ),
            overview("msn-b", "b", "active", vec![]),
        ];
        let app = make_app(catalog, "pi");
        assert_eq!(app.missions.len(), 1);
        assert_eq!(app.missions[0].mission_id, "msn-a");
    }

    #[test]
    fn move_selection_wraps_around() {
        let mut app = make_app(
            vec![
                overview("msn-a", "a", "active", vec![]),
                overview("msn-b", "b", "active", vec![]),
            ],
            "",
        );
        app.move_selection(-1);
        assert_eq!(app.selected, 1);
        app.move_selection(1);
        assert_eq!(app.selected, 0);
    }

    #[test]
    fn short_time_extracts_clock() {
        assert_eq!(short_time("2026-08-15T23:45:01Z"), "23:45:01");
        assert_eq!(short_time(""), "-");
    }

    #[test]
    fn form_field_navigation_wraps() {
        assert_eq!(
            next_form_field(
                FormField::Layout,
                MissionLayout::Team,
                WorkspaceSource::Current
            ),
            FormField::LaunchMode
        );
        assert_eq!(
            next_form_field(
                FormField::LaunchMode,
                MissionLayout::Team,
                WorkspaceSource::Current
            ),
            FormField::Workspace
        );
        assert_eq!(
            next_form_field(
                FormField::Workspace,
                MissionLayout::Team,
                WorkspaceSource::Current
            ),
            FormField::Profile
        );
        assert_eq!(
            next_form_field(
                FormField::Profile,
                MissionLayout::Team,
                WorkspaceSource::Current
            ),
            FormField::Roles
        );
        assert_eq!(
            next_form_field(
                FormField::Roles,
                MissionLayout::Team,
                WorkspaceSource::Current
            ),
            FormField::Title
        );
        assert_eq!(
            next_form_field(
                FormField::Title,
                MissionLayout::Team,
                WorkspaceSource::Current
            ),
            FormField::Layout
        );
        // Simple layout skips the Roles field.
        assert_eq!(
            next_form_field(
                FormField::Layout,
                MissionLayout::Simple,
                WorkspaceSource::Current
            ),
            FormField::Workspace
        );
        assert_eq!(
            next_form_field(
                FormField::Workspace,
                MissionLayout::Simple,
                WorkspaceSource::Current
            ),
            FormField::Profile
        );
        assert_eq!(
            next_form_field(
                FormField::Profile,
                MissionLayout::Simple,
                WorkspaceSource::Current
            ),
            FormField::Title
        );
        assert_eq!(
            prev_form_field(
                FormField::Layout,
                MissionLayout::Team,
                WorkspaceSource::Current
            ),
            FormField::Title
        );
        // Import layout inserts the target-path field.
        assert_eq!(
            next_form_field(
                FormField::Workspace,
                MissionLayout::Simple,
                WorkspaceSource::Import
            ),
            FormField::WorktreePath
        );
    }

    #[test]
    fn form_option_indexes_and_cursor_wrap() {
        assert_eq!(layout_at(0), MissionLayout::Team);
        assert_eq!(layout_at(1), MissionLayout::Simple);
        assert_eq!(launch_mode_at(0), LaunchMode::Auto);
        assert_eq!(launch_mode_at(1), LaunchMode::Manual);
        assert_eq!(profile_at(0), Provider::Codex);
        assert_eq!(profile_at(1), Provider::Pi);
        assert_eq!(profile_at(Provider::ALL.len() - 1), Provider::Droid);
        assert_eq!(workspace_source_at(0), WorkspaceSource::Current);
        assert_eq!(workspace_source_at(1), WorkspaceSource::Worktree);
        assert_eq!(workspace_source_at(2), WorkspaceSource::Import);
        assert_eq!(move_index(0, 3, -1), 2);
        assert_eq!(move_index(2, 3, 1), 0);
    }

    #[test]
    fn launch_mode_option_updates_the_single_mission_selection() {
        let mut app = make_app(vec![], "");
        app.new_field = FormField::LaunchMode;
        app.launch_mode_cursor = 0;
        app.toggle_option();
        assert_eq!(app.new_launch_mode, LaunchMode::Auto);

        app.launch_mode_cursor = 1;
        app.toggle_option();
        assert_eq!(app.new_launch_mode, LaunchMode::Manual);
    }

    #[test]
    fn new_job_request_preserves_the_tui_launch_mode() {
        let request = build_new_mission_request(
            "Auto from TUI",
            Provider::Codex,
            &["pm".to_string(), "worker".to_string()],
            LaunchMode::Auto,
        );
        assert_eq!(request.launch_mode, LaunchMode::Auto);
    }

    #[test]
    fn error_line_includes_operation_and_structured_herdr_reason() {
        let error = KernelError {
            category: crate::ErrorCategory::Infrastructure,
            code: "mission_region_unavailable".into(),
            message: "Mission region is unavailable in the current Herdr session".into(),
            retryable: false,
            details: std::collections::BTreeMap::from([
                ("operation".into(), serde_json::json!("tab get")),
                (
                    "stderr".into(),
                    serde_json::json!(
                        r#"{"error":{"code":"tab_not_found","message":"tab w78:t1 not found"}}"#
                    ),
                ),
            ]),
        };

        let line = error_line(&error);
        assert!(line.contains("mission_region_unavailable"), "{line}");
        assert!(line.contains("tab get"), "{line}");
        assert!(line.contains("tab w78:t1 not found"), "{line}");
    }

    #[test]
    fn live_role_healths_have_explicit_labels_glyphs_and_running_counts() {
        assert_eq!(health_label("working"), "运行中");
        assert_eq!(health_label("blocked"), "阻塞");
        assert_eq!(health_label("done"), "已完成");
        assert_eq!(health_label("missing"), "缺失");
        assert_eq!(health_glyph("working").0, "●");
        assert_eq!(health_glyph("blocked").0, "!");
        assert_eq!(health_glyph("done").0, "✓");
        assert_eq!(health_glyph("missing").0, "✕");

        let (summary, _) = role_health_cell(&[
            role("pm", "working"),
            role("worker", "running"),
            role("reviewer", "idle"),
        ]);
        assert_eq!(summary, "2/3 运行");
    }

    #[test]
    fn selected_mission_renders_separate_metadata_and_role_table_regions() {
        let mission_id = "msn-20260827-082057-rust-version-0b801f50";
        let mut mission = overview(
            mission_id,
            "rust-version",
            "active",
            vec![role("reviewer", "working")],
        );
        mission.pending_assignments = 1;
        mission.roles[0].agent_name = "mission-rust-version-reviewer".into();
        mission.roles[0].pane_id = "w16:p6".into();
        let app = make_app(vec![mission], "");

        let rendered = render_detail_text(&app, 88, 16);
        assert!(rendered.contains("Mission 详 情"), "{rendered}");
        assert!(rendered.contains(mission_id));
        assert!(rendered.contains("角 色"));
        assert!(rendered.contains("状 态"), "{rendered}");
        assert!(rendered.contains("Provider / Model"));
        assert!(rendered.contains("Agent"));
        assert!(rendered.contains("Pane"));
        assert!(rendered.contains("mission-rust-version-reviewer"));
        assert!(rendered.contains("w16:p6"));
    }

    #[test]
    fn compact_role_table_keeps_status_role_agent_and_pane_headers() {
        let mut mission = overview(
            "msn-20260827-082057-rust-version-0b801f50",
            "rust-version",
            "active",
            vec![role("reviewer", "working")],
        );
        mission.roles[0].agent_name = "mission-rust-version-reviewer".into();
        mission.roles[0].pane_id = "w16:p6".into();
        let app = make_app(vec![mission], "");

        let rendered = render_detail_text(&app, 60, 16);
        assert!(rendered.contains("状 态"), "{rendered}");
        assert!(rendered.contains("角 色"));
        assert!(rendered.contains("Agent"));
        assert!(rendered.contains("Pane"));
        assert!(!rendered.contains("Provider / Model"));
        assert!(rendered.contains("mission-rust-version-reviewer"));
        assert!(rendered.contains("w16:p6"));
    }

    #[test]
    fn full_dashboard_at_96_by_24_keeps_complete_metadata_and_all_roles() {
        let mission_id = "msn-20260827-082057-rust-version-0b801f50";
        let mut roles = vec![
            role("pm", "idle"),
            role("worker", "idle"),
            role("scout", "idle"),
            role("reviewer", "working"),
        ];
        for (index, role) in roles.iter_mut().enumerate() {
            role.agent_name = format!("mission-rust-version-{}", role.role);
            role.pane_id = format!("w16:p{}", index + 1);
        }
        let mut mission = overview(mission_id, "rust-version", "active", roles);
        mission.pending_assignments = 1;
        let mut app = make_app(vec![mission], "");

        let rendered = render_app_text(&mut app, 96, 24);
        assert!(rendered.contains(mission_id), "{rendered}");
        assert!(rendered.contains("Generation"), "{rendered}");
        assert!(rendered.contains("PM"), "{rendered}");
        assert!(rendered.contains("Worker"), "{rendered}");
        assert!(rendered.contains("Scout"), "{rendered}");
        assert!(rendered.contains("Reviewer"), "{rendered}");
    }

    #[test]
    fn native_text_selection_disables_terminal_mouse_capture() {
        let mut output = Vec::new();
        enable_native_text_selection(&mut output).unwrap();
        let escape = String::from_utf8(output).unwrap();
        assert!(escape.contains("?1000l"));
        assert!(escape.contains("?1002l"));
        assert!(escape.contains("?1003l"));
        assert!(escape.contains("?1006l"));
    }

    fn render_detail_text(app: &App, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        let frame = terminal
            .draw(|frame| app.render_detail(frame, frame.area()))
            .unwrap();
        frame
            .buffer
            .content()
            .chunks(usize::from(width))
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn render_app_text(app: &mut App, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        let frame = terminal.draw(|frame| app.render(frame)).unwrap();
        frame
            .buffer
            .content()
            .chunks(usize::from(width))
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }
}
