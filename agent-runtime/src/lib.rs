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
    OrderPlaced(Order),
    OrderRejected(String),
    /// A stop-loss / take-profit exit fired. This is a forced close
    /// issued by the risk layer itself, independent of what the
    /// strategy wanted to do this tick.
    StopLossHit { price: f64, move_pct: f64 },
    TakeProfitHit { price: f64, move_pct: f64 },
    /// Equity crossed 2x the current baseline: half is "withdrawn" and
    /// the remaining half becomes the new baseline the agent keeps
    /// trading with, so the milestone can be hit again.
    Split { withdrawn: f64, new_baseline: f64 },
    Died { final_balance: f64 },
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

    pub fn account_state(&self) -> AccountState {
        self.broker.account_state()
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
        let order = Order { symbol: self.symbol.clone(), side, units: account.open_units.abs() };
        match self.broker.place_order(order).await {
            Ok(_) if hit_sl => Some(AgentEvent::StopLossHit { price: last, move_pct }),
            Ok(_) => Some(AgentEvent::TakeProfitHit { price: last, move_pct }),
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
                Ok(_fill) => events.push(AgentEvent::OrderPlaced(order)),
                Err(e) => events.push(AgentEvent::OrderRejected(e.to_string())),
            }
        }

        let account = self.broker.account_state();
        if account.equity <= 0.0 {
            self.status = AgentStatus::Dead;
            events.push(AgentEvent::Died { final_balance: account.equity });
            return events;
        }
        if account.equity >= self.baseline * 2.0 {
            let withdrawn = account.equity / 2.0;
            let new_baseline = account.equity - withdrawn;
            self.total_withdrawn += withdrawn;
            self.baseline = new_baseline;
            events.push(AgentEvent::Split { withdrawn, new_baseline });
        }

        events
    }
}
