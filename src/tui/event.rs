//! Terminal wiring: the only module in `tui` that touches a real terminal or
//! performs network/process I/O.
//!
//! [`run`] owns the event loop; [`execute`] is the thin translation from a
//! [`Cmd`] to the [`Msg`] it produces, kept separate so it can be unit tested
//! with [`crate::jira::fake::FakeJiraClient`] instead of a live Jira and a
//! real terminal.

use std::collections::VecDeque;
use std::time::Duration;

use crossterm::event::{self, Event as CEvent, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use thiserror::Error;

use crate::jira::adf::adf_to_text;
use crate::jira::client::JiraClient;
use crate::jira::jql::my_open_tickets_jql;
use crate::jira::types::Issue;
use crate::tui::app::{App, Cmd, Msg, update};
use crate::tui::keymap::map_key;
use crate::tui::ui::draw;

/// How long to wait for a key press between redraws.
const POLL_INTERVAL: Duration = Duration::from_millis(250);

/// Errors that can occur while running the TUI.
#[derive(Debug, Error)]
pub enum TuiError {
    /// Setting up or tearing down the terminal, or drawing to it, failed.
    #[error("terminal I/O error: {0}")]
    Io(#[from] std::io::Error),
}

/// Dependencies the TUI needs to talk to Jira and build browsable URLs.
pub struct TuiDeps {
    /// Client used to fetch tickets, transitions, and apply transitions.
    pub jira: Box<dyn JiraClient>,
    /// Base URL of the Jira instance, used to build `{base_url}/browse/{key}`
    /// links for [`Cmd::OpenUrl`].
    pub base_url: String,
}

/// Restores the terminal (raw mode and the alternate screen) when dropped.
///
/// Constructed immediately after `enable_raw_mode` succeeds, so the terminal
/// is restored whether `run` returns normally, returns an error, or the
/// current thread panics while the guard is in scope (`Drop` still runs
/// during unwinding).
struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(std::io::stdout(), LeaveAlternateScreen);
    }
}

/// Run the interactive board until the user quits.
///
/// Enters raw mode and the alternate screen, fetches the initial ticket list,
/// then loops: draw the current screen, wait up to `POLL_INTERVAL` for a key
/// press, map it to a [`Msg`], run it through [`crate::tui::app::update`], and
/// execute any resulting [`Cmd`]s. The terminal is always restored before
/// returning, including on error.
pub fn run(deps: TuiDeps) -> Result<(), TuiError> {
    enable_raw_mode()?;
    let _guard = TerminalGuard;
    execute!(std::io::stdout(), EnterAlternateScreen)?;

    let backend = CrosstermBackend::new(std::io::stdout());
    let mut terminal = Terminal::new(backend)?;

    let mut app = App::new();
    app = run_cmds(app, vec![Cmd::FetchTickets], &deps);

    while !app.quit {
        terminal.draw(|frame| draw(frame, &app))?;

        if event::poll(POLL_INTERVAL)?
            && let CEvent::Key(key_event) = event::read()?
            && key_event.kind == KeyEventKind::Press
            && let Some(msg) = map_key(&app.screen, app.show_help, key_event.code)
        {
            let (next_app, cmds) = update(app, msg);
            app = run_cmds(next_app, cmds, &deps);
        }
    }

    Ok(())
}

/// Execute every `Cmd` in `cmds`, feeding each resulting `Msg` back through
/// `update` (which may itself produce further `Cmd`s, e.g. loading
/// transitions after opening the detail screen).
fn run_cmds(mut app: App, cmds: Vec<Cmd>, deps: &TuiDeps) -> App {
    let mut pending: VecDeque<Cmd> = cmds.into();
    while let Some(cmd) = pending.pop_front() {
        for msg in execute(deps, cmd) {
            let (next_app, more_cmds) = update(app, msg);
            app = next_app;
            pending.extend(more_cmds);
        }
    }
    app
}

/// Translate a single [`Cmd`] into the [`Msg`]s it produces.
///
/// Performs the actual I/O (a Jira API call, or spawning the `open`
/// process); everything else in `tui` stays pure and terminal-free.
fn execute(deps: &TuiDeps, cmd: Cmd) -> Vec<Msg> {
    match cmd {
        Cmd::FetchTickets => fetch_tickets(deps),
        Cmd::FetchTransitions { key } => fetch_transitions(deps, &key),
        Cmd::ApplyTransition { key, transition_id } => apply_transition(deps, &key, &transition_id),
        Cmd::OpenUrl(url) => open_url(&url),
    }
}

/// Run `Cmd::FetchTickets`: search for the user's open tickets and map them
/// to [`crate::tui::app::TicketSummary`]s.
fn fetch_tickets(deps: &TuiDeps) -> Vec<Msg> {
    match deps.jira.search(&my_open_tickets_jql()) {
        Ok(result) => {
            let tickets = result
                .issues
                .into_iter()
                .map(|issue| to_ticket_summary(issue, &deps.base_url))
                .collect();
            vec![Msg::TicketsLoaded(tickets)]
        }
        Err(err) => vec![Msg::TicketsFailed(err.to_string())],
    }
}

/// Run `Cmd::FetchTransitions`: list the workflow transitions available on
/// `key`.
fn fetch_transitions(deps: &TuiDeps, key: &str) -> Vec<Msg> {
    match deps.jira.transitions(key) {
        Ok(transitions) => vec![Msg::TransitionsLoaded(transitions)],
        Err(err) => vec![Msg::TransitionsFailed(err.to_string())],
    }
}

/// Run `Cmd::ApplyTransition`: apply the transition, then re-fetch the issue
/// to learn its resulting status (the transition endpoint itself returns no
/// body).
fn apply_transition(deps: &TuiDeps, key: &str, transition_id: &str) -> Vec<Msg> {
    if let Err(err) = deps.jira.transition(key, transition_id) {
        return vec![Msg::TransitionFailed(err.to_string())];
    }
    match deps.jira.get_issue(key) {
        Ok(issue) => vec![Msg::TransitionApplied {
            key: key.to_string(),
            status_category: issue.fields.status.status_category.key.clone(),
            status: issue.fields.status.name,
        }],
        Err(err) => vec![Msg::TransitionFailed(err.to_string())],
    }
}

/// Best-effort open `url` in the user's default browser via the `open`
/// command. Failures are not surfaced as a dedicated message (the fixed `Msg`
/// set has no `OpenUrlFailed` variant); [`Msg::TicketsFailed`] is reused
/// purely for its `status_line`-setting effect, which is exactly what an
/// open-browser failure needs.
fn open_url(url: &str) -> Vec<Msg> {
    match std::process::Command::new("open").arg(url).status() {
        Ok(status) if status.success() => Vec::new(),
        Ok(status) => vec![Msg::TicketsFailed(format!(
            "failed to open browser (exit {status})"
        ))],
        Err(err) => vec![Msg::TicketsFailed(format!("failed to open browser: {err}"))],
    }
}

/// Convert a Jira [`Issue`] into a [`crate::tui::app::TicketSummary`],
/// deriving `url` from `base_url` and `description` from the issue's ADF
/// description via [`adf_to_text`].
fn to_ticket_summary(issue: Issue, base_url: &str) -> crate::tui::app::TicketSummary {
    let description = issue
        .fields
        .description
        .as_ref()
        .map(adf_to_text)
        .unwrap_or_default();
    crate::tui::app::TicketSummary {
        url: format!("{base_url}/browse/{}", issue.key),
        key: issue.key,
        summary: issue.fields.summary,
        status_category: issue.fields.status.status_category.key.clone(),
        status: issue.fields.status.name,
        description,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jira::fake::FakeJiraClient;
    use crate::jira::types::{IssueFields, Status, StatusCategory};

    fn issue(key: &str, status: &str) -> Issue {
        Issue {
            key: key.to_string(),
            fields: IssueFields {
                summary: "Fix the thing".to_string(),
                status: Status {
                    name: status.to_string(),
                    status_category: StatusCategory {
                        key: "new".to_string(),
                    },
                },
                description: Some(serde_json::json!({
                    "type": "doc",
                    "version": 1,
                    "content": [
                        { "type": "paragraph", "content": [{ "type": "text", "text": "Body text" }] }
                    ]
                })),
                assignee: None,
            },
        }
    }

    fn deps(jira: FakeJiraClient) -> TuiDeps {
        TuiDeps {
            jira: Box::new(jira),
            base_url: "https://example.atlassian.net".to_string(),
        }
    }

    #[test]
    fn fetch_tickets_maps_issues_to_ticket_summaries() {
        use crate::jira::types::SearchResult;

        let jira = FakeJiraClient::new().with_search_result(SearchResult {
            issues: vec![issue("AX-1", "To Do")],
            next_page_token: None,
        });
        let msgs = fetch_tickets(&deps(jira));
        match msgs.as_slice() {
            [Msg::TicketsLoaded(tickets)] => {
                assert_eq!(tickets.len(), 1);
                assert_eq!(tickets[0].key, "AX-1");
                assert_eq!(tickets[0].status, "To Do");
                assert_eq!(tickets[0].url, "https://example.atlassian.net/browse/AX-1");
                assert_eq!(tickets[0].description, "Body text");
            }
            other => panic!("expected TicketsLoaded, got {other:?}"),
        }
    }

    #[test]
    fn fetch_tickets_with_empty_search_result_loads_empty_list() {
        let jira = FakeJiraClient::new();
        let msgs = fetch_tickets(&deps(jira));
        assert_eq!(msgs, vec![Msg::TicketsLoaded(vec![])]);
    }

    #[test]
    fn fetch_tickets_failure_emits_tickets_failed() {
        let jira = FakeJiraClient::new().with_search_error(500, "boom");
        let msgs = fetch_tickets(&deps(jira));
        match msgs.as_slice() {
            [Msg::TicketsFailed(message)] => assert_eq!(message, "Jira API error (500): boom"),
            other => panic!("expected TicketsFailed, got {other:?}"),
        }
    }

    #[test]
    fn fetch_transitions_success_emits_transitions_loaded() {
        let jira = FakeJiraClient::new();
        let msgs = fetch_transitions(&deps(jira), "AX-1");
        assert_eq!(msgs, vec![Msg::TransitionsLoaded(vec![])]);
    }

    #[test]
    fn apply_transition_success_refetches_issue_for_new_status() {
        let jira = FakeJiraClient::new().with_issue("AX-1", issue("AX-1", "In Progress"));
        let msgs = apply_transition(&deps(jira), "AX-1", "11");
        assert_eq!(
            msgs,
            vec![Msg::TransitionApplied {
                key: "AX-1".to_string(),
                status: "In Progress".to_string(),
                status_category: "new".to_string()
            }]
        );
    }

    #[test]
    fn apply_transition_failure_to_refetch_emits_transition_failed() {
        let jira = FakeJiraClient::new().with_issue_not_found("AX-1");
        let msgs = apply_transition(&deps(jira), "AX-1", "11");
        match msgs.as_slice() {
            [Msg::TransitionFailed(_)] => {}
            other => panic!("expected TransitionFailed, got {other:?}"),
        }
    }

    #[test]
    fn to_ticket_summary_derives_url_and_extracts_description() {
        let summary = to_ticket_summary(issue("AX-1", "To Do"), "https://example.atlassian.net");
        assert_eq!(summary.key, "AX-1");
        assert_eq!(summary.status, "To Do");
        assert_eq!(summary.url, "https://example.atlassian.net/browse/AX-1");
        assert_eq!(summary.description, "Body text");
    }

    #[test]
    fn to_ticket_summary_with_no_description_is_empty_string() {
        let mut issue = issue("AX-1", "To Do");
        issue.fields.description = None;
        let summary = to_ticket_summary(issue, "https://example.atlassian.net");
        assert_eq!(summary.description, "");
    }

    #[test]
    fn run_cmds_feeds_tickets_loaded_back_through_update() {
        use crate::jira::types::SearchResult;

        let jira = FakeJiraClient::new().with_search_result(SearchResult {
            issues: vec![issue("AX-1", "To Do")],
            next_page_token: None,
        });
        let app = run_cmds(App::new(), vec![Cmd::FetchTickets], &deps(jira));
        assert_eq!(app.columns.len(), 1);
        assert_eq!(app.selected_col, 0);
        assert_eq!(app.selected_row, 0);
    }
}
