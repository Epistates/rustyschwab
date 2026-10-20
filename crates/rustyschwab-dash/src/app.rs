//! The dashboard's state and what each key does. Keys never act on their
//! own: an action that changes anything goes through a confirmation and
//! comes out as a console [`Command`] for the caller to run.

use std::sync::Arc;

use chrono::{DateTime, Duration, NaiveTime, TimeZone, Utc};
use chrono_tz::America::New_York;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use rust_decimal::Decimal;
use rustyschwab_console::{Command, Snapshot};
use rustyschwab_mcp::delegation::Mode;

/// The panes the selection keys move in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pane {
    Tape,
    Approvals,
    Orders,
    Agents,
}

impl Pane {
    const ALL: [Pane; 4] = [Pane::Tape, Pane::Approvals, Pane::Orders, Pane::Agents];
    fn next(self, back: bool) -> Self {
        let at = Self::ALL.iter().position(|p| *p == self).unwrap_or(0);
        let len = Self::ALL.len();
        Self::ALL[if back {
            (at + len - 1) % len
        } else {
            (at + 1) % len
        }]
    }
}

/// When a delegation ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Until {
    /// Good till cancelled.
    Cancelled,
    OneHour,
    FourHours,
    /// Today's 16:00 in New York (tomorrow's, after it).
    MarketClose,
}

impl Until {
    const ALL: [Until; 4] = [
        Until::Cancelled,
        Until::OneHour,
        Until::FourHours,
        Until::MarketClose,
    ];
    pub fn label(self) -> &'static str {
        match self {
            Until::Cancelled => "good till cancelled",
            Until::OneHour => "1 hour",
            Until::FourHours => "4 hours",
            Until::MarketClose => "today's close (16:00 ET)",
        }
    }
    pub fn at(self, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
        match self {
            Until::Cancelled => None,
            Until::OneHour => Some(now + Duration::hours(1)),
            Until::FourHours => Some(now + Duration::hours(4)),
            Until::MarketClose => {
                let local = now.with_timezone(&New_York);
                let close = NaiveTime::from_hms_opt(16, 0, 0)?;
                let mut day = local.date_naive();
                if local.time() >= close {
                    day = day.succ_opt()?;
                }
                New_York
                    .from_local_datetime(&day.and_time(close))
                    .single()
                    .map(|at| at.with_timezone(&Utc))
            }
        }
    }
}

/// Which mode the delegation form is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModeChoice {
    Supervised,
    Threshold,
    Autonomous,
}

impl ModeChoice {
    const ALL: [ModeChoice; 3] = [
        ModeChoice::Supervised,
        ModeChoice::Threshold,
        ModeChoice::Autonomous,
    ];
}

/// The delegation form for one agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DelegateForm {
    pub principal: String,
    pub mode: ModeChoice,
    /// The threshold in dollars, as typed.
    pub amount: String,
    pub until: Until,
    /// Which row the arrow keys change: 0 mode, 1 amount, 2 until.
    pub row: usize,
}

impl DelegateForm {
    fn mode(&self) -> Result<Mode, String> {
        Ok(match self.mode {
            ModeChoice::Supervised => Mode::Supervised,
            ModeChoice::Autonomous => Mode::Autonomous,
            ModeChoice::Threshold => {
                let above: Decimal = self
                    .amount
                    .parse()
                    .map_err(|_| "Type a dollar amount for the threshold".to_owned())?;
                if above <= Decimal::ZERO {
                    return Err("The threshold must be positive".into());
                }
                Mode::Threshold { above }
            }
        })
    }
}

/// What the free-text prompt is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Purpose {
    HaltReason,
    KillReason,
}

/// A dialog over the dashboard.
#[derive(Debug, Clone, PartialEq)]
pub enum Modal {
    /// Run `command` on `y`.
    Confirm {
        title: String,
        body: String,
        command: Command,
    },
    /// Read a line of text.
    Prompt {
        purpose: Purpose,
        text: String,
    },
    /// Kill asks twice: the reason, then typing `kill`.
    KillConfirm {
        reason: String,
        typed: String,
    },
    Delegate(DelegateForm),
    Help,
}

/// The last command's result, for the status line.
#[derive(Debug, Clone)]
pub struct Status {
    pub at: DateTime<Utc>,
    pub ok: bool,
    pub text: String,
}

/// Everything the dashboard shows and remembers between frames.
#[derive(Debug)]
pub struct App {
    pub snapshot: Arc<Snapshot>,
    pub focus: Pane,
    /// Selected row in each pane; the tape lists newest first.
    pub tape: usize,
    pub approval: usize,
    pub order: usize,
    pub agent: usize,
    pub modal: Option<Modal>,
    pub status: Option<Status>,
    /// A command is running; further ones wait.
    pub busy: bool,
    pub quit: bool,
}

impl App {
    pub fn new(snapshot: Arc<Snapshot>) -> Self {
        Self {
            snapshot,
            focus: Pane::Tape,
            tape: 0,
            approval: 0,
            order: 0,
            agent: 0,
            modal: None,
            status: None,
            busy: false,
            quit: false,
        }
    }

    /// A new snapshot: keep selections inside the lists.
    pub fn update(&mut self, snapshot: Arc<Snapshot>) {
        self.snapshot = snapshot;
        let clamp = |at: &mut usize, len: usize| *at = (*at).min(len.saturating_sub(1));
        clamp(&mut self.tape, self.snapshot.activity.len());
        clamp(&mut self.approval, self.snapshot.approvals.len());
        clamp(&mut self.order, self.snapshot.orders.len());
        clamp(&mut self.agent, self.snapshot.principals.len());
    }

    pub fn finished(&mut self, outcome: Result<String, String>) {
        self.busy = false;
        self.status = Some(match outcome {
            Ok(text) => Status {
                at: Utc::now(),
                ok: true,
                text,
            },
            Err(text) => Status {
                at: Utc::now(),
                ok: false,
                text,
            },
        });
    }

    fn fail(&mut self, text: impl Into<String>) {
        self.status = Some(Status {
            at: Utc::now(),
            ok: false,
            text: text.into(),
        });
    }

    /// The agent the agent-scoped keys act on: the selected one in the
    /// agents pane, the selected approval's or tape entry's elsewhere.
    fn agent_in_focus(&self) -> Option<String> {
        let s = &self.snapshot;
        match self.focus {
            Pane::Approvals => s.approvals.get(self.approval).map(|a| a.principal.clone()),
            Pane::Tape => s
                .activity
                .iter()
                .rev()
                .nth(self.tape)
                .map(|a| a.principal.clone()),
            _ => s.principals.get(self.agent).map(|p| p.name.clone()),
        }
        .or_else(|| s.principals.first().map(|p| p.name.clone()))
    }

    /// Handle a key; a confirmed action comes back as a command to run.
    pub fn on_key(&mut self, key: KeyEvent) -> Option<Command> {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.quit = true;
            return None;
        }
        if let Some(modal) = self.modal.take() {
            return self.on_modal_key(modal, key);
        }
        match key.code {
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Char('?') => self.modal = Some(Modal::Help),
            KeyCode::Tab => self.focus = self.focus.next(false),
            KeyCode::BackTab => self.focus = self.focus.next(true),
            KeyCode::Char('1') => self.focus = Pane::Tape,
            KeyCode::Char('2') => self.focus = Pane::Approvals,
            KeyCode::Char('3') => self.focus = Pane::Orders,
            KeyCode::Char('4') => self.focus = Pane::Agents,
            KeyCode::Down | KeyCode::Char('j') => self.step(1),
            KeyCode::Up | KeyCode::Char('k') => self.step(-1),
            KeyCode::Home | KeyCode::Char('g') => self.select_first(),
            KeyCode::Char('a') => self.ask_approval(true),
            KeyCode::Char('x') => self.ask_approval(false),
            KeyCode::Char('d') => match self.agent_in_focus() {
                Some(principal) => {
                    let current = self
                        .snapshot
                        .principals
                        .iter()
                        .find(|p| p.name == principal)
                        .map(|p| p.approval.clone());
                    let (mode, amount) = match current {
                        Some(Mode::Threshold { above }) => {
                            (ModeChoice::Threshold, above.to_string())
                        }
                        Some(Mode::Autonomous) => (ModeChoice::Autonomous, String::new()),
                        _ => (ModeChoice::Supervised, String::new()),
                    };
                    self.modal = Some(Modal::Delegate(DelegateForm {
                        principal,
                        mode,
                        amount,
                        until: Until::Cancelled,
                        row: 0,
                    }));
                }
                None => self.fail("No agent to delegate to"),
            },
            KeyCode::Char('u') => match self.agent_in_focus() {
                Some(principal) => {
                    self.modal = Some(Modal::Confirm {
                        title: "End delegation".into(),
                        body: format!("Return {principal} to its policy's own approval mode?"),
                        command: Command::EndDelegation { principal },
                    })
                }
                None => self.fail("No agent selected"),
            },
            KeyCode::Char('h') => {
                self.modal = Some(Modal::Prompt {
                    purpose: Purpose::HaltReason,
                    text: String::new(),
                })
            }
            KeyCode::Char('r') => {
                self.modal = Some(Modal::Confirm {
                    title: "Resume trading".into(),
                    body: "Clear the operator halt so agents may submit orders again?".into(),
                    command: Command::Resume,
                })
            }
            KeyCode::Char('K') => {
                self.modal = Some(Modal::Prompt {
                    purpose: Purpose::KillReason,
                    text: String::new(),
                })
            }
            _ => {}
        }
        None
    }

    fn ask_approval(&mut self, approve: bool) {
        let Some(waiting) = self.snapshot.approvals.get(self.approval).cloned() else {
            self.fail("No order is waiting for approval");
            return;
        };
        self.focus = Pane::Approvals;
        let mut body = format!("{}\n{}", waiting.principal, waiting.description);
        if let Some(rationale) = &waiting.rationale {
            body.push_str(&format!("\nThe model says: {rationale}"));
        }
        self.modal = Some(Modal::Confirm {
            title: if approve {
                format!(
                    "Approve for {} minutes",
                    rustyschwab_console::APPROVAL_MINUTES
                )
            } else {
                "Refuse this order".into()
            },
            body,
            command: if approve {
                Command::Approve {
                    principal: waiting.principal,
                    digest: waiting.digest,
                }
            } else {
                Command::Deny {
                    principal: waiting.principal,
                    digest: waiting.digest,
                }
            },
        });
    }

    fn on_modal_key(&mut self, modal: Modal, key: KeyEvent) -> Option<Command> {
        match modal {
            Modal::Help => None,
            Modal::Confirm {
                title,
                body,
                command,
            } => match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => Some(command),
                KeyCode::Char('n') | KeyCode::Esc => None,
                _ => {
                    self.modal = Some(Modal::Confirm {
                        title,
                        body,
                        command,
                    });
                    None
                }
            },
            Modal::Prompt { purpose, mut text } => match key.code {
                KeyCode::Esc => None,
                KeyCode::Enter if text.trim().is_empty() => {
                    self.modal = Some(Modal::Prompt { purpose, text });
                    None
                }
                KeyCode::Enter => match purpose {
                    Purpose::HaltReason => Some(Command::Halt {
                        reason: text.trim().to_owned(),
                    }),
                    Purpose::KillReason => {
                        self.modal = Some(Modal::KillConfirm {
                            reason: text.trim().to_owned(),
                            typed: String::new(),
                        });
                        None
                    }
                },
                KeyCode::Backspace => {
                    text.pop();
                    self.modal = Some(Modal::Prompt { purpose, text });
                    None
                }
                KeyCode::Char(c) if text.len() < 200 => {
                    text.push(c);
                    self.modal = Some(Modal::Prompt { purpose, text });
                    None
                }
                _ => {
                    self.modal = Some(Modal::Prompt { purpose, text });
                    None
                }
            },
            Modal::KillConfirm { reason, mut typed } => match key.code {
                KeyCode::Esc => None,
                KeyCode::Enter if typed == "kill" => Some(Command::Kill { reason }),
                KeyCode::Backspace => {
                    typed.pop();
                    self.modal = Some(Modal::KillConfirm { reason, typed });
                    None
                }
                KeyCode::Char(c) if typed.len() < 8 => {
                    typed.push(c);
                    self.modal = Some(Modal::KillConfirm { reason, typed });
                    None
                }
                _ => {
                    self.modal = Some(Modal::KillConfirm { reason, typed });
                    None
                }
            },
            Modal::Delegate(mut form) => {
                match key.code {
                    KeyCode::Esc => return None,
                    KeyCode::Enter => match form.mode() {
                        Ok(mode) => {
                            return Some(Command::Delegate {
                                principal: form.principal,
                                mode,
                                until: form.until.at(Utc::now()),
                            });
                        }
                        Err(e) => {
                            self.fail(e);
                        }
                    },
                    KeyCode::Up => form.row = form.row.saturating_sub(1),
                    KeyCode::Down | KeyCode::Tab => form.row = (form.row + 1).min(2),
                    KeyCode::Left | KeyCode::Right => {
                        let back = key.code == KeyCode::Left;
                        match form.row {
                            0 => form.mode = cycle(&ModeChoice::ALL, form.mode, back),
                            2 => form.until = cycle(&Until::ALL, form.until, back),
                            _ => {}
                        }
                    }
                    KeyCode::Char('s') if form.row == 0 => form.mode = ModeChoice::Supervised,
                    KeyCode::Char('t') if form.row == 0 => form.mode = ModeChoice::Threshold,
                    KeyCode::Char('a') if form.row == 0 => form.mode = ModeChoice::Autonomous,
                    KeyCode::Char(c) if form.row == 1 && (c.is_ascii_digit() || c == '.') => {
                        if form.amount.len() < 12 {
                            form.amount.push(c);
                        }
                    }
                    KeyCode::Backspace if form.row == 1 => {
                        form.amount.pop();
                    }
                    _ => {}
                }
                self.modal = Some(Modal::Delegate(form));
                None
            }
        }
    }

    fn step(&mut self, by: isize) {
        let len = match self.focus {
            Pane::Tape => self.snapshot.activity.len(),
            Pane::Approvals => self.snapshot.approvals.len(),
            Pane::Orders => self.snapshot.orders.len(),
            Pane::Agents => self.snapshot.principals.len(),
        };
        let at = match self.focus {
            Pane::Tape => &mut self.tape,
            Pane::Approvals => &mut self.approval,
            Pane::Orders => &mut self.order,
            Pane::Agents => &mut self.agent,
        };
        *at = at.saturating_add_signed(by).min(len.saturating_sub(1));
    }

    fn select_first(&mut self) {
        match self.focus {
            Pane::Tape => self.tape = 0,
            Pane::Approvals => self.approval = 0,
            Pane::Orders => self.order = 0,
            Pane::Agents => self.agent = 0,
        }
    }
}

fn cycle<T: Copy + PartialEq>(all: &[T], current: T, back: bool) -> T {
    let at = all.iter().position(|v| *v == current).unwrap_or(0);
    let len = all.len();
    all[if back {
        (at + len - 1) % len
    } else {
        (at + 1) % len
    }]
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustyschwab_console::snapshot::ApprovalView;

    fn press(app: &mut App, code: KeyCode) -> Option<Command> {
        app.on_key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn type_text(app: &mut App, text: &str) {
        for c in text.chars() {
            assert!(press(app, KeyCode::Char(c)).is_none());
        }
    }

    fn waiting() -> Arc<Snapshot> {
        let mut snapshot = Snapshot::empty("paper", "ops");
        snapshot.approvals.push(ApprovalView {
            principal: "desk".into(),
            digest: "d".repeat(64),
            tool: "place_order".into(),
            description: "BUY 1 ABC".into(),
            rationale: Some("why".into()),
            first_seen: Utc::now(),
            seen: 1,
        });
        Arc::new(snapshot)
    }

    #[test]
    fn approving_asks_first_and_then_names_the_order() {
        let mut app = App::new(waiting());
        assert!(press(&mut app, KeyCode::Char('a')).is_none());
        assert!(matches!(app.modal, Some(Modal::Confirm { .. })));
        let command = press(&mut app, KeyCode::Char('y')).unwrap();
        assert_eq!(
            command,
            Command::Approve {
                principal: "desk".into(),
                digest: "d".repeat(64)
            }
        );
        // Any other key leaves the question open; Esc drops it.
        press(&mut app, KeyCode::Char('x'));
        assert!(press(&mut app, KeyCode::Char('q')).is_none());
        assert!(app.modal.is_some() && !app.quit);
        press(&mut app, KeyCode::Esc);
        assert!(app.modal.is_none());
    }

    #[test]
    fn kill_needs_a_reason_and_the_word() {
        let mut app = App::new(waiting());
        press(&mut app, KeyCode::Char('K'));
        assert!(press(&mut app, KeyCode::Enter).is_none(), "no reason yet");
        type_text(&mut app, "runaway");
        assert!(press(&mut app, KeyCode::Enter).is_none());
        type_text(&mut app, "kil");
        assert!(press(&mut app, KeyCode::Enter).is_none());
        type_text(&mut app, "l");
        assert_eq!(
            press(&mut app, KeyCode::Enter),
            Some(Command::Kill {
                reason: "runaway".into()
            })
        );
    }

    #[test]
    fn the_delegation_form_builds_a_threshold_grant() {
        let mut snapshot = (*waiting()).clone();
        snapshot
            .principals
            .push(rustyschwab_console::snapshot::PrincipalView {
                name: "desk".into(),
                approval: Mode::Supervised,
                delegation: None,
                ceiling: Mode::Autonomous,
                dead_man: None,
                budget: Default::default(),
                pending_submissions: 0,
                last_call: None,
                active: true,
            });
        let mut app = App::new(Arc::new(snapshot));
        app.focus = Pane::Agents;
        press(&mut app, KeyCode::Char('d'));
        press(&mut app, KeyCode::Char('t'));
        press(&mut app, KeyCode::Down);
        type_text(&mut app, "2500");
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Right);
        let Some(Command::Delegate {
            principal,
            mode,
            until,
        }) = press(&mut app, KeyCode::Enter)
        else {
            panic!("no grant");
        };
        assert_eq!(principal, "desk");
        assert_eq!(
            mode,
            Mode::Threshold {
                above: Decimal::from(2500)
            }
        );
        assert!(until.is_some_and(|u| u > Utc::now()));
    }

    #[test]
    fn market_close_is_the_next_four_pm_in_new_york() {
        let morning = New_York
            .with_ymd_and_hms(2026, 10, 20, 10, 0, 0)
            .unwrap()
            .with_timezone(&Utc);
        let close = Until::MarketClose
            .at(morning)
            .unwrap()
            .with_timezone(&New_York);
        assert_eq!(
            close.format("%Y-%m-%d %H:%M").to_string(),
            "2026-10-20 16:00"
        );
        let evening = morning + Duration::hours(8);
        let close = Until::MarketClose
            .at(evening)
            .unwrap()
            .with_timezone(&New_York);
        assert_eq!(
            close.format("%Y-%m-%d %H:%M").to_string(),
            "2026-10-21 16:00"
        );
    }
}
