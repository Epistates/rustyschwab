//! Drawing the dashboard: amber on black, gains green, losses red.

use chrono::Utc;
use chrono_tz::America::New_York;
use ratatui::Frame;
use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, BorderType, Cell, Clear, Paragraph, Row, Table, TableState, Wrap};
use rustyschwab_mcp::delegation::Mode;

use crate::app::{App, Modal, ModeChoice, Pane, Purpose};

const AMBER: Color = Color::Rgb(255, 176, 0);
const DIM: Color = Color::Rgb(150, 105, 0);
const BLACK: Color = Color::Rgb(0, 0, 0);
const GREEN: Color = Color::Rgb(0, 210, 90);
const RED: Color = Color::Rgb(235, 60, 60);
const CYAN: Color = Color::Rgb(0, 190, 220);
const WHITE: Color = Color::Rgb(230, 230, 230);

fn amber() -> Style {
    Style::new().fg(AMBER).bg(BLACK)
}

fn signed(value: Option<f64>) -> Style {
    match value {
        Some(v) if v > 0.0 => Style::new().fg(GREEN),
        Some(v) if v < 0.0 => Style::new().fg(RED),
        _ => Style::new().fg(WHITE),
    }
}

fn money(value: Option<f64>) -> String {
    value.map_or_else(|| "-".into(), |v| format!("{v:.2}"))
}

/// A pane's frame; `key` is the digit that focuses it, if any.
fn pane(title: &str, key: Option<char>, focused: bool) -> Block<'static> {
    let style = if focused {
        Style::new()
            .fg(BLACK)
            .bg(AMBER)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::new().fg(AMBER).add_modifier(Modifier::BOLD)
    };
    Block::bordered()
        .border_type(BorderType::Plain)
        .border_style(Style::new().fg(if focused { AMBER } else { DIM }))
        .title(Span::styled(
            match key {
                Some(key) => format!(" {key} {title} "),
                None => format!(" {title} "),
            },
            style,
        ))
        .style(amber())
}

fn mode_label(mode: &Mode) -> String {
    match mode {
        Mode::Supervised => "SUPERVISED".into(),
        Mode::Threshold { above } => format!("APPROVE>${above}"),
        Mode::Autonomous => "AUTONOMOUS".into(),
    }
}

fn mode_style(mode: &Mode) -> Style {
    match mode {
        Mode::Supervised => Style::new().fg(GREEN),
        Mode::Threshold { .. } => Style::new().fg(AMBER),
        Mode::Autonomous => Style::new().fg(RED).add_modifier(Modifier::BOLD),
    }
}

fn outcome_style(outcome: &str) -> Style {
    match outcome {
        "ok" => Style::new().fg(GREEN),
        "approval_required" => Style::new().fg(AMBER).add_modifier(Modifier::BOLD),
        _ => Style::new().fg(RED),
    }
}

pub fn render(frame: &mut Frame<'_>, app: &App) {
    frame.render_widget(Block::new().style(amber()), frame.area());
    let [header, agents, body, bottom, footer] = Layout::vertical([
        Constraint::Length(1),
        // Borders and the column header, then one row per agent.
        Constraint::Length(app.snapshot.principals.len().clamp(1, 6) as u16 + 3),
        Constraint::Fill(3),
        Constraint::Fill(2),
        Constraint::Length(1),
    ])
    .areas(frame.area());
    render_header(frame, app, header);
    render_agents(frame, app, agents);
    let [tape, side] =
        Layout::horizontal([Constraint::Percentage(58), Constraint::Percentage(42)]).areas(body);
    render_tape(frame, app, tape);
    let [approvals, orders] =
        Layout::vertical([Constraint::Fill(1), Constraint::Fill(2)]).areas(side);
    render_approvals(frame, app, approvals);
    render_orders(frame, app, orders);
    let [positions, quotes, account] = Layout::horizontal([
        Constraint::Percentage(45),
        Constraint::Percentage(30),
        Constraint::Percentage(25),
    ])
    .areas(bottom);
    render_positions(frame, app, positions);
    render_quotes(frame, app, quotes);
    render_account(frame, app, account);
    render_footer(frame, app, footer);
    if let Some(modal) = &app.modal {
        render_modal(frame, app, modal);
    }
}

fn render_header(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let s = &app.snapshot;
    let clock = Utc::now()
        .with_timezone(&New_York)
        .format("%a %b %d %H:%M:%S ET");
    let bar = Style::new()
        .fg(BLACK)
        .bg(AMBER)
        .add_modifier(Modifier::BOLD);
    let mut spans = vec![
        Span::styled(" RUSTYSCHWAB ", bar),
        Span::styled(
            format!(" {} ", s.mode.to_uppercase()),
            Style::new()
                .fg(BLACK)
                .bg(if s.mode == "live" { RED } else { CYAN }),
        ),
        Span::styled(format!("  {clock}  "), Style::new().fg(WHITE)),
    ];
    match &s.halt {
        Some(reason) => spans.push(Span::styled(
            format!(" HALTED: {reason} "),
            Style::new().fg(WHITE).bg(RED).add_modifier(Modifier::BOLD),
        )),
        None => spans.push(Span::styled(" TRADING ", Style::new().fg(BLACK).bg(GREEN))),
    }
    spans.push(Span::styled(
        format!("  operator {}  ", s.operator),
        Style::new().fg(DIM),
    ));
    if !s.warnings.is_empty() {
        spans.push(Span::styled(
            format!(" {} warning(s): {} ", s.warnings.len(), s.warnings[0]),
            Style::new().fg(RED),
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)).style(amber()), area);
}

fn render_agents(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let focused = app.focus == Pane::Agents;
    let rows: Vec<Row<'_>> = app
        .snapshot
        .principals
        .iter()
        .map(|p| {
            let until = p.delegation.as_ref().map_or_else(
                || "policy".to_owned(),
                |g| {
                    g.until
                        .and_then(chrono::DateTime::from_timestamp_millis)
                        .map_or_else(
                            || format!("by {} GTC", g.operator),
                            |at| {
                                format!(
                                    "by {} to {}",
                                    g.operator,
                                    at.with_timezone(&New_York).format("%H:%M")
                                )
                            },
                        )
                },
            );
            let dead_man = p.dead_man.as_ref().map_or_else(
                || Cell::from("-"),
                |d| {
                    let style = if d.seconds_left < 60 {
                        Style::new().fg(RED).add_modifier(Modifier::BOLD)
                    } else {
                        Style::new().fg(WHITE)
                    };
                    Cell::from(Span::styled(
                        format!("{}:{:02}", d.seconds_left / 60, d.seconds_left % 60),
                        style,
                    ))
                },
            );
            let budget = format!(
                "{}/{} orders  ${}/{}",
                p.budget.orders_used,
                p.budget.max_orders.map_or("∞".into(), |m| m.to_string()),
                p.budget.notional_used.round_dp(0),
                p.budget
                    .max_notional
                    .map_or("∞".into(), |m| m.round_dp(0).to_string()),
            );
            let heard = p.last_call.map_or_else(
                || {
                    if p.active {
                        "-".into()
                    } else {
                        "never ran".into()
                    }
                },
                |at| {
                    let ago = (Utc::now() - at).num_seconds().max(0);
                    format!("{ago}s ago")
                },
            );
            Row::new(vec![
                Cell::from(Span::styled(
                    p.name.clone(),
                    Style::new().fg(CYAN).add_modifier(Modifier::BOLD),
                )),
                Cell::from(Span::styled(
                    mode_label(&p.approval),
                    mode_style(&p.approval),
                )),
                Cell::from(until),
                Cell::from(format!("max {}", mode_label(&p.ceiling).to_lowercase())),
                dead_man,
                Cell::from(budget),
                Cell::from(format!("{} pending", p.pending_submissions)),
                Cell::from(heard),
            ])
        })
        .collect();
    let table = Table::new(
        rows,
        [
            Constraint::Length(14),
            Constraint::Length(16),
            Constraint::Length(20),
            Constraint::Length(18),
            Constraint::Length(7),
            Constraint::Length(28),
            Constraint::Length(10),
            Constraint::Fill(1),
        ],
    )
    .header(
        Row::new([
            "AGENT",
            "APPROVAL",
            "DELEGATED",
            "CEILING",
            "DEADMAN",
            "BUDGET TODAY",
            "",
            "LAST CALL",
        ])
        .style(Style::new().fg(DIM)),
    )
    .row_highlight_style(Style::new().add_modifier(Modifier::REVERSED))
    .block(pane("AGENTS", Some('4'), focused));
    let mut state = TableState::new().with_selected(focused.then_some(app.agent));
    frame.render_stateful_widget(table, area, &mut state);
}

fn render_tape(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let focused = app.focus == Pane::Tape;
    let [list, detail] = Layout::vertical([Constraint::Fill(1), Constraint::Length(5)]).areas(area);
    let rows: Vec<Row<'_>> = app
        .snapshot
        .activity
        .iter()
        .rev()
        .map(|a| {
            Row::new(vec![
                Cell::from(a.at.with_timezone(&New_York).format("%H:%M:%S").to_string()),
                Cell::from(Span::styled(a.principal.clone(), Style::new().fg(CYAN))),
                Cell::from(Span::styled(a.tool.clone(), Style::new().fg(WHITE))),
                Cell::from(Span::styled(a.outcome.clone(), outcome_style(&a.outcome))),
                Cell::from(a.summary.clone()),
            ])
        })
        .collect();
    let table = Table::new(
        rows,
        [
            Constraint::Length(8),
            Constraint::Length(12),
            Constraint::Length(20),
            Constraint::Length(18),
            Constraint::Fill(1),
        ],
    )
    .header(Row::new(["TIME", "AGENT", "TOOL", "OUTCOME", "CALL"]).style(Style::new().fg(DIM)))
    .row_highlight_style(Style::new().add_modifier(Modifier::REVERSED))
    .block(pane("AGENT TAPE", Some('1'), focused));
    let mut state =
        TableState::new().with_selected((!app.snapshot.activity.is_empty()).then_some(app.tape));
    frame.render_stateful_widget(table, list, &mut state);

    let selected = app.snapshot.activity.iter().rev().nth(app.tape);
    let text = match selected {
        Some(a) => {
            let mut lines = vec![Line::from(vec![
                Span::styled(
                    format!("{} {} ", a.principal, a.tool),
                    Style::new().fg(CYAN),
                ),
                Span::styled(
                    format!("{} in {} ms", a.outcome, a.duration_ms),
                    outcome_style(&a.outcome),
                ),
                Span::raw(
                    a.order_id
                        .as_ref()
                        .map_or(String::new(), |id| format!("  order {id}")),
                ),
            ])];
            if let Some(rationale) = &a.rationale {
                lines.push(Line::from(vec![
                    Span::styled("WHY ", Style::new().fg(DIM)),
                    Span::styled(rationale.clone(), Style::new().fg(WHITE)),
                ]));
            }
            if let Some(message) = &a.message {
                lines.push(Line::from(Span::styled(
                    message.clone(),
                    Style::new().fg(RED),
                )));
            }
            Text::from(lines)
        }
        None => Text::from(Span::styled(
            "Waiting for the agents' first call",
            Style::new().fg(DIM),
        )),
    };
    frame.render_widget(
        Paragraph::new(text).wrap(Wrap { trim: true }).block(
            Block::bordered()
                .border_style(Style::new().fg(DIM))
                .style(amber()),
        ),
        detail,
    );
}

fn render_approvals(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let focused = app.focus == Pane::Approvals;
    let count = app.snapshot.approvals.len();
    let title = if count > 0 {
        format!("APPROVALS ({count} WAITING)")
    } else {
        "APPROVALS".into()
    };
    let rows: Vec<Row<'_>> = app
        .snapshot
        .approvals
        .iter()
        .map(|a| {
            Row::new(vec![
                Cell::from(Span::styled(a.principal.clone(), Style::new().fg(CYAN))),
                Cell::from(Span::styled(a.description.clone(), Style::new().fg(WHITE))),
                Cell::from(a.rationale.clone().unwrap_or_default()),
            ])
        })
        .collect();
    let block = pane(&title, Some('2'), focused);
    let block = if count > 0 {
        block.border_style(Style::new().fg(AMBER).add_modifier(Modifier::BOLD))
    } else {
        block
    };
    let table = Table::new(
        rows,
        [
            Constraint::Length(12),
            Constraint::Fill(2),
            Constraint::Fill(1),
        ],
    )
    .header(Row::new(["AGENT", "ORDER", "WHY"]).style(Style::new().fg(DIM)))
    .row_highlight_style(Style::new().add_modifier(Modifier::REVERSED))
    .block(block);
    let mut state = TableState::new().with_selected((count > 0).then_some(app.approval));
    frame.render_stateful_widget(table, area, &mut state);
}

fn render_orders(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let focused = app.focus == Pane::Orders;
    let rows: Vec<Row<'_>> = app
        .snapshot
        .orders
        .iter()
        .map(|o| {
            let status = if o.working {
                Style::new().fg(AMBER).add_modifier(Modifier::BOLD)
            } else if o.status == "FILLED" {
                Style::new().fg(GREEN)
            } else {
                Style::new().fg(DIM)
            };
            let price = o
                .price
                .or(o.stop_price)
                .map_or_else(|| "MKT".into(), |p| format!("{p:.2}"));
            Row::new(vec![
                Cell::from(o.order_id.clone()),
                Cell::from(Span::styled(o.status.clone(), status)),
                Cell::from(Span::styled(o.legs.clone(), Style::new().fg(WHITE))),
                Cell::from(format!("{} {price}", o.order_type)),
                Cell::from(format!(
                    "{}/{}",
                    o.filled.map_or("-".into(), |f| f.to_string()),
                    o.quantity.map_or("-".into(), |q| q.to_string())
                )),
                Cell::from(Span::styled(
                    o.principal.clone().unwrap_or_else(|| "-".into()),
                    Style::new().fg(CYAN),
                )),
            ])
        })
        .collect();
    let table = Table::new(
        rows,
        [
            Constraint::Length(9),
            Constraint::Length(10),
            Constraint::Fill(1),
            Constraint::Length(14),
            Constraint::Length(7),
            Constraint::Length(10),
        ],
    )
    .header(
        Row::new(["ID", "STATUS", "LEGS", "TYPE", "FILLED", "AGENT"]).style(Style::new().fg(DIM)),
    )
    .row_highlight_style(Style::new().add_modifier(Modifier::REVERSED))
    .block(pane("ORDERS", Some('3'), focused));
    let mut state = TableState::new()
        .with_selected((focused && !app.snapshot.orders.is_empty()).then_some(app.order));
    frame.render_stateful_widget(table, area, &mut state);
}

fn render_positions(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let rows: Vec<Row<'_>> = app
        .snapshot
        .accounts
        .iter()
        .flat_map(|account| {
            account.positions.iter().map(move |p| {
                Row::new(vec![
                    Cell::from(account.number.clone()),
                    Cell::from(Span::styled(p.symbol.clone(), Style::new().fg(CYAN))),
                    Cell::from(p.quantity.to_string()),
                    Cell::from(money(p.average_price)),
                    Cell::from(money(p.market_value)),
                    Cell::from(Span::styled(money(p.day_pnl), signed(p.day_pnl))),
                    Cell::from(Span::styled(money(p.open_pnl), signed(p.open_pnl))),
                ])
            })
        })
        .collect();
    let table = Table::new(
        rows,
        [
            Constraint::Length(7),
            Constraint::Length(8),
            Constraint::Length(8),
            Constraint::Length(10),
            Constraint::Length(11),
            Constraint::Length(10),
            Constraint::Length(10),
        ],
    )
    .header(
        Row::new([
            "ACCT", "SYMBOL", "QTY", "AVG", "VALUE", "DAY P&L", "OPEN P&L",
        ])
        .style(Style::new().fg(DIM)),
    )
    .block(pane("POSITIONS", None, false));
    frame.render_widget(table, area);
}

fn render_quotes(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let rows: Vec<Row<'_>> = app
        .snapshot
        .quotes
        .iter()
        .map(|q| {
            Row::new(vec![
                Cell::from(Span::styled(q.symbol.clone(), Style::new().fg(CYAN))),
                Cell::from(Span::styled(money(q.last), Style::new().fg(WHITE))),
                Cell::from(Span::styled(
                    q.change_percent.map_or("-".into(), |p| format!("{p:+.2}%")),
                    signed(q.change_percent),
                )),
                Cell::from(format!("{}x{}", money(q.bid), money(q.ask))),
            ])
        })
        .collect();
    let table = Table::new(
        rows,
        [
            Constraint::Length(7),
            Constraint::Length(9),
            Constraint::Length(8),
            Constraint::Fill(1),
        ],
    )
    .header(Row::new(["SYM", "LAST", "CHG", "BID x ASK"]).style(Style::new().fg(DIM)))
    .block(pane("QUOTES", None, false));
    frame.render_widget(table, area);
}

fn render_account(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let mut lines = Vec::new();
    for account in &app.snapshot.accounts {
        let label = |name: &str, value: Option<f64>, style: Style| {
            Line::from(vec![
                Span::styled(format!("{name:<9}"), Style::new().fg(DIM)),
                Span::styled(money(value), style),
            ])
        };
        lines.push(Line::from(Span::styled(
            format!("ACCOUNT {}", account.number),
            Style::new().fg(AMBER).add_modifier(Modifier::BOLD),
        )));
        lines.push(label(
            "VALUE",
            account.liquidation_value,
            Style::new().fg(WHITE),
        ));
        lines.push(label("CASH", account.cash, Style::new().fg(WHITE)));
        lines.push(label(
            "BUY PWR",
            account.buying_power,
            Style::new().fg(WHITE),
        ));
        lines.push(label("DAY P&L", account.day_pnl, signed(account.day_pnl)));
        lines.push(label(
            "OPEN P&L",
            account.open_pnl,
            signed(account.open_pnl),
        ));
        if let Some(at) = account.observed_at {
            let age = (Utc::now() - at).num_seconds().max(0);
            lines.push(Line::from(Span::styled(
                format!("as of {age}s ago"),
                Style::new().fg(DIM),
            )));
        }
    }
    if lines.is_empty() {
        lines.push(Line::from(Span::styled(
            "No account read yet",
            Style::new().fg(DIM),
        )));
    }
    frame.render_widget(
        Paragraph::new(lines).block(pane("ACCOUNT", None, false)),
        area,
    );
}

fn render_footer(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let key = |k: &str, label: &str| {
        vec![
            Span::styled(format!(" {k} "), Style::new().fg(BLACK).bg(AMBER)),
            Span::styled(format!(" {label} "), Style::new().fg(AMBER)),
        ]
    };
    let mut spans: Vec<Span<'_>> = [
        ("a", "approve"),
        ("x", "refuse"),
        ("d", "delegate"),
        ("u", "end deleg."),
        ("h", "halt"),
        ("r", "resume"),
        ("K", "kill"),
        ("?", "help"),
        ("q", "quit"),
    ]
    .iter()
    .flat_map(|(k, l)| key(k, l))
    .collect();
    if app.busy {
        spans.push(Span::styled(" working… ", Style::new().fg(AMBER)));
    } else if let Some(status) = &app.status {
        spans.push(Span::styled(
            format!(
                " {} {} ",
                status.at.with_timezone(&New_York).format("%H:%M:%S"),
                status.text
            ),
            Style::new().fg(if status.ok { GREEN } else { RED }),
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)).style(amber()), area);
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let [area] = Layout::horizontal([Constraint::Length(width.min(area.width))])
        .flex(Flex::Center)
        .areas(area);
    let [area] = Layout::vertical([Constraint::Length(height.min(area.height))])
        .flex(Flex::Center)
        .areas(area);
    area
}

fn render_modal(frame: &mut Frame<'_>, app: &App, modal: &Modal) {
    let (title, lines, height): (String, Vec<Line<'_>>, u16) = match modal {
        Modal::Confirm { title, body, .. } => {
            let mut lines: Vec<Line<'_>> = body.lines().map(|l| Line::from(l.to_owned())).collect();
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "[y] yes   [n] no",
                Style::new().fg(AMBER),
            )));
            let height = lines.len() as u16 + 4;
            (title.clone(), lines, height)
        }
        Modal::Prompt { purpose, text } => {
            let title = match purpose {
                Purpose::HaltReason => "Halt all trading",
                Purpose::KillReason => "KILL: halt and cancel every working order",
            };
            (
                title.into(),
                vec![
                    Line::from("Reason (recorded in the audit trail):"),
                    Line::from(Span::styled(format!("> {text}_"), Style::new().fg(WHITE))),
                    Line::from(""),
                    Line::from(Span::styled(
                        "[Enter] continue   [Esc] cancel",
                        Style::new().fg(AMBER),
                    )),
                ],
                8,
            )
        }
        Modal::KillConfirm { reason, typed } => (
            "KILL".into(),
            vec![
                Line::from(format!("Reason: {reason}")),
                Line::from(Span::styled(
                    "Halts every server, then cancels every working order on every account shown.",
                    Style::new().fg(RED),
                )),
                Line::from("Type kill and press Enter to confirm:"),
                Line::from(Span::styled(format!("> {typed}_"), Style::new().fg(WHITE))),
                Line::from(Span::styled("[Esc] cancel", Style::new().fg(AMBER))),
            ],
            9,
        ),
        Modal::Delegate(form) => {
            let ceiling = app
                .snapshot
                .principals
                .iter()
                .find(|p| p.name == form.principal)
                .map(|p| mode_label(&p.ceiling).to_lowercase())
                .unwrap_or_default();
            let row = |index: usize, label: &str, value: String| {
                let style = if form.row == index {
                    Style::new().fg(BLACK).bg(AMBER)
                } else {
                    Style::new().fg(WHITE)
                };
                Line::from(vec![
                    Span::styled(format!("{label:<11}"), Style::new().fg(DIM)),
                    Span::styled(format!(" {value} "), style),
                ])
            };
            let mode = match form.mode {
                ModeChoice::Supervised => "◀ supervised: approve every order ▶",
                ModeChoice::Threshold => "◀ threshold: approve orders over $ ▶",
                ModeChoice::Autonomous => "◀ autonomous: no approvals ▶",
            };
            (
                format!("Delegate to {}", form.principal),
                vec![
                    row(0, "Mode", mode.into()),
                    row(
                        1,
                        "Over $",
                        if form.mode == ModeChoice::Threshold {
                            format!("{}_", form.amount)
                        } else {
                            "-".into()
                        },
                    ),
                    row(2, "Until", format!("◀ {} ▶", form.until.label())),
                    Line::from(""),
                    Line::from(Span::styled(
                        format!(
                            "The policy allows at most: {ceiling}. Risk limits apply in every mode."
                        ),
                        Style::new().fg(DIM),
                    )),
                    Line::from(Span::styled(
                        "[↑↓] field  [←→] change  [Enter] grant  [Esc] cancel",
                        Style::new().fg(AMBER),
                    )),
                ],
                10,
            )
        }
        Modal::Help => (
            "Keys".into(),
            [
                "Tab / 1-4   move between tape, approvals, orders, agents",
                "↑↓ j k      select;  g  first",
                "a / x       approve / refuse the selected waiting order",
                "d           delegate an approval mode to the selected agent",
                "u           end the agent's delegation (back to its policy)",
                "h / r       halt / resume all trading",
                "K           kill: halt, then cancel every working order",
                "q           quit (agents keep running)",
            ]
            .iter()
            .map(|l| Line::from(*l))
            .collect(),
            12,
        ),
    };
    let area = centered(frame.area(), 76, height);
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: false }).block(
            Block::bordered()
                .border_type(BorderType::Double)
                .border_style(Style::new().fg(AMBER))
                .title(Span::styled(
                    format!(" {title} "),
                    Style::new()
                        .fg(BLACK)
                        .bg(AMBER)
                        .add_modifier(Modifier::BOLD),
                ))
                .style(amber()),
        ),
        area,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use rustyschwab_console::Snapshot;
    use rustyschwab_console::snapshot::{ActivityView, ApprovalView};
    use std::sync::Arc;

    fn screen(app: &App) -> String {
        let mut terminal = Terminal::new(TestBackend::new(160, 45)).unwrap();
        terminal.draw(|frame| render(frame, app)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        let mut text = String::new();
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                text.push_str(buffer[(x, y)].symbol());
            }
            text.push('\n');
        }
        text
    }

    #[test]
    fn the_screen_shows_the_tape_the_queue_and_the_halt() {
        let mut snapshot = Snapshot::empty("paper", "ops");
        snapshot.halt = Some("lunch".into());
        snapshot.activity.push(ActivityView {
            sequence: 1,
            at: Utc::now(),
            principal: "desk".into(),
            tool: "place_order".into(),
            outcome: "approval_required".into(),
            duration_ms: 3,
            summary: "BUY 1 ABC LIMIT 9".into(),
            rationale: Some("breakout above resistance".into()),
            message: None,
            order_id: None,
        });
        snapshot.approvals.push(ApprovalView {
            principal: "desk".into(),
            digest: "d".repeat(64),
            tool: "place_order".into(),
            description: "BUY 1 ABC LIMIT 9".into(),
            rationale: None,
            first_seen: Utc::now(),
            seen: 1,
        });
        let mut app = App::new(Arc::new(snapshot));
        let text = screen(&app);
        assert!(text.contains("HALTED: lunch"), "{text}");
        assert!(text.contains("AGENT TAPE"));
        assert!(text.contains("approval_required"));
        assert!(text.contains("breakout above resistance"));
        assert!(text.contains("APPROVALS (1 WAITING)"));
        app.modal = Some(Modal::Help);
        assert!(screen(&app).contains("delegate an approval mode"));
    }
}
