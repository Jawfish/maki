use std::borrow::Cow;
use std::cmp::Reverse;
use std::env;
use std::path::Path;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use super::subscription_usage::SubscriptionUsage;
use super::{RetryInfo, Status};

use crate::animation::spinner_frame;
use crate::components::layout::{COST_COL, TOKENS_COL, right_align};
use crate::theme;

use crate::components::marker::State;
use maki_providers::{ModelPricing, ProviderUsage, TokenUsage, UsageLimit, format_tokens};
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

const FAST_LABEL: &str = " [fast]";
const CLAUDE_ICON: &str = "\u{ec82}";
const OPENAI_ICON: &str = "\u{ec81}";
const RESET_ICON: &str = "\u{eb37}";
const WARNING_PERCENTAGE: u32 = 85;
const PERCENT_COL: usize = 3;
const GLOBAL_COST_PREFIX: &str = "\u{03a3}";
const AUTO_SCROLL_PAUSED: &str = " auto-scroll paused";
const RETRY_SEPARATOR: &str = " · ";

/// Eviction order when the bar does not fit: `Ambient` goes first, `Blocking`
/// last, so what stops the run is the last thing a narrow terminal loses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Priority {
    Ambient,
    Routine,
    Primary,
    Blocking,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Side {
    Left,
    Right,
}

/// Every piece of the bar, in render order per side. The priority table below
/// is the only place eviction order is decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Segment {
    Running,
    Restoring,
    Mode,
    Thinking,
    Session,
    AutoScroll,
    Retry,
    Error,
    Flash,
    Quota,
    Cwd,
    Model,
    Context,
    Cost,
    GlobalCost,
}

impl Segment {
    const fn priority(self) -> Priority {
        match self {
            Self::Retry | Self::Error => Priority::Blocking,
            Self::Running | Self::Mode | Self::Flash => Priority::Primary,
            Self::Restoring | Self::AutoScroll | Self::Context | Self::Cost | Self::Quota => {
                Priority::Routine
            }
            Self::Thinking | Self::Session | Self::Cwd | Self::Model | Self::GlobalCost => {
                Priority::Ambient
            }
        }
    }

    const fn side(self) -> Side {
        match self {
            Self::Quota
            | Self::Cwd
            | Self::Model
            | Self::Context
            | Self::Cost
            | Self::GlobalCost => Side::Right,
            _ => Side::Left,
        }
    }
}

struct Rendered {
    segment: Segment,
    spans: Vec<Span<'static>>,
}

impl Rendered {
    fn width(&self) -> usize {
        self.spans.iter().map(Span::width).sum()
    }
}

fn label_style() -> Style {
    theme::current().status_dim
}

fn value_style() -> Style {
    Style::new().fg(theme::current().foreground)
}

/// Drops whole segments, lowest priority first and rightmost first inside a
/// priority, until the remainder fits. `Blocking` segments are never dropped:
/// a terminal too narrow for them clips instead of hiding why work stopped.
fn fit(mut rendered: Vec<Rendered>, width: usize) -> Vec<Rendered> {
    let mut total: usize = rendered.iter().map(Rendered::width).sum();
    while total > width {
        let Some((index, priority)) = rendered
            .iter()
            .enumerate()
            .map(|(index, r)| (index, r.segment.priority()))
            .min_by_key(|&(index, priority)| (priority, Reverse(index)))
        else {
            break;
        };
        if priority == Priority::Blocking {
            break;
        }
        total -= rendered.remove(index).width();
    }
    rendered
}

pub struct UsageStats<'a> {
    pub global_usage: &'a TokenUsage,
    pub context_size: u32,
    pub cost: Option<f64>,
    pub pricing: &'a ModelPricing,
    pub context_window: u32,
    pub show_global: bool,
}

pub struct StatusBarContext<'a> {
    pub status: &'a Status,
    pub mode_label: Cow<'static, str>,
    pub mode_style: Style,
    pub model_id: &'a str,
    pub stats: UsageStats<'a>,
    pub auto_scroll: bool,
    pub chat_name: Option<&'a str>,
    pub retry_info: Option<&'a RetryInfo>,
    /// Active thinking level, e.g. `adaptive` or `high`. `None` hides the badge.
    pub thinking: Option<Cow<'static, str>>,
    pub fast: bool,
    pub restoring: bool,
    pub subscription_usage: &'a SubscriptionUsage,
    /// The task line already shows phase, elapsed and the retry countdown
    /// while it is up, so the bar drops those to avoid saying it twice.
    pub task_line_visible: bool,
}

pub struct StatusBar {
    flash: Option<(String, Instant)>,
    started_at: Instant,
    cwd_branch: String,
    pub flash_duration: Duration,
    branch_update_rx: Option<flume::Receiver<()>>,
}

impl StatusBar {
    pub fn new(flash_duration: Duration) -> Self {
        Self {
            flash: None,
            started_at: Instant::now(),
            cwd_branch: cwd_branch_label(),
            flash_duration,
            branch_update_rx: spawn_branch_watcher(),
        }
    }

    pub fn flash(&mut self, msg: String) {
        self.flash = Some((msg, Instant::now()));
    }

    #[cfg(test)]
    pub fn flash_text(&self) -> Option<&str> {
        self.flash.as_ref().map(|(s, _)| s.as_str())
    }

    pub fn refresh_cwd(&mut self) {
        self.cwd_branch = cwd_branch_label();
    }

    pub fn poll_branch_update(&mut self) {
        let Some(rx) = &self.branch_update_rx else {
            return;
        };
        if rx.try_iter().next().is_some() {
            self.cwd_branch = cwd_branch_label();
        }
    }

    pub fn clear_flash(&mut self) {
        self.flash = None;
    }

    pub fn clear_expired_hint(&mut self) {
        if self
            .flash
            .as_ref()
            .is_some_and(|(_, t)| t.elapsed() >= self.flash_duration)
        {
            self.flash = None;
        }
    }

    pub fn view(&self, frame: &mut Frame, area: Rect, ctx: &StatusBarContext) {
        let rendered = fit(self.segments(ctx), usize::from(area.width));
        let (left, right): (Vec<_>, Vec<_>) = rendered
            .into_iter()
            .partition(|r| r.segment.side() == Side::Left);
        let left_spans: Vec<Span<'static>> = left.into_iter().flat_map(|r| r.spans).collect();
        let right_spans: Vec<Span<'static>> = right.into_iter().flat_map(|r| r.spans).collect();

        let [left_area, right_area] = Layout::horizontal([
            Constraint::Min(0),
            Constraint::Length(right_spans.iter().map(|s| s.width() as u16).sum()),
        ])
        .areas(area);

        frame.render_widget(Paragraph::new(Line::from(left_spans)), left_area);
        frame.render_widget(
            Paragraph::new(Line::from(right_spans)).alignment(Alignment::Right),
            right_area,
        );
    }

    fn segments(&self, ctx: &StatusBarContext) -> Vec<Rendered> {
        let mut out = Vec::new();
        let mut push = |segment: Segment, spans: Vec<Span<'static>>| {
            out.push(Rendered { segment, spans });
        };

        if *ctx.status == Status::Streaming && !ctx.task_line_visible {
            let mut spans = vec![Span::raw(" ")];
            spans.extend(State::Running.label_spans(self.started_at.elapsed().as_millis()));
            push(Segment::Running, spans);
        }

        if ctx.restoring {
            let ch = spinner_frame(self.started_at.elapsed().as_millis());
            push(
                Segment::Restoring,
                vec![Span::styled(
                    format!(" {ch}"),
                    theme::current().status_notice,
                )],
            );
        }

        push(
            Segment::Mode,
            vec![Span::styled(format!(" {}", ctx.mode_label), ctx.mode_style)],
        );

        if let Some(level) = &ctx.thinking {
            push(
                Segment::Thinking,
                vec![Span::styled(format!(" [{level}]"), label_style())],
            );
        }

        if let Some(name) = ctx.chat_name {
            push(
                Segment::Session,
                vec![Span::styled(format!(" [{name}]"), label_style())],
            );
        }

        if !ctx.auto_scroll {
            push(
                Segment::AutoScroll,
                vec![Span::styled(AUTO_SCROLL_PAUSED, label_style())],
            );
        }

        if let Some(retry) = ctx.retry_info {
            let mut spans = vec![Span::raw(" ")];
            spans.extend(State::NeedsAttention.label_spans(0));
            spans.push(Span::styled(
                format!(" {}", retry.message),
                theme::current().status_retry_error,
            ));
            if !ctx.task_line_visible {
                let secs = retry
                    .deadline
                    .saturating_duration_since(Instant::now())
                    .as_secs();
                spans.push(Span::styled(
                    format!("{RETRY_SEPARATOR}retrying in {secs}s (#{})", retry.attempt),
                    theme::current().status_retry_info,
                ));
            }
            push(Segment::Retry, spans);
        }

        if let Status::Error { message, .. } = ctx.status {
            let mut spans = vec![Span::raw(" ")];
            spans.extend(State::Failed.label_spans(0));
            spans.push(Span::styled(format!(" {message}"), theme::current().error));
            push(Segment::Error, spans);
        }

        if let Some((ref msg, _)) = self.flash {
            push(
                Segment::Flash,
                vec![Span::styled(
                    format!(" {msg}"),
                    theme::current().status_notice,
                )],
            );
        }

        let quota = quota_spans(ctx.subscription_usage, now_millis());
        if !quota.is_empty() {
            push(Segment::Quota, quota);
        }

        push(
            Segment::Cwd,
            vec![
                Span::styled(self.cwd_branch.clone(), label_style()),
                Span::raw("  "),
            ],
        );

        let mut model = vec![Span::styled(
            compact_model_name(ctx.model_id).to_string(),
            label_style(),
        )];
        if ctx.fast {
            model.push(Span::styled(FAST_LABEL, label_style()));
        }
        push(Segment::Model, model);

        let percentage = if ctx.stats.context_window > 0 {
            (ctx.stats.context_size as f64 / ctx.stats.context_window as f64 * 100.0) as u32
        } else {
            0
        };
        push(
            Segment::Context,
            context_spans(ctx.stats.context_size, ctx.stats.context_window, percentage),
        );

        if let Some(cost) = ctx.stats.cost {
            push(Segment::Cost, cost_spans("", cost));
        }

        if ctx.stats.show_global && !ctx.stats.pricing.is_zero() {
            push(
                Segment::GlobalCost,
                cost_spans(
                    GLOBAL_COST_PREFIX,
                    ctx.stats.global_usage.cost(ctx.stats.pricing, ctx.fast),
                ),
            );
        }

        out
    }
}

fn compact_model_name(model: &str) -> &str {
    model.rsplit_once('/').map_or(model, |(_, name)| name)
}

/// Live size is the value, the window and percentage the label around it, so
/// the eye lands on the number that moves. The value stays right-aligned in
/// its column while it grows.
fn context_spans(size: u32, window: u32, percentage: u32) -> Vec<Span<'static>> {
    vec![
        Span::styled(
            format!("  {}", right_align(&format_tokens(size), TOKENS_COL)),
            value_style(),
        ),
        Span::styled(
            format!(
                "/{} ({}%)",
                format_tokens(window),
                right_align(&percentage.to_string(), PERCENT_COL)
            ),
            label_style(),
        ),
    ]
}

fn cost_spans(prefix: &str, cost: f64) -> Vec<Span<'static>> {
    vec![
        Span::styled(format!(" {prefix}"), label_style()),
        Span::styled(
            format!("{} ", right_align(&format!("${cost:.3}"), COST_COL)),
            value_style(),
        ),
    ]
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn quota_spans(usage: &SubscriptionUsage, now: u64) -> Vec<Span<'static>> {
    let providers = [
        (CLAUDE_ICON, usage.anthropic.as_ref()),
        (OPENAI_ICON, usage.openai.as_ref()),
    ];
    let visible = providers
        .iter()
        .filter(|(_, usage)| usage.is_some_and(|usage| has_visible_limit(usage, now)))
        .count();
    let mut spans = Vec::new();
    for (icon, usage) in providers {
        let Some(usage) = usage else { continue };
        let Some(limit) = weekly_limit(usage, now) else {
            continue;
        };
        if !spans.is_empty() {
            spans.push(Span::raw("  "));
        }
        if visible > 1 {
            spans.push(Span::styled(
                format!("{icon} "),
                theme::current().status_dim,
            ));
        }
        let percentage = limit.percentage.unwrap_or_default();
        let reset = compact_reset(limit.reset_at.unwrap_or_default() - now);
        let text = format!("{percentage}% {RESET_ICON}{reset}");
        let style = if percentage > WARNING_PERCENTAGE {
            theme::current().error
        } else {
            theme::current().status_dim
        };
        spans.push(Span::styled(text, style));
    }
    if !spans.is_empty() {
        spans.push(Span::raw("  "));
    }
    spans
}

fn has_visible_limit(usage: &ProviderUsage, now: u64) -> bool {
    weekly_limit(usage, now).is_some()
}

fn weekly_limit(usage: &ProviderUsage, now: u64) -> Option<&UsageLimit> {
    usage
        .limits
        .iter()
        .find(|limit| limit.label.starts_with("Current week") && visible_limit(limit, now))
}

fn visible_limit(limit: &UsageLimit, now: u64) -> bool {
    limit.percentage.is_some() && limit.reset_at.is_some_and(|reset| reset > now)
}

fn compact_reset(milliseconds: u64) -> String {
    let minutes = milliseconds.div_ceil(60_000);
    if minutes < 60 {
        format!("{minutes}m")
    } else if minutes < 24 * 60 {
        format!("{}h", minutes.div_ceil(60))
    } else {
        format!("{}d", minutes.div_ceil(24 * 60))
    }
}

fn collapse_home(path: &str) -> String {
    let Some(home) = maki_storage::paths::home() else {
        return path.to_string();
    };
    collapse_home_with(path, &home.to_string_lossy())
}

fn collapse_home_with(path: &str, home: &str) -> String {
    path.strip_prefix(home)
        .map(|rest| format!("~{rest}"))
        .unwrap_or_else(|| path.to_string())
}

fn cwd_branch_label() -> String {
    let cwd = env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| ".".into());
    let label = collapse_home(&cwd);
    match detect_branch(&cwd) {
        Some(branch) => format!("{label}:{branch}"),
        None => label,
    }
}

fn detect_branch(cwd: &str) -> Option<String> {
    let head = std::fs::read_to_string(find_git_dir(Path::new(cwd))?.join("HEAD")).ok()?;
    let head = head.trim();
    head.strip_prefix("ref: refs/heads/")
        .map(str::to_string)
        .or_else(|| Some(head.get(..7)?.to_string()))
}

fn find_git_dir(cwd: &Path) -> Option<std::path::PathBuf> {
    let mut dir = cwd;
    loop {
        let git = dir.join(".git");
        if git.is_dir() {
            return Some(git);
        }
        dir = dir.parent()?;
    }
}

fn spawn_branch_watcher() -> Option<flume::Receiver<()>> {
    use notify::{RecursiveMode, Watcher};

    let cwd = env::current_dir().ok()?;
    let git_dir = find_git_dir(&cwd)?;
    let (tx, rx) = flume::bounded(1);

    std::thread::spawn(move || {
        let Ok(mut watcher) = notify::recommended_watcher(move |res: Result<notify::Event, _>| {
            if res.is_ok_and(|e| e.paths.iter().any(|p| p.ends_with("HEAD"))) {
                let _ = tx.try_send(());
            }
        }) else {
            return;
        };
        if watcher.watch(&git_dir, RecursiveMode::NonRecursive).is_ok() {
            std::thread::park();
        }
    });

    Some(rx)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use tempfile::TempDir;
    use test_case::test_case;

    #[test_case("/home/user/projects/app", "/home/user", "~/projects/app" ; "inside_home")]
    #[test_case("/tmp/other", "/home/user", "/tmp/other"                  ; "outside_home")]
    #[test_case("/home/user", "/home/user", "~"                           ; "exact_home")]
    fn collapse_home_cases(path: &str, home: &str, expected: &str) {
        assert_eq!(collapse_home_with(path, home), expected);
    }

    fn tmp_with_head(content: Option<&str>) -> (TempDir, String) {
        let dir = TempDir::new().unwrap();
        if let Some(head) = content {
            let git = dir.path().join(".git");
            fs::create_dir(&git).unwrap();
            fs::write(git.join("HEAD"), head).unwrap();
        }
        let path = dir.path().to_string_lossy().into_owned();
        (dir, path)
    }

    #[test_case(Some("ref: refs/heads/feature/foo\n"), Some("feature/foo") ; "regular_ref")]
    #[test_case(Some("abc1234deadbeef\n"),            Some("abc1234")      ; "detached_head")]
    #[test_case(None,                                 None                 ; "no_git_dir")]
    fn detect_branch_cases(head: Option<&str>, expected: Option<&str>) {
        let (_dir, path) = tmp_with_head(head);
        assert_eq!(detect_branch(&path), expected.map(String::from));
    }

    #[test_case("chatgpt-subscription/gpt-5.6-sol", "gpt-5.6-sol")]
    #[test_case("claude-opus-5", "claude-opus-5")]
    fn compact_model_name_cases(model: &str, expected: &str) {
        assert_eq!(compact_model_name(model), expected);
    }

    fn limit(label: &str, percentage: Option<u32>, reset_at: Option<u64>) -> UsageLimit {
        UsageLimit {
            label: label.into(),
            percentage,
            reset_at,
            detail: None,
        }
    }

    #[test]
    fn quota_formatting_shows_weekly_only_without_window_label() {
        let now = 1_000_000;
        let usage = SubscriptionUsage {
            anthropic: Some(ProviderUsage {
                plan: None,
                limits: vec![
                    limit(
                        "Current week (all models)",
                        Some(20),
                        Some(now + 86_400_000),
                    ),
                    limit("Current session", Some(65), Some(now + 3_600_000)),
                ],
            }),
            openai: Some(ProviderUsage {
                plan: None,
                limits: vec![limit("Current week", Some(90), Some(now + 604_800_000))],
            }),
        };
        let spans = quota_spans(&usage, now);
        let text: String = spans.iter().map(|span| span.content.as_ref()).collect();
        assert_eq!(
            text,
            format!("{CLAUDE_ICON} 20% {RESET_ICON}1d  {OPENAI_ICON} 90% {RESET_ICON}7d  ")
        );
        assert_eq!(spans[4].style, theme::current().error);
    }

    #[test]
    fn quota_formatting_omits_session_and_invalid_weekly_limits() {
        let now = 1_000_000;
        let usage = SubscriptionUsage {
            anthropic: None,
            openai: Some(ProviderUsage {
                plan: None,
                limits: vec![
                    limit("Current session", Some(40), Some(now + 30 * 60_000)),
                    limit("Current week", None, Some(now + 100_000)),
                    limit("Current week expired", Some(20), Some(now)),
                ],
            }),
        };
        assert!(quota_spans(&usage, now).is_empty());
    }

    #[test]
    fn detect_branch_from_subdirectory() {
        let (_dir, path) = tmp_with_head(Some("ref: refs/heads/main\n"));
        let sub = Path::new(&path).join("sub");
        fs::create_dir(&sub).unwrap();
        assert_eq!(
            detect_branch(&sub.to_string_lossy()),
            Some("main".to_string())
        );
    }

    #[test]
    fn clear_expired_hint_removes_stale_flash() {
        let mut bar = StatusBar::new(Duration::ZERO);
        bar.flash("Copied".into());
        bar.clear_expired_hint();
        assert!(bar.flash.is_none());
    }

    #[test]
    fn clear_flash_removes_flash() {
        let mut bar = StatusBar::new(Duration::from_secs(999));
        bar.flash("Copied".into());
        bar.clear_flash();
        assert!(bar.flash.is_none());
    }

    const CONTEXT_PREFIX: &str = "  ";
    const PERCENT_SUFFIX: &str = "%)";
    const CONTEXT_WINDOW: u32 = 1_000_000;
    const TEST_BRANCH: &str = "~/projects/app:main";
    const TEST_MODEL: &str = "anthropic/claude-opus-5";
    const TEST_MODE: &str = "plan";
    const TEST_SESSION: &str = "second";
    const TEST_THINKING: &str = "high";
    const ERROR_MESSAGE: &str = "provider refused the request";
    const RETRY_MESSAGE: &str = "rate limited";
    const RETRY_ATTEMPT: u32 = 3;
    const WIDE: usize = 200;
    const MEDIUM: usize = 70;
    const NARROW: usize = 40;
    const VERY_NARROW: usize = 16;

    fn text(spans: &[Span<'static>]) -> String {
        spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test_case(0, 0 ; "empty_context")]
    #[test_case(1_024, 12 ; "small_context")]
    #[test_case(950_000, 99 ; "nearly_full_context")]
    fn context_text_keeps_numbers_in_one_column(size: u32, percentage: u32) {
        let spans = context_spans(size, CONTEXT_WINDOW, percentage);
        let rendered = text(&spans);
        let (used, _) = rendered.split_once('/').unwrap();
        assert_eq!(used.len(), CONTEXT_PREFIX.len() + TOKENS_COL);
        let percent = rendered.rsplit_once('(').unwrap().1;
        assert_eq!(percent.len(), PERCENT_COL + PERCENT_SUFFIX.len());
    }

    #[test_case(0.0 ; "zero_cost")]
    #[test_case(1234.5 ; "large_cost")]
    fn cost_text_keeps_costs_in_one_column(cost: f64) {
        let rendered = text(&cost_spans("", cost));
        assert_eq!(rendered.len(), COST_COL.max(rendered.trim().len()) + 2);
    }

    #[test]
    fn context_and_cost_dim_the_label_and_emphasize_the_value() {
        let context = context_spans(1_024, CONTEXT_WINDOW, 12);
        assert_eq!(context[0].style, value_style());
        assert_eq!(context[1].style, label_style());
        let cost = cost_spans("", 1.0);
        assert_eq!(cost[0].style, label_style());
        assert_eq!(cost[1].style, value_style());
    }

    fn bar() -> StatusBar {
        let mut bar = StatusBar::new(Duration::from_secs(999));
        bar.cwd_branch = TEST_BRANCH.to_owned();
        bar
    }

    fn context<'a>(status: &'a Status, retry: Option<&'a RetryInfo>) -> StatusBarContext<'a> {
        static USAGE: TokenUsage = TokenUsage {
            input: 0,
            output: 0,
            cache_creation: 0,
            cache_read: 0,
        };
        static PRICING: ModelPricing = ModelPricing {
            input: 0.0,
            output: 0.0,
            cache_write: 0.0,
            cache_read: 0.0,
            fast: None,
        };
        static SUBSCRIPTION: SubscriptionUsage = SubscriptionUsage {
            anthropic: None,
            openai: None,
        };
        StatusBarContext {
            status,
            mode_label: TEST_MODE.into(),
            mode_style: Style::default(),
            model_id: TEST_MODEL,
            stats: UsageStats {
                global_usage: &USAGE,
                context_size: 1_024,
                cost: Some(1.5),
                pricing: &PRICING,
                context_window: CONTEXT_WINDOW,
                show_global: false,
            },
            auto_scroll: false,
            chat_name: Some(TEST_SESSION),
            retry_info: retry,
            thinking: Some(TEST_THINKING.into()),
            fast: false,
            restoring: false,
            subscription_usage: &SUBSCRIPTION,
            task_line_visible: false,
        }
    }

    fn surviving(bar: &StatusBar, ctx: &StatusBarContext, width: usize) -> Vec<Segment> {
        fit(bar.segments(ctx), width)
            .into_iter()
            .map(|r| r.segment)
            .collect()
    }

    #[test_case(WIDE, &[Segment::Mode, Segment::Thinking, Segment::Session, Segment::AutoScroll, Segment::Cwd, Segment::Model, Segment::Context, Segment::Cost] ; "wide_keeps_everything")]
    #[test_case(MEDIUM, &[Segment::Mode, Segment::Thinking, Segment::AutoScroll, Segment::Context, Segment::Cost] ; "medium_drops_environment")]
    #[test_case(NARROW, &[Segment::Mode, Segment::AutoScroll] ; "narrow_drops_usage")]
    #[test_case(VERY_NARROW, &[Segment::Mode] ; "very_narrow_keeps_primary_only")]
    fn width_degradation_evicts_lowest_priority_first(width: usize, expected: &[Segment]) {
        let status = Status::Idle;
        let ctx = context(&status, None);
        assert_eq!(surviving(&bar(), &ctx, width), expected, "at width {width}");
    }

    #[test_case(NARROW ; "narrow")]
    #[test_case(VERY_NARROW ; "very_narrow")]
    fn error_outranks_routine_metadata(width: usize) {
        let status = Status::error(ERROR_MESSAGE.to_owned());
        let ctx = context(&status, None);
        let kept = surviving(&bar(), &ctx, width);
        assert!(kept.contains(&Segment::Error), "{kept:?}");
        assert!(!kept.contains(&Segment::Cwd), "{kept:?}");
        assert!(!kept.contains(&Segment::Context), "{kept:?}");
    }

    #[test]
    fn blocking_retry_survives_the_narrowest_terminal() {
        let status = Status::Streaming;
        let retry = RetryInfo {
            attempt: RETRY_ATTEMPT,
            message: RETRY_MESSAGE.to_owned(),
            deadline: Instant::now() + Duration::from_secs(1),
        };
        let ctx = context(&status, Some(&retry));
        let kept = surviving(&bar(), &ctx, VERY_NARROW);
        assert_eq!(kept, vec![Segment::Retry]);
    }

    #[test]
    fn task_line_takes_over_run_phase_and_retry_countdown() {
        let status = Status::Streaming;
        let retry = RetryInfo {
            attempt: RETRY_ATTEMPT,
            message: RETRY_MESSAGE.to_owned(),
            deadline: Instant::now() + Duration::from_secs(1),
        };
        let bar = bar();
        let mut ctx = context(&status, Some(&retry));
        ctx.task_line_visible = true;
        let with_line = bar.segments(&ctx);
        assert!(with_line.iter().all(|r| r.segment != Segment::Running));
        let retry_text = text(
            &with_line
                .iter()
                .find(|r| r.segment == Segment::Retry)
                .expect("retry message stays")
                .spans,
        );
        assert!(retry_text.contains(RETRY_MESSAGE), "{retry_text}");
        assert!(!retry_text.contains(RETRY_SEPARATOR), "{retry_text}");

        ctx.task_line_visible = false;
        let without_line = bar.segments(&ctx);
        assert!(without_line.iter().any(|r| r.segment == Segment::Running));
    }
}
