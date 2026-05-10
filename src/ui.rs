use crate::monitor::MonitorEvent;
use crate::monitor::MonitorEventKind;
use crossterm::event;
use crossterm::event::Event;
use crossterm::event::KeyCode;
use crossterm::execute;
use crossterm::terminal;
use ratatui::Frame;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Constraint;
use ratatui::layout::Direction;
use ratatui::layout::Layout;
use ratatui::style::Color;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;
use ratatui::text::Text;
use ratatui::widgets::Block;
use ratatui::widgets::Borders;
use ratatui::widgets::Paragraph;
use ratatui::widgets::Wrap;
use std::collections::VecDeque;
use std::io;
use std::time::Duration;
use tokio::sync::broadcast;

struct TerminalGuard;

impl TerminalGuard {
    fn new() -> std::io::Result<Self> {
        terminal::enable_raw_mode()?;
        execute!(std::io::stdout(), terminal::EnterAlternateScreen)?;
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = execute!(std::io::stdout(), terminal::LeaveAlternateScreen);
        let _ = terminal::disable_raw_mode();
    }
}

const PREVIEW_CHAR_LIMIT: usize = 16_000;
const MAX_REQUEST_PANES: usize = 4;

#[derive(Debug, Default)]
struct RequestPane {
    response_id: String,
    text: String,
    segments: VecDeque<PaneSegment>,
    active_function_call_id: Option<String>,
    pending_output_backslash: bool,
    status: RequestPaneStatus,
}

impl RequestPane {
    fn new(response_id: &str) -> Self {
        Self {
            response_id: response_id.to_string(),
            text: String::new(),
            segments: VecDeque::new(),
            active_function_call_id: None,
            pending_output_backslash: false,
            status: RequestPaneStatus::Running,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PaneSegment {
    kind: PaneSegmentKind,
    text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PaneSegmentKind {
    Output,
    Reasoning,
    Tool,
}

impl PaneSegmentKind {
    fn style(self) -> Style {
        match self {
            Self::Output => Style::default(),
            Self::Reasoning => Style::default().fg(Color::DarkGray),
            Self::Tool => Style::default().fg(Color::Rgb(157, 204, 235)),
        }
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
enum RequestPaneStatus {
    #[default]
    Running,
    Completed,
    Failed,
}

#[derive(Debug)]
pub struct UiHandle {
    receiver: broadcast::Receiver<MonitorEvent>,
    panes: VecDeque<RequestPane>,
}

impl UiHandle {
    pub fn new(_bind_addr: String, receiver: broadcast::Receiver<MonitorEvent>) -> Self {
        Self {
            receiver,
            panes: VecDeque::new(),
        }
    }

    pub async fn run(mut self) -> Result<(), String> {
        let _guard =
            TerminalGuard::new().map_err(|err| format!("failed to initialize terminal: {err}"))?;
        let stdout = io::stdout();
        let backend = CrosstermBackend::new(stdout);
        let mut terminal =
            Terminal::new(backend).map_err(|err| format!("failed to start terminal: {err}"))?;
        self.run_loop(&mut terminal).await
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
                    Err(broadcast::error::TryRecvError::Lagged(_)) => continue,
                    Err(broadcast::error::TryRecvError::Closed) => break,
                }
            }

            terminal
                .draw(|frame| self.render(frame))
                .map_err(|err| format!("failed to draw UI: {err}"))?;

            let event_result = tokio::task::spawn_blocking(|| {
                if event::poll(Duration::from_millis(16))? {
                    Ok(Some(event::read()?))
                } else {
                    Ok::<_, std::io::Error>(None)
                }
            })
            .await
            .map_err(|e| format!("spawn_blocking: {e}"))?;

            if let Some(ev) =
                event_result.map_err(|err| format!("failed to read terminal event: {err}"))?
            {
                match ev {
                    Event::Key(key) if key.code == KeyCode::Char('q') => return Ok(()),
                    Event::Resize(_, _) => {}
                    _ => {}
                }
            }
        }
    }

    fn apply_event(&mut self, event: MonitorEvent) {
        match event.kind {
            MonitorEventKind::RequestStarted { .. } => {
                let pane = self.started_pane_mut(&event.response_id);
                pane.text.clear();
                pane.segments.clear();
                pane.active_function_call_id = None;
                pane.pending_output_backslash = false;
                pane.status = RequestPaneStatus::Running;
            }
            MonitorEventKind::OutputTextDelta { delta } => {
                append_output_text_delta(self.pane_mut(&event.response_id), &delta);
            }
            MonitorEventKind::ReasoningTextDelta { delta } => {
                append_styled_text_delta(
                    self.pane_mut(&event.response_id),
                    PaneSegmentKind::Reasoning,
                    &delta,
                );
            }
            MonitorEventKind::RefusalDelta { delta } => {
                append_styled_text_delta(
                    self.pane_mut(&event.response_id),
                    PaneSegmentKind::Output,
                    &delta,
                );
            }
            MonitorEventKind::FunctionCallArgumentsDelta { call_id, delta } => {
                append_function_call_delta(self.pane_mut(&event.response_id), &call_id, &delta);
            }
            MonitorEventKind::ToolPhase { phase, detail } => {
                if let Some(line) = tool_phase_line(&phase, &detail) {
                    append_pane_line(
                        self.pane_mut(&event.response_id),
                        PaneSegmentKind::Tool,
                        &line,
                    );
                }
            }
            MonitorEventKind::Completed => {
                if let Some(pane) = self.existing_pane_mut(&event.response_id) {
                    flush_pending_output_backslash(pane);
                    pane.active_function_call_id = None;
                    pane.status = RequestPaneStatus::Completed;
                }
            }
            MonitorEventKind::Failed { message } => {
                if let Some(pane) = self.existing_pane_mut(&event.response_id) {
                    flush_pending_output_backslash(pane);
                    pane.active_function_call_id = None;
                    pane.status = RequestPaneStatus::Failed;
                    append_pane_line(pane, PaneSegmentKind::Tool, &format!("failed: {message}"));
                }
            }
            MonitorEventKind::UpstreamRequest { .. } => {}
            MonitorEventKind::ResponseItem { .. } => {}
        }
    }

    fn existing_pane_mut(&mut self, response_id: &str) -> Option<&mut RequestPane> {
        let index = self
            .panes
            .iter()
            .position(|pane| pane.response_id == response_id)?;
        self.panes.get_mut(index)
    }

    fn started_pane_mut(&mut self, response_id: &str) -> &mut RequestPane {
        if let Some(index) = self
            .panes
            .iter()
            .position(|pane| pane.response_id == response_id)
        {
            return self.panes.get_mut(index).expect("pane exists");
        }

        if let Some(index) = self
            .panes
            .iter()
            .rposition(|pane| pane.status != RequestPaneStatus::Running)
        {
            self.panes[index] = RequestPane::new(response_id);
            return self.panes.get_mut(index).expect("pane replaced");
        }

        if self.panes.len() >= MAX_REQUEST_PANES {
            let _ = self.panes.pop_back();
        }
        self.panes.push_front(RequestPane::new(response_id));
        self.panes.front_mut().expect("pane inserted")
    }

    fn pane_mut(&mut self, response_id: &str) -> &mut RequestPane {
        let index = self
            .panes
            .iter()
            .position(|pane| pane.response_id == response_id);
        match index {
            Some(index) => self.panes.get_mut(index).expect("pane exists"),
            None => {
                if self.panes.len() >= MAX_REQUEST_PANES {
                    self.evict_pane_for_new_request();
                }
                self.panes.push_front(RequestPane::new(response_id));
                self.panes.front_mut().expect("pane inserted")
            }
        }
    }

    fn evict_pane_for_new_request(&mut self) {
        if let Some(index) = self
            .panes
            .iter()
            .rposition(|pane| pane.status != RequestPaneStatus::Running)
        {
            let _ = self.panes.remove(index);
        } else {
            let _ = self.panes.pop_back();
        }
    }

    fn render(&self, frame: &mut Frame) {
        let pane_count = self.panes.len().min(MAX_REQUEST_PANES);
        if pane_count == 0 {
            return;
        }

        let constraints = vec![Constraint::Ratio(1, pane_count as u32); pane_count];
        let panes = Layout::default()
            .direction(Direction::Vertical)
            .constraints(constraints)
            .split(frame.area());

        for (pane, area) in self.panes.iter().zip(panes.iter()) {
            let scroll = scroll_offset(
                &pane.text,
                area.height.saturating_sub(2),
                area.width.saturating_sub(2),
            );
            frame.render_widget(
                Paragraph::new(styled_pane_text(pane))
                    .block(Block::default().borders(Borders::ALL))
                    .scroll((scroll, 0))
                    .wrap(Wrap { trim: false }),
                *area,
            );
        }
    }
}

#[cfg(test)]
fn append_preview(buffer: &mut String, delta: &str) {
    buffer.push_str(delta);
    let char_count = buffer.chars().count();
    if char_count > PREVIEW_CHAR_LIMIT {
        let drain_chars = char_count - PREVIEW_CHAR_LIMIT;
        drain_string_prefix_chars(buffer, drain_chars);
    }
}

fn append_output_text_delta(pane: &mut RequestPane, delta: &str) {
    prepare_for_styled_delta(pane, PaneSegmentKind::Output);
    let normalized = normalize_output_newlines(pane, delta);
    append_pane_text(pane, PaneSegmentKind::Output, &normalized);
}

fn append_styled_text_delta(pane: &mut RequestPane, kind: PaneSegmentKind, delta: &str) {
    if kind != PaneSegmentKind::Output {
        flush_pending_output_backslash(pane);
    }
    prepare_for_styled_delta(pane, kind);
    append_pane_text(pane, kind, delta);
}

fn prepare_for_styled_delta(pane: &mut RequestPane, kind: PaneSegmentKind) {
    if pane.active_function_call_id.take().is_some() {
        ensure_newline(pane, kind);
    } else {
        ensure_newline_after_kind_change(pane, kind);
    }
}

fn append_function_call_delta(pane: &mut RequestPane, call_id: &str, delta: &str) {
    flush_pending_output_backslash(pane);
    if pane.active_function_call_id.as_deref() != Some(call_id) {
        ensure_newline(pane, PaneSegmentKind::Tool);
        append_pane_text(
            pane,
            PaneSegmentKind::Tool,
            &format!("tool arguments {}:\n", short_id(call_id)),
        );
        pane.active_function_call_id = Some(call_id.to_string());
    }
    append_pane_text(pane, PaneSegmentKind::Tool, delta);
}

fn append_pane_line(pane: &mut RequestPane, kind: PaneSegmentKind, line: &str) {
    flush_pending_output_backslash(pane);
    pane.active_function_call_id = None;
    ensure_newline(pane, kind);
    append_pane_text(pane, kind, line);
    append_pane_text(pane, kind, "\n");
}

fn ensure_newline(pane: &mut RequestPane, kind: PaneSegmentKind) {
    if !pane.text.is_empty() && !pane.text.ends_with('\n') {
        append_pane_text(pane, kind, "\n");
    }
}

fn ensure_newline_after_kind_change(pane: &mut RequestPane, kind: PaneSegmentKind) {
    if !pane.text.is_empty() && !pane.text.ends_with('\n') && last_segment_kind(pane) != Some(kind)
    {
        append_pane_text(pane, kind, "\n");
    }
}

fn append_pane_text(pane: &mut RequestPane, kind: PaneSegmentKind, text: &str) {
    if text.is_empty() {
        return;
    }

    pane.text.push_str(text);
    match pane.segments.back_mut() {
        Some(segment) if segment.kind == kind => segment.text.push_str(text),
        _ => pane.segments.push_back(PaneSegment {
            kind,
            text: text.to_string(),
        }),
    }

    let char_count = pane.text.chars().count();
    if char_count > PREVIEW_CHAR_LIMIT {
        let drain_chars = char_count - PREVIEW_CHAR_LIMIT;
        drain_string_prefix_chars(&mut pane.text, drain_chars);
        drain_segment_prefix_chars(&mut pane.segments, drain_chars);
    }
}

fn drain_string_prefix_chars(buffer: &mut String, drain_chars: usize) {
    if drain_chars == 0 {
        return;
    }
    let drain_len = buffer
        .char_indices()
        .nth(drain_chars)
        .map(|(index, _)| index)
        .unwrap_or(buffer.len());
    buffer.drain(..drain_len);
}

fn drain_segment_prefix_chars(segments: &mut VecDeque<PaneSegment>, mut drain_chars: usize) {
    while drain_chars > 0 {
        let Some(front) = segments.front_mut() else {
            return;
        };
        let segment_chars = front.text.chars().count();
        if drain_chars >= segment_chars {
            drain_chars -= segment_chars;
            let _ = segments.pop_front();
            continue;
        }

        drain_string_prefix_chars(&mut front.text, drain_chars);
        if front.text.is_empty() {
            let _ = segments.pop_front();
        }
        return;
    }
}

fn normalize_output_newlines(pane: &mut RequestPane, delta: &str) -> String {
    let mut output = String::new();
    let mut chars = delta.chars();

    if pane.pending_output_backslash {
        pane.pending_output_backslash = false;
        match chars.next() {
            Some('n') => output.push('\n'),
            Some(ch) => {
                output.push('\\');
                output.push(ch);
            }
            None => {
                pane.pending_output_backslash = true;
                return output;
            }
        }
    }

    while let Some(ch) = chars.next() {
        if ch == '\\' {
            match chars.next() {
                Some('n') => output.push('\n'),
                Some(next) => {
                    output.push('\\');
                    output.push(next);
                }
                None => pane.pending_output_backslash = true,
            }
        } else {
            output.push(ch);
        }
    }

    output
}

fn flush_pending_output_backslash(pane: &mut RequestPane) {
    if pane.pending_output_backslash {
        pane.pending_output_backslash = false;
        append_pane_text(pane, PaneSegmentKind::Output, "\\");
    }
}

fn last_segment_kind(pane: &RequestPane) -> Option<PaneSegmentKind> {
    pane.segments
        .iter()
        .rev()
        .find(|segment| !segment.text.is_empty())
        .map(|segment| segment.kind)
}

fn styled_pane_text(pane: &RequestPane) -> Text<'static> {
    styled_text_from_segments(&pane.segments)
}

fn styled_text_from_segments(segments: &VecDeque<PaneSegment>) -> Text<'static> {
    let mut lines: Vec<Vec<Span<'static>>> = vec![Vec::new()];
    for segment in segments {
        let style = segment.kind.style();
        let mut parts = segment.text.split('\n').peekable();
        while let Some(part) = parts.next() {
            if !part.is_empty() {
                lines
                    .last_mut()
                    .expect("at least one line exists")
                    .push(Span::styled(part.to_string(), style));
            }
            if parts.peek().is_some() {
                lines.push(Vec::new());
            }
        }
    }

    Text::from(lines.into_iter().map(Line::from).collect::<Vec<_>>())
}

fn tool_phase_line(phase: &str, detail: &str) -> Option<String> {
    match phase {
        "provider_tool_detected" => None,
        "client_tool_handoff"
        | "client_tool_result"
        | "provider_tool_running"
        | "provider_tool_completed" => Some(detail.to_string()),
        _ => Some(format!("{phase}: {detail}")),
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

fn scroll_offset(text: &str, height: u16, width: u16) -> u16 {
    let height = usize::from(height);
    if height == 0 {
        return 0;
    }

    let lines = wrapped_line_count(text, usize::from(width.max(1)));
    u16::try_from(lines.saturating_sub(height)).unwrap_or(u16::MAX)
}

fn wrapped_line_count(text: &str, width: usize) -> usize {
    let width = width.max(1);
    text.split('\n')
        .map(|line| wrapped_physical_line_count(line, width))
        .sum()
}

fn wrapped_physical_line_count(line: &str, width: usize) -> usize {
    if line.is_empty() {
        return 1;
    }

    let mut lines = 1usize;
    let mut col = 0usize;
    let mut chars = line.chars().peekable();
    while let Some(ch) = chars.peek().copied() {
        let is_whitespace = ch.is_whitespace();
        let mut run_width = 0usize;
        while chars
            .peek()
            .copied()
            .is_some_and(|next| next.is_whitespace() == is_whitespace)
        {
            let _ = chars.next();
            run_width += 1;
        }

        if is_whitespace {
            append_wrapped_whitespace(run_width, width, &mut lines, &mut col);
        } else {
            append_wrapped_word(run_width, width, &mut lines, &mut col);
        }
    }

    lines
}

fn append_wrapped_word(width_used: usize, width: usize, lines: &mut usize, col: &mut usize) {
    if *col == width {
        *lines += 1;
        *col = 0;
    }

    if width_used > width {
        if *col > 0 {
            *lines += 1;
            *col = 0;
        }
        *lines += (width_used - 1) / width;
        *col = ((width_used - 1) % width) + 1;
    } else if *col == 0 {
        *col = width_used;
    } else if *col + width_used <= width {
        *col += width_used;
    } else {
        *lines += 1;
        *col = width_used;
    }
}

fn append_wrapped_whitespace(
    mut width_used: usize,
    width: usize,
    lines: &mut usize,
    col: &mut usize,
) {
    while width_used > 0 {
        if *col == width {
            *lines += 1;
            *col = 0;
        }
        let room = width - *col;
        if width_used <= room {
            *col += width_used;
            return;
        }
        width_used -= room;
        *lines += 1;
        *col = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::MAX_REQUEST_PANES;
    use super::PREVIEW_CHAR_LIMIT;
    use super::PaneSegmentKind;
    use super::RequestPaneStatus;
    use super::UiHandle;
    use super::append_preview;
    use super::scroll_offset;
    use super::wrapped_line_count;
    use crate::monitor::MonitorEvent;
    use crate::monitor::MonitorEventKind;
    use pretty_assertions::assert_eq;

    #[test]
    fn append_preview_trims_on_char_boundary() {
        let mut buffer = "a".repeat(PREVIEW_CHAR_LIMIT - 1);
        append_preview(&mut buffer, "éβ");

        assert_eq!(buffer.chars().count(), PREVIEW_CHAR_LIMIT);
        assert_eq!(buffer.chars().last(), Some('β'));
    }

    #[test]
    fn wrapped_line_count_accounts_for_width_and_newlines() {
        assert_eq!(wrapped_line_count("", 10), 1);
        assert_eq!(wrapped_line_count("abcdef", 3), 2);
        assert_eq!(wrapped_line_count("aaaaa bbbbb ccccc", 10), 3);
        assert_eq!(wrapped_line_count("abcdefghijklmnop", 5), 4);
        assert_eq!(wrapped_line_count("abc\ndef", 10), 2);
        assert_eq!(wrapped_line_count("abc\n", 10), 2);
    }

    #[test]
    fn scroll_offset_stays_at_bottom() {
        assert_eq!(scroll_offset("a\nb\nc", 2, 10), 1);
        assert_eq!(scroll_offset("abcdef", 2, 3), 0);
        assert_eq!(scroll_offset("abcdefghi", 2, 3), 1);
        assert_eq!(scroll_offset("aaaaa bbbbb ccccc", 2, 10), 1);
    }

    #[test]
    fn completed_panes_linger_until_capacity_is_needed() {
        let (_tx, receiver) = tokio::sync::broadcast::channel(1);
        let mut ui = UiHandle::new(String::new(), receiver);

        for index in 0..(MAX_REQUEST_PANES + 1) {
            ui.apply_event(started(&format!("resp_{index}")));
        }

        assert_eq!(ui.panes.len(), MAX_REQUEST_PANES);
        assert_eq!(
            ui.panes
                .iter()
                .map(|pane| pane.response_id.as_str())
                .collect::<Vec<_>>(),
            vec!["resp_4", "resp_3", "resp_2", "resp_1"]
        );

        ui.apply_event(delta("resp_3", "hello"));
        ui.apply_event(event("resp_3", MonitorEventKind::Completed));
        assert_eq!(
            ui.panes
                .iter()
                .find(|pane| pane.response_id == "resp_3")
                .map(|pane| pane.text.as_str()),
            Some("hello")
        );
        assert_eq!(
            ui.panes
                .iter()
                .find(|pane| pane.response_id == "resp_3")
                .map(|pane| &pane.status),
            Some(&RequestPaneStatus::Completed)
        );

        ui.apply_event(started("resp_5"));
        assert_eq!(
            ui.panes
                .iter()
                .map(|pane| pane.response_id.as_str())
                .collect::<Vec<_>>(),
            vec!["resp_4", "resp_5", "resp_2", "resp_1"]
        );
    }

    #[test]
    fn completed_panes_are_reused_before_adding_slots() {
        let (_tx, receiver) = tokio::sync::broadcast::channel(1);
        let mut ui = UiHandle::new(String::new(), receiver);

        ui.apply_event(started("resp_1"));
        ui.apply_event(delta("resp_1", "old"));
        ui.apply_event(event("resp_1", MonitorEventKind::Completed));
        ui.apply_event(started("resp_2"));

        assert_eq!(ui.panes.len(), 1);
        assert_eq!(
            ui.panes
                .front()
                .map(|pane| (pane.response_id.as_str(), pane.text.as_str())),
            Some(("resp_2", ""))
        );
    }

    #[test]
    fn tool_related_events_append_to_text_pane() {
        let (_tx, receiver) = tokio::sync::broadcast::channel(1);
        let mut ui = UiHandle::new(String::new(), receiver);

        ui.apply_event(started("resp_1"));
        ui.apply_event(event(
            "resp_1",
            MonitorEventKind::FunctionCallArgumentsDelta {
                call_id: "call_12345678901234567890".to_string(),
                delta: "{\"cmd\":".to_string(),
            },
        ));
        ui.apply_event(event(
            "resp_1",
            MonitorEventKind::FunctionCallArgumentsDelta {
                call_id: "call_12345678901234567890".to_string(),
                delta: "\"ls\"}".to_string(),
            },
        ));
        ui.apply_event(event(
            "resp_1",
            MonitorEventKind::ToolPhase {
                phase: "provider_tool_completed".to_string(),
                detail: "web_search result ok".to_string(),
            },
        ));
        ui.apply_event(event(
            "resp_1",
            MonitorEventKind::RefusalDelta {
                delta: "no".to_string(),
            },
        ));

        assert_eq!(
            ui.panes.front().map(|pane| pane.text.as_str()),
            Some(
                "tool arguments call_1234567890123...:\n{\"cmd\":\"ls\"}\nweb_search result ok\nno"
            )
        );
        assert_eq!(
            ui.panes.front().map(|pane| pane
                .segments
                .iter()
                .map(|segment| segment.kind)
                .collect::<Vec<_>>()),
            Some(vec![PaneSegmentKind::Tool, PaneSegmentKind::Output])
        );
    }

    #[test]
    fn output_text_preview_decodes_literal_newline_sequences() {
        let (_tx, receiver) = tokio::sync::broadcast::channel(1);
        let mut ui = UiHandle::new(String::new(), receiver);

        ui.apply_event(started("resp_1"));
        ui.apply_event(delta("resp_1", "first\\nsecond"));
        ui.apply_event(delta("resp_1", "\\"));
        ui.apply_event(delta("resp_1", "nthird"));
        ui.apply_event(event("resp_1", MonitorEventKind::Completed));

        assert_eq!(
            ui.panes.front().map(|pane| pane.text.as_str()),
            Some("first\nsecond\nthird")
        );
    }

    #[test]
    fn reasoning_output_and_tool_text_are_separate_segments() {
        let (_tx, receiver) = tokio::sync::broadcast::channel(1);
        let mut ui = UiHandle::new(String::new(), receiver);

        ui.apply_event(started("resp_1"));
        ui.apply_event(event(
            "resp_1",
            MonitorEventKind::ReasoningTextDelta {
                delta: "thinking".to_string(),
            },
        ));
        ui.apply_event(delta("resp_1", "answer"));
        ui.apply_event(event(
            "resp_1",
            MonitorEventKind::ToolPhase {
                phase: "provider_tool_completed".to_string(),
                detail: "tool result".to_string(),
            },
        ));

        let pane = ui.panes.front().expect("pane exists");
        assert_eq!(pane.text, "thinking\nanswer\ntool result\n");
        assert_eq!(
            pane.segments
                .iter()
                .map(|segment| segment.kind)
                .collect::<Vec<_>>(),
            vec![
                PaneSegmentKind::Reasoning,
                PaneSegmentKind::Output,
                PaneSegmentKind::Tool
            ]
        );
    }

    fn started(response_id: &str) -> MonitorEvent {
        event(
            response_id,
            MonitorEventKind::RequestStarted {
                model: "test-model".to_string(),
                input_items: 0,
                tool_count: 0,
                turn_count: 0,
                user_messages: 0,
                assistant_messages: 0,
                system_messages: 0,
                developer_messages: 0,
                reasoning_items: 0,
                function_calls: 0,
                function_outputs: 0,
                tool_items: 0,
                input_chars: 0,
                instructions_chars: 0,
            },
        )
    }

    fn delta(response_id: &str, text: &str) -> MonitorEvent {
        event(
            response_id,
            MonitorEventKind::OutputTextDelta {
                delta: text.to_string(),
            },
        )
    }

    fn event(response_id: &str, kind: MonitorEventKind) -> MonitorEvent {
        MonitorEvent {
            response_id: response_id.to_string(),
            timestamp_ms: 0,
            kind,
        }
    }
}
