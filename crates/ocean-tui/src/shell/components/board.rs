//! BoardComponent — a Kanban board as a projection over a persistent room's
//! transcript (phase-2 wiring of `ocean-board`).
//!
//! The component retains every room message as a `(clock, author, body)` row
//! and re-folds the whole set through `ocean_board::project` on each update.
//! The fold is order-independent (per-field last-writer-wins on `EventClock`),
//! so hydrate-plus-live-tail through this single path *is* a full replay —
//! that equality is pinned by the fixture test at the bottom of this file.
//!
//! Writes are encoded `CardEnvelope` bodies posted through the room's existing
//! message path (`Action::BoardPostCard`); the change itself arrives back over
//! the room events SSE tail. There is no local echo, no cards table, and no
//! keying on thread structure — `card_id` inside the envelope is the only join
//! key.

use crossterm::event::{KeyCode, KeyEvent};
use ocean_board::{Board, BoardEvent, CardEnvelope, CardOp, EventClock};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
    Frame,
};

use crate::shell::{
    action::Action, component::Component, components::chat::sanitize_line, panel, theme,
};

/// One retained transcript row, owned so the component can re-fold on demand.
/// `clock` follows the library's ordering rule: Bedrock `global_sequence` when
/// confirmed, else the local `seq` as the pending-only fallback.
#[derive(Debug, Clone)]
pub struct BoardRow {
    pub clock: EventClock,
    pub author_id: String,
    pub body: String,
}

impl BoardRow {
    pub fn from_message(message: &ocean_core::RoomMessage) -> Self {
        Self {
            clock: message
                .federated
                .as_ref()
                .map(|meta| EventClock::Confirmed(meta.global_sequence))
                .unwrap_or(EventClock::Pending(message.seq)),
            author_id: message.author_id.clone(),
            body: message.body.clone(),
        }
    }

    fn as_event(&self) -> BoardEvent<'_> {
        BoardEvent {
            clock: self.clock,
            author_id: &self.author_id,
            body: &self.body,
        }
    }
}

/// What the one-line input mode is collecting.
#[derive(Debug, Clone, PartialEq, Eq)]
enum InputKind {
    /// Title for a new card in the named column.
    NewCard { column: String },
    /// Replacement title for the card.
    Retitle { card_id: String },
    /// Assignee for the card (blank clears).
    Assign { card_id: String },
    /// Comment text for the card.
    Comment { card_id: String },
}

impl InputKind {
    fn prompt(&self) -> String {
        match self {
            Self::NewCard { column } => format!("new card in {column}"),
            Self::Retitle { .. } => "retitle card".to_string(),
            Self::Assign { .. } => "assign (blank clears)".to_string(),
            Self::Comment { .. } => "comment".to_string(),
        }
    }
}

pub struct BoardComponent {
    room_key: Option<String>,
    generation: u64,
    rows: Vec<BoardRow>,
    board: Board,    loading: bool,
    tail_gap: bool,
    error: Option<String>,
    /// Selected column index into `display_columns()` (board columns plus an
    /// optional trailing "closed" pseudo-column).
    col: usize,
    /// Selected card index within the selected column's card list.
    card: usize,
    detail: bool,
    input: Option<(InputKind, String)>,
    /// Draft committed and awaiting its POST outcome; restored on failure so
    /// nothing typed is lost.
    pending_draft: Option<(InputKind, String)>,
    pub focused: bool,
}

impl Default for BoardComponent {
    fn default() -> Self {
        Self {
            room_key: None,
            generation: 0,
            rows: Vec::new(),
            // An empty fold is the empty board (default columns, no cards).
            board: ocean_board::project(std::iter::empty()),
            loading: false,
            tail_gap: false,
            error: None,
            col: 0,
            card: 0,
            detail: false,
            input: None,
            pending_draft: None,
            focused: false,
        }
    }
}

impl BoardComponent {
    /// Begin (re)hydrating a room's board. Called by the app when it accepts
    /// `Action::OpenBoard`; the data arrives later as `Action::BoardHydrated`.
    pub fn begin(&mut self, room_key: String, generation: u64) {
        self.room_key = Some(room_key);
        self.generation = generation;
        self.rows.clear();
        // An empty fold is the empty board (default columns, no cards).
        self.board = ocean_board::project(std::iter::empty());
        self.loading = true;
        self.tail_gap = false;
        self.error = None;
        self.col = 0;
        self.card = 0;
        self.detail = false;
        self.input = None;
        self.pending_draft = None;
    }

    /// The room this board is bound to (the app posts card ops here).
    pub fn room_key(&self) -> Option<&str> {
        self.room_key.as_deref()
    }

    /// True while the one-line input owns keys — Esc must cancel the input
    /// rather than leave the board surface.
    pub fn has_open_input(&self) -> bool {
        self.input.is_some()
    }

    fn refold(&mut self) {
        self.board = ocean_board::project(self.rows.iter().map(BoardRow::as_event));
        self.clamp_selection();
    }

    /// Column names in display order, plus a trailing "closed" pseudo-column
    /// when archived cards exist (reopen lives there).
    fn display_columns(&self) -> Vec<String> {
        let mut columns = self.board.columns.clone();
        if !self.board.closed().is_empty() {
            columns.push("closed".to_string());
        }
        columns
    }

    fn is_closed_column(&self, columns: &[String], index: usize) -> bool {
        index == columns.len() - 1 && columns.last().is_some_and(|c| c == "closed")
    }

    /// Cards of the selected display column, in projection order.
    fn column_cards(&self) -> Vec<&ocean_board::Card> {
        let columns = self.display_columns();
        let Some(name) = columns.get(self.col) else {
            return Vec::new();
        };
        if self.is_closed_column(&columns, self.col) {
            self.board.closed()
        } else {
            self.board.column(name)
        }
    }

    fn selected_card(&self) -> Option<&ocean_board::Card> {
        self.column_cards().get(self.card).copied()
    }

    fn clamp_selection(&mut self) {
        let columns = self.display_columns();
        if columns.is_empty() {
            self.col = 0;
            self.card = 0;
            return;
        }
        self.col = self.col.min(columns.len() - 1);
        let len = self.column_cards().len();
        self.card = if len == 0 { 0 } else { self.card.min(len - 1) };
    }

    /// Move the selected card one display column left/right. Never targets or
    /// leaves the "closed" pseudo-column (archive state is `x`/`u` only).
    fn move_selected(&mut self, delta: isize) -> Option<Action> {
        let columns = self.display_columns();
        if self.is_closed_column(&columns, self.col) {
            return Some(Action::Status(
                "reopen the card before moving it (u)".to_string(),
            ));
        }
        let card = self.selected_card()?;
        let target = (self.col as isize + delta).clamp(0, self.board.columns.len() as isize - 1)
            as usize;
        let column = self.board.columns.get(target)?.clone();
        if column == card.column {
            return None;
        }
        self.post(CardEnvelope::new(
            card.id.clone(),
            CardOp::Move { column },
        ))
    }

    /// Encode and emit a card op. Encode errors surface inline and never post.
    fn post(&mut self, envelope: CardEnvelope) -> Option<Action> {
        match envelope.encode() {
            Ok(body) => Some(Action::BoardPostCard { body }),
            Err(error) => {
                self.error = Some(error.to_string());
                None
            }
        }
    }

    fn commit_input(&mut self) -> Option<Action> {
        let (kind, text) = self.input.take()?;
        let text = text.trim().to_string();
        let envelope = match &kind {
            InputKind::NewCard { column } => CardEnvelope::new(
                uuid::Uuid::new_v4().to_string(),
                CardOp::Create {
                    title: text.clone(),
                    column: column.clone(),
                },
            ),
            InputKind::Retitle { card_id } => CardEnvelope::new(
                card_id.clone(),
                CardOp::Retitle { title: text.clone() },
            ),
            InputKind::Assign { card_id } => CardEnvelope::new(
                card_id.clone(),
                CardOp::Assign {
                    assignee: if text.is_empty() {
                        None
                    } else {
                        Some(text.clone())
                    },
                },
            ),
            InputKind::Comment { card_id } => CardEnvelope::new(
                card_id.clone(),
                CardOp::Comment { text: text.clone() },
            ),
        };
        match envelope.encode() {
            Ok(body) => {
                self.pending_draft = Some((kind, text));
                Some(Action::BoardPostCard { body })
            }
            Err(error) => {
                // Validation happens on encode; keep the draft open so the
                // operator can fix a blank/over-long field instead of losing it.
                self.error = Some(error.to_string());
                self.input = Some((kind, text));
                None
            }
        }
    }

    fn handle_input_key(&mut self, key: KeyEvent) -> Option<Action> {
        match key.code {
            KeyCode::Esc => {
                self.input = None;
                None
            }
            KeyCode::Enter => self.commit_input(),
            KeyCode::Backspace => {
                if let Some((_, text)) = &mut self.input {
                    text.pop();
                }
                None
            }
            KeyCode::Char(c) => {
                if let Some((_, text)) = &mut self.input {
                    text.push(c);
                }
                None
            }
            _ => None,
        }
    }
}

impl Component for BoardComponent {
    fn handle_key(&mut self, key: KeyEvent) -> Option<Action> {
        self.error = None;
        if self.input.is_some() {
            return self.handle_input_key(key);
        }
        if self.loading {
            return None;
        }
        match key.code {
            KeyCode::Left | KeyCode::Char('h') => {
                self.col = self.col.saturating_sub(1);
                self.card = 0;
                self.detail = false;
            }
            KeyCode::Right | KeyCode::Char('l') => {
                let columns = self.display_columns();
                if !columns.is_empty() {
                    self.col = (self.col + 1).min(columns.len() - 1);
                }
                self.card = 0;
                self.detail = false;
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.card = self.card.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                let len = self.column_cards().len();
                if len > 0 {
                    self.card = (self.card + 1).min(len - 1);
                }
            }
            KeyCode::Enter => {
                if self.selected_card().is_some() {
                    self.detail = !self.detail;
                }
            }
            KeyCode::Char('n') => {
                let columns = self.display_columns();
                if self.is_closed_column(&columns, self.col) {
                    return Some(Action::Status(
                        "new cards start in a board column, not the archive".to_string(),
                    ));
                }
                if let Some(column) = columns.get(self.col) {
                    self.input = Some((
                        InputKind::NewCard {
                            column: column.clone(),
                        },
                        String::new(),
                    ));
                }
            }
            KeyCode::Char('r') => {
                if let Some(card) = self.selected_card() {
                    self.input = Some((
                        InputKind::Retitle {
                            card_id: card.id.clone(),
                        },
                        card.title.clone(),
                    ));
                }
            }
            KeyCode::Char('a') => {
                if let Some(card) = self.selected_card() {
                    self.input = Some((
                        InputKind::Assign {
                            card_id: card.id.clone(),
                        },
                        card.assignee.clone().unwrap_or_default(),
                    ));
                }
            }
            KeyCode::Char('c') => {
                if let Some(card) = self.selected_card() {
                    self.input = Some((
                        InputKind::Comment {
                            card_id: card.id.clone(),
                        },
                        String::new(),
                    ));
                }
            }
            KeyCode::Char('x') => {
                if let Some(card) = self.selected_card() {
                    if !card.closed {
                        return self.post(CardEnvelope::new(card.id.clone(), CardOp::Close));
                    }
                }
            }
            KeyCode::Char('u') => {
                if let Some(card) = self.selected_card() {
                    if card.closed {
                        return self.post(CardEnvelope::new(card.id.clone(), CardOp::Reopen));
                    }
                }
            }
            KeyCode::Char('H') => return self.move_selected(-1),
            KeyCode::Char('L') => return self.move_selected(1),
            _ => {}
        }
        None
    }

    fn update(&mut self, action: &Action) -> Option<Action> {
        match action {
            Action::BoardHydrated {
                generation,
                room_key,
                rows,
                ..
            } if *generation == self.generation => {
                self.room_key = Some(room_key.clone());
                self.rows = rows.clone();
                self.loading = false;
                self.tail_gap = false;
                self.refold();
            }
            Action::BoardRoomMessage { generation, message }
                if *generation == self.generation && !self.loading =>
            {
                self.rows.push(BoardRow::from_message(message));
                self.tail_gap = false;
                self.refold();
            }
            Action::BoardStreamGap { generation } if *generation == self.generation => {
                self.tail_gap = true;
            }
            Action::BoardPostFinished { generation, result }
                if *generation == self.generation =>
            {
                match result {
                    Ok(()) => self.pending_draft = None,
                    Err(error) => {
                        // Restore the draft so nothing typed is lost; the SSE
                        // echo — never this response — is what changes the board.
                        self.input = self.pending_draft.take();
                        self.error = Some(error.clone());
                    }
                }
            }
            Action::BoardOpFailed { generation, message }
                if *generation == self.generation =>
            {
                self.loading = false;
                self.error = Some(message.clone());
            }
            _ => {}
        }
        None
    }

    fn draw(&mut self, frame: &mut Frame, area: Rect) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        self.clamp_selection();
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Min(1),
                Constraint::Length(1),
            ])
            .split(area);

        // Header: room identity, truthful counts, and the unsupported-event
        // gap said out loud instead of silently rendering a partial board.
        let open: usize = (0..self.board.columns.len())
            .map(|i| self.board.column(&self.board.columns[i]).len())
            .sum();
        let closed = self.board.closed().len();
        let mut header = vec![
            Span::styled("board", Style::default().fg(theme::CYAN)),
            Span::styled(
                format!(
                    " · {} · {open} open · {closed} closed",
                    self.room_key.as_deref().unwrap_or("…")
                ),
                Style::default().fg(theme::COMMENT),
            ),
        ];
        if self.loading {
            header.push(Span::styled(
                " · folding transcript…",
                Style::default().fg(theme::YELLOW),
            ));
        }
        if self.tail_gap {
            header.push(Span::styled(
                " · tail reconnecting",
                Style::default().fg(theme::YELLOW),
            ));
        }
        if self.board.unsupported_events > 0 {
            header.push(Span::styled(
                format!(
                    " · {} card event(s) from a newer schema not shown",
                    self.board.unsupported_events
                ),
                Style::default().fg(theme::YELLOW).add_modifier(Modifier::BOLD),
            ));
        }
        frame.render_widget(Paragraph::new(Line::from(header)), chunks[0]);

        // Columns (+ optional trailing "closed" pseudo-column).
        let columns = self.display_columns();
        if !columns.is_empty() {
            let col_areas = Layout::default()
                .direction(Direction::Horizontal)
                .constraints(vec![Constraint::Ratio(1, columns.len() as u32); columns.len()])
                .split(chunks[1]);
            let detail_open = self.detail && self.selected_card().is_some();
            for (index, name) in columns.iter().enumerate() {
                let cards: Vec<&ocean_board::Card> = if self.is_closed_column(&columns, index) {
                    self.board.closed()
                } else {
                    self.board.column(name)
                };
                let mut lines = Vec::new();
                for (card_index, card) in cards.iter().enumerate() {
                    let selected = index == self.col && card_index == self.card;
                    let title = sanitize_line(&card.title);
                    let title = if title.is_empty() {
                        "(untitled — create event not yet seen)".to_string()
                    } else {
                        title
                    };
                    let mut spans = vec![Span::styled(
                        panel::fit_cells(&title, col_areas[index].width.saturating_sub(4) as usize),
                        if selected {
                            Style::default().fg(theme::FG).bg(theme::BG_HL)
                        } else {
                            Style::default().fg(theme::FG)
                        },
                    )];
                    if let Some(assignee) = &card.assignee {
                        spans.push(Span::styled(
                            format!(" @{assignee}"),
                            Style::default().fg(theme::BLUE),
                        ));
                    }
                    if !card.comments.is_empty() {
                        spans.push(Span::styled(
                            format!(" ·{}", card.comments.len()),
                            Style::default().fg(theme::COMMENT),
                        ));
                    }
                    lines.push(Line::from(spans));
                }
                if cards.is_empty() {
                    lines.push(Line::from(Span::styled(
                        "·",
                        Style::default().fg(theme::COMMENT),
                    )));
                }
                let title_style = if index == self.col {
                    Style::default().fg(theme::CYAN)
                } else {
                    Style::default().fg(theme::COMMENT)
                };
                let block = Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(theme::EDGE))
                    .title(Span::styled(
                        format!(" {name} ({}) ", cards.len()),
                        title_style,
                    ));
                frame.render_widget(Paragraph::new(lines).block(block), col_areas[index]);
            }

            // Card detail drawer: bottom third of the selected column's area.
            if detail_open {
                if let Some(card) = self.selected_card() {
                    let col_area = col_areas[self.col];
                    let detail_h = (col_area.height / 3).max(4).min(col_area.height);
                    let detail_area = Rect::new(
                        col_area.x,
                        col_area.y + col_area.height - detail_h,
                        col_area.width,
                        detail_h,
                    );
                    let mut lines = vec![
                        Line::from(Span::styled(
                            sanitize_line(&card.title),
                            Style::default().fg(theme::FG).add_modifier(Modifier::BOLD),
                        )),
                        Line::from(Span::styled(
                            format!(
                                "id {} · by {} · {}",
                                card.id,
                                card.created_by,
                                match &card.assignee {
                                    Some(a) => format!("assigned @{a}"),
                                    None => "unassigned".to_string(),
                                }
                            ),
                            Style::default().fg(theme::COMMENT),
                        )),
                    ];
                    for comment in &card.comments {
                        lines.push(Line::from(vec![
                            Span::styled(
                                format!("{}: ", comment.author_id),
                                Style::default().fg(theme::BLUE),
                            ),
                            Span::styled(
                                sanitize_line(&comment.text),
                                Style::default().fg(theme::FG),
                            ),
                        ]));
                    }
                    let block = Block::default()
                        .borders(Borders::TOP)
                        .border_style(Style::default().fg(theme::EDGE));
                    frame.render_widget(Paragraph::new(lines).block(block), detail_area);
                }
            }
        }

        // Footer: input prompt while collecting, else the key legend.
        let footer = if let Some((kind, text)) = &self.input {
            Line::from(vec![
                Span::styled(
                    format!("{}: ", kind.prompt()),
                    Style::default().fg(theme::CYAN),
                ),
                Span::styled(sanitize_line(text), Style::default().fg(theme::FG)),
                Span::styled("▏", Style::default().fg(theme::CYAN)),
            ])
        } else if let Some(error) = &self.error {
            Line::from(Span::styled(
                sanitize_line(error),
                Style::default().fg(theme::ORANGE),
            ))
        } else {
            Line::from(Span::styled(
                "n new · H/L move · r retitle · a assign · c comment · x close · u reopen · Enter detail · Esc chat",
                Style::default().fg(theme::COMMENT),
            ))
        };
        frame.render_widget(Paragraph::new(footer), chunks[2]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ocean_board::project;

    fn row(clock: u64, author: &str, envelope: CardEnvelope) -> BoardRow {
        BoardRow {
            clock: EventClock::Confirmed(clock),
            author_id: author.to_string(),
            body: envelope.encode().expect("fixture envelopes are valid"),
        }
    }

    fn chat(clock: u64, author: &str, text: &str) -> BoardRow {
        BoardRow {
            clock: EventClock::Confirmed(clock),
            author_id: author.to_string(),
            body: text.to_string(),
        }
    }

    fn fold(rows: &[BoardRow]) -> Board {
        project(rows.iter().map(BoardRow::as_event))
    }

    /// The phase-2 contract: folding a hydrate prefix and then the live tail
    /// through `project` — the component's exact update path — must equal a
    /// full replay, including when arrival order is scrambled.
    #[test]
    fn hydrate_plus_tail_equals_full_replay() {
        let card_a = "11111111-1111-4111-8111-111111111111";
        let card_b = "22222222-2222-4222-8222-222222222222";
        let hydrate = vec![
            chat(1, "ec", "plain chat never becomes a card"),
            row(
                2,
                "ec",
                CardEnvelope::new(
                    card_a,
                    CardOp::Create {
                        title: "write the plan".into(),
                        column: "backlog".into(),
                    },
                ),
            ),
            row(3, "ec", CardEnvelope::new(card_a, CardOp::Move { column: "doing".into() })),
        ];
        let tail = vec![
            row(
                4,
                "deepseek",
                CardEnvelope::new(
                    card_b,
                    CardOp::Create {
                        title: "review it".into(),
                        column: "review".into(),
                    },
                ),
            ),
            row(
                5,
                "deepseek",
                CardEnvelope::new(card_a, CardOp::Comment { text: "lgtm".into() }),
            ),
            row(6, "ec", CardEnvelope::new(card_a, CardOp::Close)),
            chat(7, "fable", "nice board"),
        ];

        // The component's path: rows retained in arrival order, re-folded whole.
        let mut incremental = hydrate.clone();
        incremental.extend(tail.clone());
        let via_tail = fold(&incremental);

        // Full replay, scrambled: every event still resolves per-field LWW.
        let shuffled = vec![
            tail[2].clone(),
            hydrate[1].clone(),
            tail[0].clone(),
            hydrate[0].clone(),
            tail[3].clone(),
            hydrate[2].clone(),
            tail[1].clone(),
        ];
        let via_replay = fold(&shuffled);

        assert_eq!(via_tail, via_replay);
        let card = &via_tail.cards[card_a];
        assert!(card.closed, "the later close wins over the earlier move");
        assert_eq!(card.column, "doing", "close preserves the card's column");
        assert_eq!(card.comments.len(), 1, "chat bodies never become comments");
        assert_eq!(via_tail.unsupported_events, 0);
    }

    /// A tagged envelope this build cannot apply is counted, never folded into
    /// chat or dropped silently.
    #[test]
    fn unsupported_envelopes_are_counted_not_absorbed() {
        let rows = vec![
            chat(1, "ec", "hello"),
            BoardRow {
                clock: EventClock::Confirmed(2),
                author_id: "newer-seat".into(),
                body: r#"{"kind":"ocean.board.card","v":99,"card_id":"x","op":"teleport"}"#
                    .to_string(),
            },
        ];
        let board = fold(&rows);
        assert_eq!(board.unsupported_events, 1);
        assert!(board.cards.is_empty());
    }

    /// Pending (local, unconfirmed) events sort after every confirmed one —
    /// the documented pending-only fallback for local-only rooms.
    #[test]
    fn pending_clock_settles_after_confirmation() {
        let card = "33333333-3333-4333-8333-333333333333";
        let pending = BoardRow {
            clock: EventClock::Pending(7),
            author_id: "ec".into(),
            body: CardEnvelope::new(card, CardOp::Move { column: "done".into() })
                .encode()
                .unwrap(),
        };
        let confirmed = row(
            5,
            "ec",
            CardEnvelope::new(
                card,
                CardOp::Create {
                    title: "ship it".into(),
                    column: "doing".into(),
                },
            ),
        );
        let board = fold(&[pending, confirmed]);
        assert_eq!(
            board.cards[card].column, "done",
            "the pending move is newer than the confirmed create and wins"
        );
    }
}
