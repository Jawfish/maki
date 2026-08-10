use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use maki_providers::provider::ProviderKind;
use maki_providers::{ProviderUsage, Timeouts};
use tracing::debug;

const POLL_INTERVAL: Duration = Duration::from_secs(3 * 60);

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SubscriptionUsage {
    pub anthropic: Option<ProviderUsage>,
    pub openai: Option<ProviderUsage>,
}

impl SubscriptionUsage {
    fn retain(&mut self, kind: ProviderKind, usage: Option<ProviderUsage>) {
        let Some(usage) = usage.filter(|usage| !usage.limits.is_empty()) else {
            return;
        };
        match kind {
            ProviderKind::Anthropic => self.anthropic = Some(usage),
            ProviderKind::OpenAi => self.openai = Some(usage),
            _ => {}
        }
    }
}

pub struct BackgroundSubscriptionUsage {
    pub usage: Arc<ArcSwap<SubscriptionUsage>>,
    pub task: smol::Task<()>,
}

pub fn spawn(timeouts: Timeouts) -> BackgroundSubscriptionUsage {
    let usage = Arc::new(ArcSwap::from_pointee(SubscriptionUsage::default()));
    let shared = Arc::clone(&usage);
    let task = smol::spawn(async move {
        loop {
            poll(&shared, timeouts).await;
            smol::Timer::after(POLL_INTERVAL).await;
        }
    });
    BackgroundSubscriptionUsage { usage, task }
}

async fn poll(shared: &ArcSwap<SubscriptionUsage>, timeouts: Timeouts) {
    let mut next = (**shared.load()).clone();
    for kind in [ProviderKind::Anthropic, ProviderKind::OpenAi] {
        let result = maki_providers::provider::fetch_subscription_usage(kind, timeouts).await;
        match result {
            Ok(usage) => next.retain(kind, usage),
            Err(error) => debug!(provider = %kind, %error, "subscription usage fetch failed"),
        }
    }
    shared.store(Arc::new(next));
}

#[cfg(test)]
mod tests {
    use maki_providers::UsageLimit;

    use super::*;

    fn usage(percentage: u32) -> ProviderUsage {
        ProviderUsage {
            plan: None,
            limits: vec![UsageLimit {
                label: "Current session".into(),
                percentage: Some(percentage),
                reset_at: Some(2_000_000_000_000),
                detail: None,
            }],
        }
    }

    #[test]
    fn retain_keeps_last_successful_nonempty_usage() {
        let mut retained = SubscriptionUsage::default();
        retained.retain(ProviderKind::Anthropic, Some(usage(25)));
        retained.retain(ProviderKind::Anthropic, None);
        retained.retain(ProviderKind::Anthropic, Some(ProviderUsage::default()));
        assert_eq!(retained.anthropic, Some(usage(25)));
    }

    #[test]
    fn retain_merges_provider_results_independently() {
        let mut retained = SubscriptionUsage::default();
        retained.retain(ProviderKind::OpenAi, Some(usage(70)));
        retained.retain(ProviderKind::Anthropic, Some(usage(30)));
        retained.retain(ProviderKind::OpenAi, Some(usage(80)));
        assert_eq!(retained.anthropic, Some(usage(30)));
        assert_eq!(retained.openai, Some(usage(80)));
    }
}
