use trading_core::{AccountState, Broker, Candle, MarketContext, NewsItem, Order, Side, Strategy};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentStatus {
    Alive,
    /// Balance hit zero (or below). Agent stops trading permanently.
    Dead,
}

#[derive(Debug, Clone)]
pub enum AgentEvent {
    Tick { equity: f64 },
    /// A strategy order filled. `price` is the actual fill (P2-2 needs it
    /// for trade reconstruction — never drop fill data from the log).
    OrderPlaced { order: Order, price: f64 },
    OrderRejected(String),
    /// A stop-loss / take-profit exit fired. This is a forced close
    /// issued by the risk layer itself, independent of what the
    /// strategy wanted to do this tick. `closed_units` is the full
    /// position just flattened (P2-2 trade accounting).
    StopLossHit { price: f64, move_pct: f64, closed_units: f64 },
    TakeProfitHit { price: f64, move_pct: f64, closed_units: f64 },
    /// Equity crossed 2x the current baseline: half is "withdrawn" and
    /// the remaining half becomes the new baseline the agent keeps
    /// trading with, so the milestone can be hit again.
    Split { withdrawn: f64, new_baseline: f64 },
    Died { final_balance: f64 },
    /// A meta-strategy (router) switched its active candidate. Emitted
    /// on change only, so analytics can attribute performance per
    /// regime without log-spamming every tick.
    RegimeSelected { strategy: String },
}

/// Wraps a Strategy + Broker pair with:
///   - the account-lifecycle rules (die at zero, split 50/50 at double)
///   - an independent stop-loss / take-profit risk layer that runs
///     BEFORE the strategy gets a say each tick, so it reacts to price
///     movement immediately regardless of what the strategy (AI or
///     algorithmic) is doing or how fast it is.
pub struct Agent {
    pub id: String,
    pub symbol: String,
    strategy: Box<dyn Strategy>,
    broker: Box<dyn Broker>,
    status: AgentStatus,
    baseline: f64,
    pub total_withdrawn: f64,
    history: Vec<Candle>,
    recent_news: Vec<NewsItem>,
    max_history: usize,
    stop_loss_pct: Option<f64>,
    take_profit_pct: Option<f64>,
    last_active: Option<String>,
}

impl Agent {
    pub fn new(
        id: impl Into<String>,
        symbol: impl Into<String>,
        strategy: Box<dyn Strategy>,
        broker: Box<dyn Broker>,
        starting_balance: f64,
    ) -> Self {
        Self {
            id: id.into(),
            symbol: symbol.into(),
            strategy,
            broker,
            status: AgentStatus::Alive,
            baseline: starting_balance,
            total_withdrawn: 0.0,
            history: Vec::new(),
            recent_news: Vec::new(),
            max_history: 200,
            stop_loss_pct: None,
            take_profit_pct: None,
            last_active: None,
        }
    }

    pub fn with_stop_loss(mut self, pct: f64) -> Self {
        self.stop_loss_pct = Some(pct);
        self
    }

    pub fn with_take_profit(mut self, pct: f64) -> Self {
        self.take_profit_pct = Some(pct);
        self
    }

    pub fn status(&self) -> AgentStatus {
        self.status
    }

    pub fn baseline(&self) -> f64 {
        self.baseline
    }

    pub fn account_state(&self) -> AccountState {
        self.broker.account_state()
    }

    /// Rebuild an agent from a persisted snapshot (see P1-1): a broker
    /// already restored to its snapshotted position plus the snapshotted
    /// split-baseline and lifetime withdrawals. Price history and
    /// strategy internals (e.g. SMA crossover memory) are NOT restored —
    /// they rebuild over the next ticks, so expect up to `slow`-window
    /// ticks before crossover strategies fire again.
    pub fn restore(
        id: impl Into<String>,
        symbol: impl Into<String>,
        strategy: Box<dyn Strategy>,
        broker: Box<dyn Broker>,
        baseline: f64,
        total_withdrawn: f64,
    ) -> Self {
        let mut a = Self::new(id, symbol, strategy, broker, baseline);
        a.baseline = baseline;
        a.total_withdrawn = total_withdrawn;
        a
    }

    pub fn push_news(&mut self, item: NewsItem) {
        self.recent_news.push(item);
        if self.recent_news.len() > 10 {
            self.recent_news.remove(0);
        }
    }

    /// Called once before the tick loop starts (and again after any
    /// reconnect in a live deployment) to make sure the broker's view
    /// of the account matches reality before any trading resumes.
    pub async fn reconcile(&mut self) {
        if let Err(e) = self.broker.reconcile().await {
            eprintln!("[{}] reconcile failed: {e} — treating as untrusted until next successful reconcile", self.id);
        }
    }

    /// Independent risk layer: forces a closing order if the open
    /// position has moved beyond the stop-loss or take-profit
    /// threshold. Runs before the strategy is consulted.
    async fn check_risk_exits(&mut self) -> Option<AgentEvent> {
        let account = self.broker.account_state();
        if account.open_units == 0.0 {
            return None;
        }
        let entry = account.entry_price?;
        let last = self.broker.last_price()?;
        let move_pct = if account.open_units > 0.0 {
            (last - entry) / entry
        } else {
            (entry - last) / entry
        };

        let hit_sl = self.stop_loss_pct.map_or(false, |sl| move_pct <= -sl);
        let hit_tp = self.take_profit_pct.map_or(false, |tp| move_pct >= tp);
        if !hit_sl && !hit_tp {
            return None;
        }

        let side = if account.open_units > 0.0 { Side::Sell } else { Side::Buy };
        let closed_units = account.open_units.abs();
        let order = Order { symbol: self.symbol.clone(), side, units: closed_units };
        match self.broker.place_order(order).await {
            Ok(_) if hit_sl => Some(AgentEvent::StopLossHit { price: last, move_pct, closed_units }),
            Ok(_) => Some(AgentEvent::TakeProfitHit { price: last, move_pct, closed_units }),
            Err(_) => None, // couldn't close — next tick will try again
        }
    }

    /// Feed one new price tick through the agent. Order of operations
    /// matters and is deliberate:
    ///   1. mark to market
    ///   2. check death
    ///   3. risk exits (stop-loss/take-profit) — strategy-agnostic
    ///   4. strategy decision (skipped if a risk exit just fired)
    ///   5. death + split check again, post-trade
    pub async fn on_tick(&mut self, candle: Candle) -> Vec<AgentEvent> {
        let mut events = Vec::new();
        if self.status == AgentStatus::Dead {
            return events;
        }

        self.broker.mark_price(candle.close);
        self.history.push(candle);
        if self.history.len() > self.max_history {
            self.history.remove(0);
        }

        let account = self.broker.account_state();
        events.push(AgentEvent::Tick { equity: account.equity });

        if account.equity <= 0.0 {
            self.status = AgentStatus::Dead;
            events.push(AgentEvent::Died { final_balance: account.equity });
            return events;
        }

        if let Some(event) = self.check_risk_exits().await {
            events.push(event);
            let account = self.broker.account_state();
            if account.equity <= 0.0 {
                self.status = AgentStatus::Dead;
                events.push(AgentEvent::Died { final_balance: account.equity });
            }
            return events; // skip strategy this tick after a forced exit
        }

        let ctx = MarketContext {
            symbol: &self.symbol,
            history: &self.history,
            recent_news: &self.recent_news,
            account,
        };
        if let Some(order) = self.strategy.decide(&ctx).await {
            match self.broker.place_order(order.clone()).await {
                Ok(fill) => events.push(AgentEvent::OrderPlaced { order, price: fill.price }),
                Err(e) => events.push(AgentEvent::OrderRejected(e.to_string())),
            }
        }

        // Regime attribution (Tier 2): meta-strategies report their
        // active candidate via the trait hook; emit on change only
        // (including the first observation, so the starting regime is
        // on record). Leaf strategies report None — nothing emitted.
        let active = self.strategy.active_strategy().map(|s| s.to_string());
        if active != self.last_active {
            if let Some(ref name) = active {
                events.push(AgentEvent::RegimeSelected { strategy: name.clone() });
            }
            self.last_active = active;
        }

        let account = self.broker.account_state();
        if account.equity <= 0.0 {
            self.status = AgentStatus::Dead;
            events.push(AgentEvent::Died { final_balance: account.equity });
            return events;
        }
        if account.equity > self.baseline * 2.0 {
            let withdrawn = account.equity / 2.0;
            match self.broker.withdraw(withdrawn) {
                Ok(()) => {
                    // Baseline = post-withdraw equity, so the milestone
                    // cannot re-fire on the next tick at a flat price.
                    let post = self.broker.account_state();
                    self.total_withdrawn += withdrawn;
                    self.baseline = post.equity;
                    events.push(AgentEvent::Split { withdrawn, new_baseline: post.equity });
                }
                Err(e) => {
                    events.push(AgentEvent::OrderRejected(format!("split withdraw failed: {e}")));
                }
            }
        }

        events
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use broker_paper::PaperBroker;
    use chrono::Utc;

    fn candle(price: f64) -> Candle {
        Candle { time: Utc::now(), open: price, high: price, low: price, close: price }
    }

    struct Hold;
    #[async_trait]
    impl Strategy for Hold {
        fn name(&self) -> &str {
            "hold"
        }
        async fn decide(&mut self, _ctx: &MarketContext<'_>) -> Option<Order> {
            None
        }
    }

    /// Meta-strategy stub: holds, but reports a scripted active candidate
    /// per tick so regime events are testable without an LLM.
    struct RegimeStub {
        script: Vec<&'static str>,
        tick: usize,
    }
    #[async_trait]
    impl Strategy for RegimeStub {
        fn name(&self) -> &str {
            "regime-stub"
        }
        async fn decide(&mut self, _ctx: &MarketContext<'_>) -> Option<Order> {
            self.tick += 1;
            None
        }
        fn active_strategy(&self) -> Option<&str> {
            self.script.get(self.tick.saturating_sub(1)).copied()
        }
    }

    struct AlwaysBuy {
        units: f64,
    }
    #[async_trait]
    impl Strategy for AlwaysBuy {
        fn name(&self) -> &str {
            "always-buy"
        }
        async fn decide(&mut self, ctx: &MarketContext<'_>) -> Option<Order> {
            Some(Order { symbol: ctx.symbol.to_string(), side: Side::Buy, units: self.units })
        }
    }

    struct BuyOnce {
        units: f64,
        fired: bool,
    }
    #[async_trait]
    impl Strategy for BuyOnce {
        fn name(&self) -> &str {
            "buy-once"
        }
        async fn decide(&mut self, ctx: &MarketContext<'_>) -> Option<Order> {
            if self.fired {
                return None;
            }
            self.fired = true;
            Some(Order { symbol: ctx.symbol.to_string(), side: Side::Buy, units: self.units })
        }
    }

    /// Buys once, closes the full position on the next decision, then holds.
    /// Used to realize profit so split-withdraw (cash-only) can fire.
    struct BuyThenClose {
        units: f64,
        step: u8,
    }
    #[async_trait]
    impl Strategy for BuyThenClose {
        fn name(&self) -> &str {
            "buy-then-close"
        }
        async fn decide(&mut self, ctx: &MarketContext<'_>) -> Option<Order> {
            self.step += 1;
            match self.step {
                1 => Some(Order { symbol: ctx.symbol.to_string(), side: Side::Buy, units: self.units }),
                2 => Some(Order {
                    symbol: ctx.symbol.to_string(),
                    side: Side::Sell,
                    units: ctx.account.open_units.abs(),
                }),
                _ => None,
            }
        }
    }

    fn has_split(events: &[AgentEvent]) -> bool {
        events.iter().any(|e| matches!(e, AgentEvent::Split { .. }))
    }

    #[tokio::test]
    async fn stop_loss_fires_before_strategy() {
        let broker: Box<dyn Broker> = Box::new(PaperBroker::new(10_000.0));
        let mut agent = Agent::new("t", "EUR_USD", Box::new(AlwaysBuy { units: 10.0 }), broker, 10_000.0)
            .with_stop_loss(0.005);
        agent.on_tick(candle(1.0)).await; // opens long ~1.00006
        assert!(agent.account_state().open_units > 0.0);
        let events = agent.on_tick(candle(0.994)).await; // ~-0.6% -> SL
        assert!(
            events.iter().any(|e| matches!(e, AgentEvent::StopLossHit { .. })),
            "expected StopLossHit, got {events:?}"
        );
        assert!(
            events.iter().all(|e| !matches!(e, AgentEvent::OrderPlaced { .. })),
            "strategy must be skipped after risk exit: {events:?}"
        );
        assert_eq!(agent.account_state().open_units, 0.0);
    }

    #[tokio::test]
    async fn take_profit_fires_before_strategy() {
        let broker: Box<dyn Broker> = Box::new(PaperBroker::new(10_000.0));
        let mut agent = Agent::new("t", "EUR_USD", Box::new(AlwaysBuy { units: 10.0 }), broker, 10_000.0)
            .with_take_profit(0.01);
        agent.on_tick(candle(1.0)).await;
        let events = agent.on_tick(candle(1.02)).await; // ~+2% -> TP
        assert!(
            events.iter().any(|e| matches!(e, AgentEvent::TakeProfitHit { .. })),
            "expected TakeProfitHit, got {events:?}"
        );
        assert!(events.iter().all(|e| !matches!(e, AgentEvent::OrderPlaced { .. })));
        assert_eq!(agent.account_state().open_units, 0.0);
    }

    #[tokio::test]
    async fn split_fires_once_and_resets_baseline() {
        let broker: Box<dyn Broker> = Box::new(PaperBroker::new(100.0));
        let mut agent = Agent::new(
            "t",
            "EUR_USD",
            Box::new(BuyThenClose { units: 50.0, step: 0 }),
            broker,
            100.0,
        );
        agent.on_tick(candle(1.0)).await; // buy 50 @ ~1.0
        // Close the winner: realizes ~+105 cash, equity ~205 flat -> split fires.
        let events = agent.on_tick(candle(3.1)).await;
        assert!(has_split(&events), "expected Split, got {events:?}");
        let equity_after = agent.account_state().equity;
        assert!(
            agent.total_withdrawn > 90.0 && equity_after < 115.0,
            "withdrawn={} equity_after={equity_after} (buggy code leaves equity ~205)",
            agent.total_withdrawn
        );
        // Flat next tick must NOT re-fire.
        let events2 = agent.on_tick(candle(3.1)).await;
        assert!(!has_split(&events2), "split re-fired: {events2:?}");
    }

    #[tokio::test]
    async fn split_defers_while_profit_is_unrealized() {
        // Honest limitation: withdraw() is cash-only, so an open winner
        // whose profit is mostly unrealized cannot split yet. Must defer
        // (OrderRejected), never invent cash.
        let broker: Box<dyn Broker> = Box::new(PaperBroker::new(100.0));
        let mut agent = Agent::new(
            "t",
            "EUR_USD",
            Box::new(BuyOnce { units: 50.0, fired: false }),
            broker,
            100.0,
        );
        agent.on_tick(candle(1.0)).await;
        let events = agent.on_tick(candle(3.1)).await; // equity ~205, balance 100
        assert!(!has_split(&events), "must not split unrealized cash: {events:?}");
        assert!(
            events.iter().any(|e| matches!(e, AgentEvent::OrderRejected(_))),
            "expected deferral notice, got {events:?}"
        );
    }

    #[tokio::test]
    async fn death_at_zero_after_catastrophic_loss() {
        let broker: Box<dyn Broker> =
            Box::new(PaperBroker::new(10.0).with_max_leverage(5.0));
        let mut agent = Agent::new(
            "t",
            "EUR_USD",
            Box::new(BuyOnce { units: 45.0, fired: false }),
            broker,
            10.0,
        );
        agent.on_tick(candle(1.0)).await;
        let events = agent.on_tick(candle(0.5)).await;
        assert!(
            events.iter().any(|e| matches!(e, AgentEvent::Died { .. })),
            "expected Died, got {events:?}"
        );
        assert_eq!(agent.status(), AgentStatus::Dead);
        // Dead agents ignore further ticks.
        assert!(agent.on_tick(candle(0.5)).await.is_empty());
    }

    #[tokio::test]
    async fn death_at_zero_starting_balance() {
        let broker: Box<dyn Broker> = Box::new(PaperBroker::new(0.0));
        let mut agent = Agent::new("t", "EUR_USD", Box::new(Hold), broker, 0.0);
        let events = agent.on_tick(candle(1.0)).await;
        assert!(events.iter().any(|e| matches!(e, AgentEvent::Died { .. })));
    }

    #[tokio::test]
    async fn restore_rebuilds_position_baseline_and_withdrawn() {
        // Simulate a snapshot taken mid-run: $80 cash, long 5 @1.2,
        // baseline $100, $20 previously withdrawn.
        let broker: Box<dyn Broker> =
            Box::new(PaperBroker::restore(80.0, 5.0, 1.2).unwrap());
        let mut agent = Agent::restore("t", "EUR_USD", Box::new(Hold), broker, 100.0, 20.0);
        assert_eq!(agent.baseline(), 100.0);
        assert_eq!(agent.total_withdrawn, 20.0);
        // Mark at entry: equity == balance, position intact.
        let events = agent.on_tick(candle(1.2)).await;
        let st = agent.account_state();
        assert_eq!(st.open_units, 5.0);
        assert_eq!(st.entry_price, Some(1.2));
        assert!((st.equity - 80.0).abs() < 1e-9);
        assert!(!has_split(&events), "no spurious split on restore: {events:?}");
    }

    #[tokio::test]
    async fn regime_selected_emitted_on_change_only() {
        let broker: Box<dyn Broker> = Box::new(PaperBroker::new(100.0));
        let mut agent = Agent::new(
            "t",
            "EUR_USD",
            Box::new(RegimeStub { script: vec!["rsi", "rsi", "donch"], tick: 0 }),
            broker,
            100.0,
        );
        let regimes = |events: &[AgentEvent]| -> Vec<String> {
            events
                .iter()
                .filter_map(|e| match e {
                    AgentEvent::RegimeSelected { strategy } => Some(strategy.clone()),
                    _ => None,
                })
                .collect()
        };
        // Tick 1: first observation recorded. Tick 2: unchanged, silent.
        // Tick 3: switch emitted.
        assert_eq!(regimes(&agent.on_tick(candle(1.0)).await), vec!["rsi".to_string()]);
        assert!(regimes(&agent.on_tick(candle(1.0)).await).is_empty());
        assert_eq!(regimes(&agent.on_tick(candle(1.0)).await), vec!["donch".to_string()]);
    }

    #[tokio::test]
    async fn leaf_strategies_emit_no_regime_events() {
        let broker: Box<dyn Broker> = Box::new(PaperBroker::new(100.0));
        let mut agent = Agent::new("t", "EUR_USD", Box::new(Hold), broker, 100.0);
        for _ in 0..3 {
            let events = agent.on_tick(candle(1.0)).await;
            assert!(events.iter().all(|e| !matches!(e, AgentEvent::RegimeSelected { .. })));
        }
    }
}
