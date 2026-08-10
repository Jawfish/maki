//! The one place that turns an error into what the user reads: what failed,
//! the cause that matters, and the next action. Every error surface (agent
//! runs, streams, failed tools, expired logins) builds a report here, so no
//! surface invents its own wording, leaks provider jargon or echoes a secret.

use serde::Serialize;
use serde::ser::SerializeStruct;

use crate::error::AgentError;

const REDACTED: &str = "[redacted]";
/// Prefixes providers use for issued credentials.
const SECRET_PREFIXES: [&str; 10] = [
    "sk-",
    "ghp_",
    "gho_",
    "ghu_",
    "github_pat_",
    "xoxb-",
    "xoxp-",
    "hf_",
    "AIza",
    "ya29.",
];
/// Words that introduce a credential, so the token after them goes too.
const SECRET_LEADS: [&str; 8] = [
    "authorization",
    "bearer",
    "token",
    "key",
    "apikey",
    "api_key",
    "api-key",
    "password",
];
/// Length past which an opaque alphanumeric run is treated as a credential.
const OPAQUE_SECRET_LEN: usize = 24;
/// How much of a raw error body the detail view keeps.
const DETAIL_LIMIT: usize = 400;
const DETAIL_ELLIPSIS: &str = "...";

const NEXT_RETRY: &str = "Press enter to retry.";
const NEXT_WAIT_RETRY: &str = "Wait a moment, then press enter to retry.";
const NEXT_LOGIN: &str = "Run `maki auth login` in another terminal, then press enter to retry.";
const NEXT_COMPACT: &str = "Run /compact or start a new session, then send again.";
const NEXT_MODEL: &str = "Pick another model with /model, then send again.";
const NEXT_CONFIG: &str = "Fix the setting the detail names, then start maki again.";
const NEXT_TOOL: &str = "Read the detail, fix the cause, then ask maki to try again.";
const NEXT_CONTINUE: &str = "Send a message to carry on.";

const WHAT_SEND: &str = "Sending your message failed";
const WHAT_REPLY: &str = "The reply stopped early";
const WHAT_LOGIN: &str = "Your login is not usable";
const WHAT_CONFIG: &str = "Reading your settings failed";
const WHAT_SESSION: &str = "The session outgrew the model";
const WHAT_CANCELLED: &str = "The run was cancelled";

const CAUSE_RATE_LIMIT: &str = "this account is over its rate limit";
const CAUSE_OVERLOADED: &str = "the service is overloaded";
const CAUSE_SERVICE_DOWN: &str = "the service could not answer";
const CAUSE_CREDENTIALS: &str = "the saved credentials were turned away";
const CAUSE_REFUSED: &str = "the service turned the request away";
const CAUSE_CONTEXT: &str = "the whole session no longer fits in the model context";
const CAUSE_CONNECTION: &str = "the connection dropped";
const CAUSE_UNREADABLE: &str = "the reply could not be read";
const CAUSE_INTERNAL: &str = "maki lost an internal channel";
const CAUSE_MALFORMED_REQUEST: &str = "the request could not be built";
const CAUSE_CANCELLED: &str = "you cancelled it";

const SERVER_ERROR_FLOOR: u16 = 500;
const OVERLOADED_STATUS: u16 = 529;
const RATE_LIMIT_STATUS: u16 = 429;
const UNAUTHORIZED_STATUS: u16 = 401;

/// Whether the same request is worth sending again untouched. Timeouts, rate
/// limits, server errors and dropped connections are; anything the user has to
/// fix first (login, bad request, settings) is not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Transience {
    Transient,
    Permanent,
}

impl Transience {
    pub const fn is_transient(self) -> bool {
        matches!(self, Self::Transient)
    }
}

pub fn transience(error: &AgentError) -> Transience {
    if error.is_retryable() {
        Transience::Transient
    } else {
        Transience::Permanent
    }
}

/// Replaces anything that looks like a credential with [`REDACTED`]. Applied
/// to every piece of underlying text a report shows.
pub fn redact(text: &str) -> String {
    text.lines().map(redact_line).collect::<Vec<_>>().join("\n")
}

fn redact_line(line: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut after_lead = false;
    for token in line.split_whitespace() {
        let secret = is_secret(token) || (after_lead && !token.is_empty());
        after_lead = is_secret_lead(token);
        out.push(if secret {
            REDACTED.to_owned()
        } else {
            token.to_owned()
        });
    }
    out.join(" ")
}

fn core(token: &str) -> &str {
    token.trim_matches(|c: char| !c.is_alphanumeric() && c != '-' && c != '_' && c != '.')
}

fn is_secret_lead(token: &str) -> bool {
    let lead = core(token).to_lowercase();
    SECRET_LEADS.contains(&lead.as_str())
}

fn is_secret(token: &str) -> bool {
    let value = core(token);
    if SECRET_PREFIXES.iter().any(|p| value.starts_with(p)) {
        return true;
    }
    value.len() >= OPAQUE_SECRET_LEN
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        && value.chars().any(|c| c.is_ascii_digit())
        && value.chars().any(|c| c.is_ascii_alphabetic())
}

/// What failed, the cause worth naming, and the next action. `detail` holds
/// the redacted underlying text, shown only when the user asks for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErrorReport {
    what: String,
    cause: Option<String>,
    next: &'static str,
    detail: Option<String>,
    transience: Transience,
}

impl ErrorReport {
    pub fn from_agent_error(error: &AgentError) -> Self {
        if let AgentError::Tool { tool, message } = error {
            return Self::for_tool(tool, message);
        }
        let (what, cause, next) = parts(error);
        let cause = match error {
            AgentError::Config { message } => first_line(&redact(message)),
            _ => Some(cause.to_owned()),
        };
        Self {
            what: what.to_owned(),
            cause,
            next,
            detail: detail_of(error),
            transience: transience(error),
        }
    }

    /// A tool that came back with an error. Its output is the user's cause,
    /// so it is redacted and trimmed rather than dumped.
    pub fn for_tool(tool: &str, output: &str) -> Self {
        let clean = redact(output);
        let cause = first_line(&clean);
        Self {
            what: format!("The {tool} tool failed"),
            cause,
            next: NEXT_TOOL,
            detail: (!clean.trim().is_empty()).then(|| truncate(&clean)),
            transience: Transience::Permanent,
        }
    }

    pub fn auth_expired() -> Self {
        Self {
            what: WHAT_LOGIN.to_owned(),
            cause: Some(CAUSE_CREDENTIALS.to_owned()),
            next: NEXT_LOGIN,
            detail: None,
            transience: Transience::Permanent,
        }
    }

    pub fn what(&self) -> &str {
        &self.what
    }

    pub fn cause(&self) -> Option<&str> {
        self.cause.as_deref()
    }

    pub fn next_action(&self) -> &str {
        self.next
    }

    pub fn detail(&self) -> Option<&str> {
        self.detail.as_deref()
    }

    pub fn transience(&self) -> Transience {
        self.transience
    }

    pub fn is_transient(&self) -> bool {
        self.transience.is_transient()
    }

    /// What failed and why, for surfaces with one line to spend.
    pub fn primary_line(&self) -> String {
        match &self.cause {
            Some(cause) => format!("{}: {cause}", self.what),
            None => self.what.clone(),
        }
    }

    /// All three parts on one line, for logs and non-interactive surfaces.
    pub fn one_line(&self) -> String {
        format!("{}. {}", self.primary_line(), self.next)
    }

    /// The block a transcript shows: cause first, action second.
    pub fn lines(&self) -> Vec<String> {
        vec![self.primary_line(), self.next.to_owned()]
    }
}

impl Serialize for ErrorReport {
    /// Keeps the wire shape non-interactive clients already read, so the
    /// three parts arrive as one message plus the retry hint.
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("Error", 2)?;
        state.serialize_field("message", &self.one_line())?;
        state.serialize_field("transient", &self.is_transient())?;
        state.end()
    }
}

fn parts(error: &AgentError) -> (&'static str, &'static str, &'static str) {
    if error.is_context_overflow() {
        return (WHAT_SESSION, CAUSE_CONTEXT, NEXT_COMPACT);
    }
    match error {
        AgentError::Api {
            status: RATE_LIMIT_STATUS,
            ..
        } => (WHAT_SEND, CAUSE_RATE_LIMIT, NEXT_WAIT_RETRY),
        AgentError::Api {
            status: OVERLOADED_STATUS,
            ..
        } => (WHAT_SEND, CAUSE_OVERLOADED, NEXT_WAIT_RETRY),
        AgentError::Api { status, .. } if *status >= SERVER_ERROR_FLOOR => {
            (WHAT_SEND, CAUSE_SERVICE_DOWN, NEXT_RETRY)
        }
        AgentError::Api {
            status: UNAUTHORIZED_STATUS,
            ..
        } => (WHAT_LOGIN, CAUSE_CREDENTIALS, NEXT_LOGIN),
        AgentError::Api { .. } => (WHAT_SEND, CAUSE_REFUSED, NEXT_MODEL),
        AgentError::Config { .. } => (WHAT_CONFIG, CAUSE_REFUSED, NEXT_CONFIG),
        AgentError::Tool { .. } => (WHAT_SEND, CAUSE_REFUSED, NEXT_TOOL),
        AgentError::Io(_) | AgentError::Http(_) => (WHAT_SEND, CAUSE_CONNECTION, NEXT_RETRY),
        AgentError::Timeout { .. } => (WHAT_REPLY, CAUSE_CONNECTION, NEXT_RETRY),
        AgentError::Json(_) => (WHAT_REPLY, CAUSE_UNREADABLE, NEXT_RETRY),
        AgentError::Channel => (WHAT_SEND, CAUSE_INTERNAL, NEXT_RETRY),
        AgentError::HttpRequest(_) => (WHAT_SEND, CAUSE_MALFORMED_REQUEST, NEXT_MODEL),
        AgentError::Cancelled => (WHAT_CANCELLED, CAUSE_CANCELLED, NEXT_CONTINUE),
    }
}

/// The underlying text, redacted. Everything technical (status codes, bodies,
/// transport messages) lives here instead of in the primary line.
fn detail_of(error: &AgentError) -> Option<String> {
    let raw = match error {
        AgentError::Cancelled | AgentError::Channel => return None,
        other => other.to_string(),
    };
    let clean = redact(&raw);
    (!clean.trim().is_empty()).then(|| truncate(&clean))
}

fn first_line(text: &str) -> Option<String> {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(str::to_owned)
}

fn truncate(text: &str) -> String {
    let trimmed = text.trim();
    match trimmed.char_indices().nth(DETAIL_LIMIT) {
        Some((end, _)) => format!("{}{DETAIL_ELLIPSIS}", &trimmed[..end]),
        None => trimmed.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    const TOOL: &str = "bash";
    const SECRET: &str = "sk-ant-api03-abcdefghijklmnop";
    const JARGON: [&str; 8] = ["API", "HTTP", "http", "JSON", "json", "401", "429", "isahc"];

    fn api(status: u16, message: &str) -> AgentError {
        AgentError::Api {
            status,
            message: message.into(),
        }
    }

    #[test_case(api(RATE_LIMIT_STATUS, ""), Transience::Transient  ; "rate_limit")]
    #[test_case(api(OVERLOADED_STATUS, ""), Transience::Transient  ; "overloaded")]
    #[test_case(api(503, ""), Transience::Transient                ; "server_error")]
    #[test_case(AgentError::Timeout { secs: 30 }, Transience::Transient ; "timeout")]
    #[test_case(
        AgentError::Io(std::io::Error::from(std::io::ErrorKind::ConnectionReset)),
        Transience::Transient ; "connection_reset"
    )]
    #[test_case(api(UNAUTHORIZED_STATUS, ""), Transience::Permanent ; "unauthorized")]
    #[test_case(api(400, ""), Transience::Permanent                 ; "bad_request")]
    #[test_case(api(403, ""), Transience::Permanent                 ; "forbidden")]
    #[test_case(AgentError::Channel, Transience::Permanent          ; "channel")]
    fn transience_table(error: AgentError, expected: Transience) {
        assert_eq!(transience(&error), expected);
    }

    #[test_case(SECRET                                    ; "provider_key")]
    #[test_case("ghp_0123456789abcdefghijklmnop"          ; "github_token")]
    #[test_case("Authorization: abcdef1234567890"         ; "auth_header")]
    #[test_case("Bearer abcdef1234567890"                 ; "bearer_token")]
    #[test_case("\"api_key\": \"0123456789abcdefghij\""   ; "json_field")]
    #[test_case("deadbeefdeadbeefdeadbeef0123456789"      ; "opaque_run")]
    fn redact_hides_credentials(raw: &str) {
        let out = redact(raw);
        assert!(out.contains(REDACTED), "{out}");
        for token in raw.split_whitespace().filter(|t| is_secret(t)) {
            assert!(!out.contains(token), "{out}");
        }
    }

    #[test_case("the connection dropped"                  ; "prose")]
    #[test_case("edit maki-ui/src/components/marker.rs"   ; "path")]
    fn redact_keeps_plain_text(raw: &str) {
        assert_eq!(redact(raw), raw);
    }

    #[test_case(api(RATE_LIMIT_STATUS, ""), WHAT_SEND, CAUSE_RATE_LIMIT, NEXT_WAIT_RETRY ; "rate_limit")]
    #[test_case(api(UNAUTHORIZED_STATUS, ""), WHAT_LOGIN, CAUSE_CREDENTIALS, NEXT_LOGIN  ; "unauthorized")]
    #[test_case(AgentError::Timeout { secs: 30 }, WHAT_REPLY, CAUSE_CONNECTION, NEXT_RETRY ; "timeout")]
    #[test_case(
        api(400, "prompt is too long: 300000 tokens > 200000 maximum"),
        WHAT_SESSION, CAUSE_CONTEXT, NEXT_COMPACT ; "context_overflow"
    )]
    fn three_parts_per_error_kind(error: AgentError, what: &str, cause: &str, next: &str) {
        let report = ErrorReport::from_agent_error(&error);
        assert_eq!(report.what(), what);
        assert_eq!(report.cause(), Some(cause));
        assert_eq!(report.next_action(), next);
        assert_eq!(report.lines(), vec![report.primary_line(), next.to_owned()]);
    }

    #[test]
    fn primary_line_drops_provider_jargon() {
        let raw = format!("authentication_error: invalid x-api-key header {SECRET}");
        let report = ErrorReport::from_agent_error(&api(UNAUTHORIZED_STATUS, &raw));
        let primary = report.primary_line();
        for word in JARGON {
            assert!(!primary.contains(word), "{primary}");
        }
        assert!(!primary.contains(SECRET), "{primary}");
        assert!(!report.detail().unwrap().contains(SECRET));
    }

    #[test]
    fn tool_failure_reports_cause_and_action() {
        let report = ErrorReport::for_tool(TOOL, &format!("curl failed with {SECRET}\nsee log"));
        assert_eq!(report.what(), format!("The {TOOL} tool failed"));
        assert!(!report.cause().unwrap().contains(SECRET));
        assert_eq!(report.next_action(), NEXT_TOOL);
        assert!(!report.is_transient());
    }

    #[test]
    fn auth_expiry_points_at_login() {
        let report = ErrorReport::auth_expired();
        assert_eq!(report.next_action(), NEXT_LOGIN);
        assert!(report.one_line().contains(CAUSE_CREDENTIALS));
    }
}
