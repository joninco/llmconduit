use crate::monitor::MonitorEvent;
use crate::monitor::MonitorEventKind;
use crossterm::event;
use crossterm::event::Event;
use crossterm::event::KeyCode;
use crossterm::execute;
use crossterm::terminal::EnterAlternateScreen;
use crossterm::terminal::LeaveAlternateScreen;
use crossterm::terminal::disable_raw_mode;
use crossterm::terminal::enable_raw_mode;
use ratatui::Frame;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Constraint;
use ratatui::layout::Direction;
use ratatui::layout::Layout;
use ratatui::style::Color;
use ratatui::style::Modifier;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;
use ratatui::widgets::Block;
use ratatui::widgets::Borders;
use ratatui::widgets::Paragraph;
use ratatui::widgets::Wrap;
use std::collections::VecDeque;
use std::io;
use std::time::Duration;
use tokio::sync::broadcast;

const PREVIEW_CHAR_LIMIT: usize = 16_000;
const TOOL_NOTE_LIMIT: usize = 6;
const TURN_LIMIT: usize = 8;

#[derive(Debug, Default)]
struct TurnState {
    response_id: String,
    status: String,
    model: Option<String>,
    request_summary: String,
    upstream_summary: String,
    reasoning_preview: String,
    output_preview: String,
    tool_notes: VecDeque<String>,
    failure_message: Option<String>,
}

#[derive(Debug)]
pub struct UiHandle {
    bind_addr: String,
    receiver: broadcast::Receiver<MonitorEvent>,
    turns: VecDeque<TurnState>,
}

impl UiHandle {
    pub fn new(bind_addr: String, receiver: broadcast::Receiver<MonitorEvent>) -> Self {
        Self {
            bind_addr,
            receiver,
            turns: VecDeque::new(),
        }
    }

    pub async fn run(mut self) -> Result<(), String> {
        enable_raw_mode().map_err(|err| format!("failed to enable raw mode: {err}"))?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen)
            .map_err(|err| format!("failed to enter alternate screen: {err}"))?;
        let backend = CrosstermBackend::new(stdout);
        let mut terminal =
            Terminal::new(backend).map_err(|err| format!("failed to start terminal: {err}"))?;
        let result = self.run_loop(&mut terminal).await;
        disable_raw_mode().map_err(|err| format!("failed to disable raw mode: {err}"))?;
        execute!(terminal.backend_mut(), LeaveAlternateScreen)
            .map_err(|err| format!("failed to leave alternate screen: {err}"))?;
        terminal
            .show_cursor()
            .map_err(|err| format!("failed to show cursor: {err}"))?;
        result
    }

    async fn run_loop(
        &mut self,
        terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    ) -> Result<(), String> {
        loop {
            loop {
                match self.receiver.try_recv() {
                    Ok(event) => self.apply_event(event),
                    Err(broadcast::error::TryRecvError::Empty) => break,
                    Err(broadcast::error::TryRecvError::Lagged(skipped)) => {
                        self.push_monitor_notice(format!(
                            "monitor lagged, skipped {skipped} events"
                        ));
                    }
                    Err(broadcast::error::TryRecvError::Closed) => {
                        self.push_monitor_notice("monitor channel closed".to_string());
                        break;
                    }
                }
            }

            terminal
                .draw(|frame| self.render(frame))
                .map_err(|err| format!("failed to draw UI: {err}"))?;

            if event::poll(Duration::from_millis(16))
                .map_err(|err| format!("failed to poll terminal events: {err}"))?
            {
                match event::read().map_err(|err| format!("failed to read key: {err}"))? {
                    Event::Key(key) if key.code == KeyCode::Char('q') => return Ok(()),
                    Event::Resize(_, _) => {}
                    _ => {}
                }
            }
        }
    }

    fn apply_event(&mut self, event: MonitorEvent) {
        let turn = self.turn_mut(&event.response_id);
        match event.kind {
            MonitorEventKind::RequestStarted {
                model,
                input_items,
                tool_count,
                turn_count,
                user_messages,
                assistant_messages,
                system_messages,
                developer_messages,
                reasoning_items,
                function_calls,
                function_outputs,
                tool_items,
                input_chars,
                instructions_chars,
            } => {
                turn.model = Some(model.clone());
                turn.status = "running".to_string();
                turn.failure_message = None;
                turn.request_summary = summarize_request(
                    &model,
                    input_items,
                    tool_count,
                    turn_count,
                    user_messages,
                    assistant_messages,
                    system_messages,
                    developer_messages,
                    reasoning_items,
                    function_calls,
                    function_outputs,
                    tool_items,
                    input_chars,
                    instructions_chars,
                );
            }
            MonitorEventKind::UpstreamRequest {
                request_index,
                message_count,
                prompt_chars,
            } => {
                turn.upstream_summary = format!(
                    "upstream {request_index}  |  msgs {message_count}  |  prompt chars {prompt_chars}"
                );
            }
            MonitorEventKind::ResponseItem { .. } => {}
            MonitorEventKind::OutputTextDelta { delta } => {
                append_preview(&mut turn.output_preview, &delta);
            }
            MonitorEventKind::ReasoningTextDelta { delta } => {
                append_preview(&mut turn.reasoning_preview, &delta);
            }
            MonitorEventKind::ToolPhase { phase, detail } => {
                push_recent(
                    &mut turn.tool_notes,
                    format!("{phase}: {detail}"),
                    TOOL_NOTE_LIMIT,
                );
            }
            MonitorEventKind::Completed => {
                turn.status = "completed".to_string();
            }
            MonitorEventKind::Failed { message } => {
                turn.status = "failed".to_string();
                turn.failure_message = Some(message);
            }
        }
    }

    fn turn_mut(&mut self, response_id: &str) -> &mut TurnState {
        let index = self
            .turns
            .iter()
            .position(|turn| turn.response_id == response_id);
        match index {
            Some(index) => self.turns.get_mut(index).expect("turn exists"),
            None => {
                self.turns.push_front(TurnState {
                    response_id: response_id.to_string(),
                    status: "running".to_string(),
                    ..TurnState::default()
                });
                while self.turns.len() > TURN_LIMIT {
                    let _ = self.turns.pop_back();
                }
                self.turns.front_mut().expect("turn inserted")
            }
        }
    }

    fn push_monitor_notice(&mut self, message: String) {
        let turn = self.turn_mut("monitor");
        turn.status = "system".to_string();
        turn.request_summary = "resp2chat monitor".to_string();
        push_recent(&mut turn.tool_notes, message, TOOL_NOTE_LIMIT);
    }

    fn render(&self, frame: &mut Frame) {
        let latest_turn = self.turns.front();
        let layout = Layout::default()
            .direction(Direction::Vertical)
            .constraints(if latest_turn.is_some() {
                vec![
                    Constraint::Length(1),
                    Constraint::Length(6),
                    Constraint::Min(10),
                    Constraint::Length(3),
                ]
            } else {
                vec![
                    Constraint::Length(1),
                    Constraint::Length(6),
                    Constraint::Min(10),
                ]
            })
            .split(frame.area());

        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(
                    "resp2chat",
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw("  "),
                Span::raw(format!("listening on {}", self.bind_addr)),
                Span::raw("  "),
                Span::styled(
                    "q",
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(" quit"),
            ])),
            layout[0],
        );

        frame.render_widget(
            Paragraph::new(
                latest_turn
                    .map(|turn| {
                        if turn.request_summary.is_empty() && turn.upstream_summary.is_empty() {
                            "Waiting for the first request".to_string()
                        } else if turn.upstream_summary.is_empty() {
                            turn.request_summary.clone()
                        } else if turn.request_summary.is_empty() {
                            turn.upstream_summary.clone()
                        } else {
                            format!("{}\n{}", turn.request_summary, turn.upstream_summary)
                        }
                    })
                    .unwrap_or_else(|| "Waiting for the first request".to_string()),
            )
            .block(Block::default().borders(Borders::ALL).title("Input"))
            .wrap(Wrap { trim: false }),
            layout[1],
        );

        let output_lines = render_output(latest_turn);
        let output_height = usize::from(layout[2].height.saturating_sub(2));
        let output_scroll = output_lines.len().saturating_sub(output_height);
        frame.render_widget(
            Paragraph::new(output_lines)
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title("Streaming Output"),
                )
                .scroll((u16::try_from(output_scroll).unwrap_or(u16::MAX), 0))
                .wrap(Wrap { trim: false }),
            layout[2],
        );

        if latest_turn.is_some() {
            frame.render_widget(
                Paragraph::new(render_footer(&self.turns))
                    .block(Block::default().borders(Borders::ALL)),
                layout[3],
            );
        }
    }
}

fn render_output(turn: Option<&TurnState>) -> Vec<Line<'static>> {
    let Some(turn) = turn else {
        return vec!["No output yet".into()];
    };

    let mut lines = Vec::new();
    if let Some(message) = turn.failure_message.as_ref() {
        lines.push(Line::from(vec![
            Span::styled(
                "failed",
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            ),
            Span::raw(": "),
            Span::raw(message.clone()),
        ]));
        lines.push(Line::from(""));
    }
    if !turn.tool_notes.is_empty() {
        lines.push(Line::from(vec![Span::styled(
            "activity",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )]));
        for note in turn.tool_notes.iter().rev() {
            lines.push(Line::from(format!("• {note}")));
        }
        lines.push(Line::from(""));
    }
    if !turn.reasoning_preview.is_empty() {
        lines.push(Line::from(vec![Span::styled(
            "reasoning",
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )]));
        lines.extend(turn.reasoning_preview.lines().map(|line| {
            Line::from(Span::styled(
                line.to_string(),
                Style::default().fg(Color::DarkGray),
            ))
        }));
        lines.push(Line::from(""));
    }
    if !turn.output_preview.is_empty() {
        lines.push(Line::from(vec![Span::styled(
            "output",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )]));
        lines.extend(
            turn.output_preview
                .lines()
                .map(|line| Line::from(line.to_string())),
        );
    }
    if lines.is_empty() {
        vec!["Waiting for streamed output".into()]
    } else {
        lines
    }
}

fn render_footer(turns: &VecDeque<TurnState>) -> String {
    if turns.is_empty() {
        return String::new();
    }

    let latest = turns
        .front()
        .map(|turn| format!("latest {} {}", short_id(&turn.response_id), turn.status))
        .unwrap_or_default();
    let recent = turns
        .iter()
        .take(4)
        .map(|turn| format!("{} {}", short_id(&turn.response_id), turn.status))
        .collect::<Vec<_>>()
        .join("  |  ");
    if recent.is_empty() {
        latest
    } else {
        format!("{latest}  |  recent {recent}")
    }
}

#[allow(clippy::too_many_arguments)]
fn summarize_request(
    model: &str,
    input_items: usize,
    tool_count: usize,
    turn_count: usize,
    user_messages: usize,
    assistant_messages: usize,
    system_messages: usize,
    developer_messages: usize,
    reasoning_items: usize,
    function_calls: usize,
    function_outputs: usize,
    tool_items: usize,
    input_chars: usize,
    instructions_chars: usize,
) -> String {
    let mut lines = Vec::new();
    lines.push(format!(
        "model {model}  |  turns {turn_count}  |  items {input_items}  |  tools {tool_count}"
    ));
    lines.push(format!(
        "user {user_messages}  |  assistant {assistant_messages}  |  reasoning {reasoning_items}"
    ));

    let mut detail_parts = Vec::new();
    if system_messages > 0 {
        detail_parts.push(format!("system {system_messages}"));
    }
    if developer_messages > 0 {
        detail_parts.push(format!("developer {developer_messages}"));
    }
    if function_calls > 0 {
        detail_parts.push(format!("fn calls {function_calls}"));
    }
    if function_outputs > 0 {
        detail_parts.push(format!("fn outputs {function_outputs}"));
    }
    if tool_items > 0 {
        detail_parts.push(format!("tool items {tool_items}"));
    }
    detail_parts.push(format!("input chars {input_chars}"));
    if instructions_chars > 0 {
        detail_parts.push(format!("instructions {instructions_chars}"));
    }
    lines.push(detail_parts.join("  |  "));
    lines.join("\n")
}

fn append_preview(buffer: &mut String, delta: &str) {
    buffer.push_str(delta);
    let char_count = buffer.chars().count();
    if char_count > PREVIEW_CHAR_LIMIT {
        let drain_chars = char_count - PREVIEW_CHAR_LIMIT;
        let drain_len = buffer
            .char_indices()
            .nth(drain_chars)
            .map(|(index, _)| index)
            .unwrap_or(buffer.len());
        buffer.drain(..drain_len);
    }
}

fn push_recent(entries: &mut VecDeque<String>, entry: String, limit: usize) {
    entries.push_back(entry);
    while entries.len() > limit {
        let _ = entries.pop_front();
    }
}

fn short_id(response_id: &str) -> String {
    const LIMIT: usize = 18;
    if response_id.len() <= LIMIT {
        response_id.to_string()
    } else {
        format!("{}...", &response_id[..LIMIT])
    }
}

#[cfg(test)]
mod tests {
    use super::PREVIEW_CHAR_LIMIT;
    use super::append_preview;
    use pretty_assertions::assert_eq;

    #[test]
    fn append_preview_trims_on_char_boundary() {
        let mut buffer = "a".repeat(PREVIEW_CHAR_LIMIT - 1);
        append_preview(&mut buffer, "éβ");

        assert_eq!(buffer.chars().count(), PREVIEW_CHAR_LIMIT);
        assert_eq!(buffer.chars().last(), Some('β'));
    }
}
