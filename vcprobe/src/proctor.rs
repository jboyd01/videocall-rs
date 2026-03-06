use std::io::{self, Stdout};
use std::time::Duration;

use anyhow::Result;
use chrono::{Local, Utc};
use crossterm::{
    event::{Event, EventStream, KeyCode, KeyEvent, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use futures_util::StreamExt;
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState},
    Frame, Terminal,
};
use tokio::sync::mpsc;
use tokio::time;

use crate::state::{MeetingState, Participant, QualitySnapshot};

// ── Terminal setup / teardown ─────────────────────────────────────────────────

fn setup_terminal() -> Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    Ok(Terminal::new(backend)?)
}

fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    Ok(())
}

// ── Entry point ───────────────────────────────────────────────────────────────

pub async fn run(
    mut rx: mpsc::Receiver<Vec<u8>>,
    meeting_id: String,
    use_utc: bool,
) -> Result<()> {
    let mut terminal = setup_terminal()?;
    let result = run_inner(&mut terminal, &mut rx, meeting_id, use_utc).await;
    restore_terminal(&mut terminal)?;
    result
}

async fn run_inner(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    rx: &mut mpsc::Receiver<Vec<u8>>,
    meeting_id: String,
    use_utc: bool,
) -> Result<()> {
    let mut state = MeetingState::new(meeting_id);
    let mut table_state = TableState::default();
    let mut event_stream = EventStream::new();
    let mut render_tick = time::interval(Duration::from_millis(100));
    let mut stale_tick = time::interval(Duration::from_secs(1));
    let mut show_help = false;
    let mut sort_by_quality = false; // Default to alphabetical

    loop {
        tokio::select! {
            // Incoming packet from the meeting
            maybe_raw = rx.recv() => {
                match maybe_raw {
                    Some(raw) => state.process_packet(&raw),
                    None => break, // connection closed
                }
            }

            // Keyboard input
            maybe_event = event_stream.next() => {
                match maybe_event {
                    Some(Ok(Event::Key(key))) => {
                        if handle_key(key, &mut table_state, &state, &mut show_help, &mut sort_by_quality) {
                            break;
                        }
                    }
                    Some(Err(_)) | None => break,
                    _ => {}
                }
            }

            // Prune stale participants
            _ = stale_tick.tick() => {
                state.tick();
                clamp_selection(&mut table_state, state.participants.len());
            }

            // Render
            _ = render_tick.tick() => {
                terminal.draw(|f| render(f, &state, &mut table_state, use_utc, show_help, sort_by_quality))?;
            }
        }
    }

    Ok(())
}

fn handle_key(
    key: KeyEvent,
    table_state: &mut TableState,
    state: &MeetingState,
    show_help: &mut bool,
    sort_by_quality: &mut bool,
) -> bool {
    match key.code {
        KeyCode::Char('q') => return true,
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => return true,
        KeyCode::Esc => return true,
        KeyCode::Char('h') | KeyCode::Char('?') => {
            *show_help = !*show_help;
        }
        KeyCode::Char('s') | KeyCode::Char('S') => {
            *sort_by_quality = !*sort_by_quality;
            // Reset selection to top when sort changes
            if !state.participants.is_empty() {
                table_state.select(Some(0));
            }
        }
        KeyCode::Down | KeyCode::Char('j') => {
            let n = state.participants.len();
            if n > 0 {
                let next = match table_state.selected() {
                    Some(i) => (i + 1).min(n - 1),
                    None => 0,
                };
                table_state.select(Some(next));
            }
        }
        KeyCode::Up | KeyCode::Char('k') => {
            let prev = match table_state.selected() {
                Some(0) | None => 0,
                Some(i) => i - 1,
            };
            table_state.select(Some(prev));
        }
        KeyCode::Home => table_state.select(Some(0)),
        KeyCode::End => {
            let n = state.participants.len();
            if n > 0 {
                table_state.select(Some(n - 1));
            }
        }
        _ => {}
    }
    false
}

fn clamp_selection(table_state: &mut TableState, len: usize) {
    if let Some(i) = table_state.selected() {
        if len == 0 {
            table_state.select(None);
        } else if i >= len {
            table_state.select(Some(len - 1));
        }
    }
}

// ── Top-level render ──────────────────────────────────────────────────────────

fn render(
    f: &mut Frame,
    state: &MeetingState,
    table_state: &mut TableState,
    use_utc: bool,
    show_help: bool,
    sort_by_quality: bool,
) {
    let area = f.area();

    // Require minimum terminal size to avoid panics from zero-height areas
    if area.height < 10 || area.width < 40 {
        let msg = Paragraph::new("Terminal too small — resize to continue")
            .style(Style::default().fg(Color::Yellow));
        f.render_widget(msg, area);
        return;
    }

    let participants = state.sorted_participants(sort_by_quality);

    // Adaptive detail height: show it only when someone is selected
    let detail_h = if table_state.selected().is_some() { 7u16 } else { 0u16 };

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(4),                           // header (was 3, now 4 for 2-row headers)
            Constraint::Min(4),                              // participant table
            Constraint::Length(detail_h),                    // detail panel
            Constraint::Length(5),                           // events
            Constraint::Length(1),                           // footer
        ])
        .split(area);

    render_header(f, chunks[0], state, use_utc, sort_by_quality);
    render_participants(f, chunks[1], state, &participants, table_state);
    if detail_h > 0 {
        render_detail(f, chunks[2], &participants, table_state.selected());
    }
    render_events(f, chunks[3], state, use_utc);
    render_footer(f, chunks[4]);

    // Overlay help screen if active
    if show_help {
        render_help(f, area);
    }
}

// ── Header ────────────────────────────────────────────────────────────────────

fn render_header(f: &mut Frame, area: Rect, state: &MeetingState, use_utc: bool, sort_by_quality: bool) {
    let now = if use_utc {
        Utc::now().format("%H:%M:%S UTC").to_string()
    } else {
        Local::now().format("%H:%M:%S %Z").to_string()
    };

    let sort_mode = if sort_by_quality {
        "Quality ↓"
    } else {
        "Name"
    };

    let title = Line::from(vec![
        Span::styled(" vcprobe proctor ", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
        Span::raw("── "),
        Span::styled(&state.meeting_id, Style::default().add_modifier(Modifier::BOLD)),
        Span::raw(format!("  ({})", state.elapsed_str())),
    ]);

    let time_line = Line::from(vec![
        Span::raw(" "),
        Span::styled(now, Style::default().fg(Color::DarkGray)),
        Span::raw(format!(
            "   {} participant{}  ─ Sort: ",
            state.participants.len(),
            if state.participants.len() == 1 { "" } else { "s" }
        )),
        Span::styled(sort_mode, Style::default().fg(Color::Yellow)),
        Span::styled(" [S]", Style::default().fg(Color::DarkGray)),
    ]);

    let block = Block::default().borders(Borders::ALL).border_style(Style::default().fg(Color::DarkGray));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let header_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Length(1)])
        .split(inner);

    f.render_widget(Paragraph::new(title), header_chunks[0]);
    f.render_widget(Paragraph::new(time_line), header_chunks[1]);
}

// ── Participant table ─────────────────────────────────────────────────────────

fn render_participants(
    f: &mut Frame,
    area: Rect,
    state: &MeetingState,
    participants: &[&Participant],
    table_state: &mut TableState,
) {
    // Two-row grouped header
    let header_group = Row::new(vec![
        Cell::from("Participant"),
        Cell::from("Status"),
        Cell::from("Connection").style(Style::default().add_modifier(Modifier::BOLD)),
        Cell::from("Audio").style(Style::default().add_modifier(Modifier::BOLD)),
        Cell::from(""),
        Cell::from("Video").style(Style::default().add_modifier(Modifier::BOLD)),
        Cell::from(""),
        Cell::from("Overall"),
    ])
    .style(Style::default().fg(Color::Yellow))
    .height(1);

    let header_cols = Row::new(vec![
        Cell::from(""),
        Cell::from(""),
        Cell::from("RTT").style(Style::default().add_modifier(Modifier::BOLD)),
        Cell::from("Jitter").style(Style::default().add_modifier(Modifier::BOLD)),
        Cell::from("Conc/s").style(Style::default().add_modifier(Modifier::BOLD)),
        Cell::from("FPS").style(Style::default().add_modifier(Modifier::BOLD)),
        Cell::from("kbps").style(Style::default().add_modifier(Modifier::BOLD)),
        Cell::from("Quality").style(Style::default().add_modifier(Modifier::BOLD)),
    ])
    .style(Style::default().fg(Color::Yellow))
    .height(1);

    let rows: Vec<Row> = participants
        .iter()
        .map(|p| participant_row(p))
        .collect();

    let title = format!(
        " Participants ({}) ",
        state.participants.len()
    );

    let table = Table::new(
        rows,
        [
            Constraint::Min(18),      // name
            Constraint::Length(11),   // [V][M]
            Constraint::Length(8),    // RTT
            Constraint::Length(8),    // jitter
            Constraint::Length(8),    // concealment/s
            Constraint::Length(5),    // FPS
            Constraint::Length(6),    // kbps
            Constraint::Length(12),   // quality bar
        ],
    )
    .header(header_group)
    .header(header_cols)
    .block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray))
            .title(title),
    )
    .row_highlight_style(
        Style::default()
            .bg(Color::DarkGray)
            .add_modifier(Modifier::BOLD),
    )
    .highlight_symbol("▶ ");

    f.render_stateful_widget(table, area, table_state);
}

fn participant_row<'a>(p: &'a Participant) -> Row<'a> {
    let stale = p.is_stale();

    // Status indicators
    let video = if p.video_enabled {
        Span::styled("[V]", Style::default().fg(Color::Cyan))
    } else {
        Span::styled("[ ]", Style::default().fg(Color::DarkGray))
    };
    let mic = if p.audio_enabled {
        Span::styled("[M]", Style::default().fg(Color::Green))
    } else {
        Span::styled("[ ]", Style::default().fg(Color::DarkGray))
    };
    // Note: Removed talking indicator - can't reliably detect from packet stream
    // Audio packets arrive at constant rate regardless of voice activity
    let status_line = Line::from(vec![video, mic]);

    // RTT badge
    let (rtt_str, rtt_color) = match p.rtt_ms {
        Some(r) if r < 80.0 => (format!("{:>5}ms", r as u32), Color::Green),
        Some(r) if r < 150.0 => (format!("{:>5}ms", r as u32), Color::Yellow),
        Some(r) => (format!("{:>5}ms", r as u32), Color::Red),
        None => ("    --  ".to_string(), Color::DarkGray),
    };

    // Jitter and concealment
    let (jitter_str, jitter_color, conceal_str, conceal_color) = match &p.quality {
        Some(q) => (
            format!("{:>5}ms", q.jitter_ms as u32),
            jitter_color(q.jitter_ms),
            format!("{:>5.1}/s", q.conceal_per_sec),
            conceal_color(q.conceal_per_sec),
        ),
        None => (
            "   --  ".to_string(),
            Color::DarkGray,
            "    -- ".to_string(),
            Color::DarkGray,
        ),
    };

    // Quality bar (10 chars wide) — based on concealment rate + jitter
    let (bar, bar_color) = quality_bar(p);

    // FPS and bitrate
    let (fps_str, kbps_str) = match &p.quality {
        Some(q) if p.video_enabled => (
            format!("{:>3}", q.fps as u32),
            format!("{:>5}", q.bitrate_kbps),
        ),
        _ => ("  --".to_string(), "   --".to_string()),
    };

    // Name color-coding based on quality score
    let name_color = if stale {
        Color::DarkGray
    } else {
        match p.quality_score() {
            Some(score) if score > 0.6 => Color::Red,
            Some(score) if score > 0.3 => Color::Yellow,
            Some(_) => Color::Green,
            None => Color::DarkGray,
        }
    };

    let name_style = if stale {
        Style::default().fg(Color::DarkGray).add_modifier(Modifier::DIM)
    } else {
        Style::default().fg(name_color)
    };

    // Truncate email to fit
    let display_name = truncate(&p.email, 20);

    Row::new(vec![
        Cell::from(display_name).style(name_style),
        Cell::from(status_line),
        Cell::from(rtt_str).style(Style::default().fg(rtt_color)),
        Cell::from(jitter_str).style(Style::default().fg(jitter_color)),
        Cell::from(conceal_str).style(Style::default().fg(conceal_color)),
        Cell::from(fps_str).style(Style::default().fg(if p.video_enabled { Color::Reset } else { Color::DarkGray })),
        Cell::from(kbps_str).style(Style::default().fg(if p.video_enabled { Color::Reset } else { Color::DarkGray })),
        Cell::from(bar).style(Style::default().fg(bar_color)),
    ])
    .height(1)
}

fn quality_bar(p: &Participant) -> (String, Color) {
    let score = p.quality_score(); // 0.0 = perfect, 1.0 = terrible
    match score {
        None => ("▒▒▒▒▒▒▒▒▒▒".to_string(), Color::DarkGray), // no data
        Some(s) => {
            let filled = (((1.0 - s) * 10.0) as usize).min(10);
            let empty = 10 - filled;
            let bar = format!("{}{}", "█".repeat(filled), "░".repeat(empty));
            let color = if s < 0.25 {
                Color::Green
            } else if s < 0.6 {
                Color::Yellow
            } else {
                Color::Red
            };
            (bar, color)
        }
    }
}

// ── Detail panel ──────────────────────────────────────────────────────────────

fn render_detail(
    f: &mut Frame,
    area: Rect,
    participants: &[&Participant],
    selected: Option<usize>,
) {
    let p = match selected.and_then(|i| participants.get(i)) {
        Some(p) => p,
        None => return,
    };

    let title = format!(" Detail: {} ", p.email);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Blue))
        .title(Span::styled(title, Style::default().fg(Color::Cyan)));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let rtt_str = p
        .rtt_ms
        .map(|r| format!("{}ms", r as u32))
        .unwrap_or_else(|| "--".to_string());

    let mut lines: Vec<Line> = vec![
        Line::from(vec![
            label("Video: "),
            flag(p.video_enabled),
            Span::raw("   "),
            label("Audio: "),
            flag(p.audio_enabled),
            Span::raw("   "),
            label("RTT: "),
            Span::styled(rtt_str, rtt_style_from(p.rtt_ms)),
        ]),
    ];

    match &p.quality {
        None => {
            lines.push(Line::from(Span::styled(
                "  No quality data received yet (waiting for HEALTH packets)",
                Style::default().fg(Color::DarkGray),
            )));
        }
        Some(q) => {
            let age_secs = q.updated_at.elapsed().as_secs();
            lines.push(Line::from(vec![
                label("Jitter: "),
                colored_value(format!("{}ms", q.jitter_ms as u32), jitter_color(q.jitter_ms)),
                Span::raw("   "),
                label("Conceal: "),
                colored_value(
                    format!("{:.1}/s", q.conceal_per_sec),
                    conceal_color(q.conceal_per_sec),
                ),
                Span::raw("   "),
                label("FPS: "),
                Span::styled(
                    format!("{}", q.fps as u32),
                    Style::default().fg(Color::Reset),
                ),
                Span::raw("   "),
                label("Bitrate: "),
                Span::styled(
                    format!("{} kbps", q.bitrate_kbps),
                    Style::default().fg(Color::Reset),
                ),
            ]));
            lines.push(Line::from(vec![
                label("Can see: "),
                flag(q.can_see),
                Span::raw("   "),
                label("Can hear: "),
                flag(q.can_listen),
                Span::raw(format!(
                    "   {}",
                    if age_secs > 5 {
                        format!("(data {}s old)", age_secs)
                    } else {
                        String::new()
                    }
                )),
            ]));
            // Quality interpretation
            let interpretation = interpret_quality(q, p.rtt_ms);
            if !interpretation.is_empty() {
                lines.push(Line::from(vec![
                    Span::styled("  ⚠ ", Style::default().fg(Color::Yellow)),
                    Span::styled(interpretation, Style::default().fg(Color::Yellow)),
                ]));
            }
        }
    }

    let paragraph = Paragraph::new(lines);
    f.render_widget(paragraph, inner);
}

fn interpret_quality(q: &QualitySnapshot, rtt_ms: Option<f64>) -> String {
    let mut issues = Vec::new();
    if q.conceal_per_sec > 5.0 {
        issues.push(format!("high audio concealment ({:.1}/s)", q.conceal_per_sec));
    }
    if q.jitter_ms > 150.0 {
        issues.push(format!("high jitter ({}ms)", q.jitter_ms as u32));
    }
    if let Some(rtt) = rtt_ms {
        if rtt > 150.0 {
            issues.push(format!("high RTT ({}ms)", rtt as u32));
        }
    }
    if !q.can_see && q.fps == 0.0 {
        issues.push("video not rendering".to_string());
    }
    issues.join(", ")
}

// ── Events panel ─────────────────────────────────────────────────────────────

fn render_events(f: &mut Frame, area: Rect, state: &MeetingState, use_utc: bool) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray))
        .title(" Events ");
    let inner = block.inner(area);
    f.render_widget(block, area);

    // Show most recent events (newest last), fitting into available lines
    let max_lines = inner.height as usize;
    let skip = state.events.len().saturating_sub(max_lines);

    let lines: Vec<Line> = state
        .events
        .iter()
        .skip(skip)
        .map(|ev| {
            // ev.when is a DateTime<Local> captured once at event creation — never recomputed.
            let when_str = if use_utc {
                ev.when.with_timezone(&Utc).format("%H:%M:%S UTC").to_string()
            } else {
                ev.when.format("%H:%M:%S").to_string()
            };
            Line::from(vec![
                Span::styled(when_str, Style::default().fg(Color::DarkGray)),
                Span::raw("  "),
                Span::raw(ev.msg.clone()),
            ])
        })
        .collect();

    f.render_widget(Paragraph::new(lines), inner);
}

// ── Footer ────────────────────────────────────────────────────────────────────

fn render_footer(f: &mut Frame, area: Rect) {
    let line = Line::from(vec![
        Span::styled(" [h/?]", Style::default().fg(Color::Yellow)),
        Span::raw(" help  "),
        Span::styled("[q]", Style::default().fg(Color::Yellow)),
        Span::raw(" quit  "),
        Span::styled("[↑/↓] [j/k]", Style::default().fg(Color::Yellow)),
        Span::raw(" navigate"),
        Span::styled(
            "                                vcprobe 0.1",
            Style::default().fg(Color::DarkGray),
        ),
    ]);
    f.render_widget(Paragraph::new(line), area);
}

// ── Help Screen ───────────────────────────────────────────────────────────────

fn render_help(f: &mut Frame, area: Rect) {
    // Calculate centered position
    let width = area.width.min(80);
    let height = area.height.min(30);
    let x = (area.width.saturating_sub(width)) / 2;
    let y = (area.height.saturating_sub(height)) / 2;

    let help_area = Rect {
        x: area.x + x,
        y: area.y + y,
        width,
        height,
    };

    let help_text = vec![
        Line::from(Span::styled(
            "vcprobe - Proctor Mode Help",
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(Span::styled("Keyboard Controls:", Style::default().fg(Color::Yellow))),
        Line::from("  h, ?     Show/hide this help screen"),
        Line::from("  s, S     Toggle sort mode (Name / Quality ↓)"),
        Line::from("  q, Esc   Quit proctor mode"),
        Line::from("  ↑/↓, j/k Navigate participant list"),
        Line::from("  Home/End Jump to first/last participant"),
        Line::from(""),
        Line::from(Span::styled("Participant Indicators:", Style::default().fg(Color::Yellow))),
        Line::from("  [V]      Video camera enabled"),
        Line::from("  [M]      Microphone enabled"),
        Line::from(""),
        Line::from(Span::styled("Quality Metrics:", Style::default().fg(Color::Yellow))),
        Line::from("  RTT       Round-trip time to server (self-reported)"),
        Line::from("            Green: <80ms  Yellow: 80-150ms  Red: >150ms"),
        Line::from(""),
        Line::from("  Jitter    Audio buffer size in milliseconds"),
        Line::from("            Green: <50ms  Yellow: 50-150ms  Red: >150ms"),
        Line::from(""),
        Line::from("  Conc/s    Audio concealment rate (expand events per second)"),
        Line::from("            Green: <1/s  Yellow: 1-5/s  Red: >5/s"),
        Line::from("            0/s = perfect, 5/s = degraded, 10+/s = poor"),
        Line::from(""),
        Line::from("  FPS       Video frames per second (as observed by peers)"),
        Line::from("  kbps      Video bitrate in kilobits/sec (as observed by peers)"),
        Line::from(""),
        Line::from("  Quality   Combined score from peer observations:"),
        Line::from("            • 50% Audio Concealment (expand events/sec)"),
        Line::from("            • 30% Jitter (audio buffer depth)"),
        Line::from("            • 20% RTT (latency)"),
        Line::from(""),
        Line::from(Span::styled("Name Colors:", Style::default().fg(Color::Yellow))),
        Line::from("  Green: Good quality (score < 0.3)"),
        Line::from("  Yellow: Fair quality (score 0.3-0.6)"),
        Line::from("  Red: Poor quality (score > 0.6)"),
        Line::from("  Gray: No quality data or stale connection"),
        Line::from(""),
        Line::from(Span::styled("Notes:", Style::default().fg(Color::Yellow))),
        Line::from("  • Quality data requires HEALTH packets from other participants"),
        Line::from("  • Solo users show RTT but no quality/FPS/bitrate data"),
        Line::from("  • Participants turn gray after 2 sec without packets"),
        Line::from(""),
        Line::from(Span::styled("Press h or ? to close", Style::default().fg(Color::DarkGray))),
    ];

    let block = Block::default()
        .title(" Help ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .style(Style::default().bg(Color::Black));

    let paragraph = Paragraph::new(help_text).block(block);
    f.render_widget(paragraph, help_area);
}

// ── Formatting helpers ────────────────────────────────────────────────────────

fn label(s: &'static str) -> Span<'static> {
    Span::styled(s, Style::default().fg(Color::DarkGray))
}

fn flag(on: bool) -> Span<'static> {
    if on {
        Span::styled("✓", Style::default().fg(Color::Green))
    } else {
        Span::styled("✗", Style::default().fg(Color::DarkGray))
    }
}

fn colored_value(s: String, color: Color) -> Span<'static> {
    Span::styled(s, Style::default().fg(color))
}

fn rtt_style_from(rtt: Option<f64>) -> Style {
    match rtt {
        Some(r) if r < 80.0 => Style::default().fg(Color::Green),
        Some(r) if r < 150.0 => Style::default().fg(Color::Yellow),
        Some(_) => Style::default().fg(Color::Red),
        None => Style::default().fg(Color::DarkGray),
    }
}

fn jitter_color(jitter_ms: f64) -> Color {
    if jitter_ms < 50.0 {
        Color::Green
    } else if jitter_ms < 150.0 {
        Color::Yellow
    } else {
        Color::Red
    }
}

fn conceal_color(cps: f64) -> Color {
    if cps < 1.0 {
        Color::Green
    } else if cps < 5.0 {
        Color::Yellow
    } else {
        Color::Red
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}…", &s[..max - 1])
    }
}
