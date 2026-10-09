#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DomainError {
    NonPositivePrice(i64),
    NegativeLots(i64),
    EmptyOrder,
    EmptyFill,
    Overfill { remaining: Lots, requested: Lots },
    IllegalTransition { from: OrderStatus, to: OrderStatus },
}

impl std::fmt::Display for DomainError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NonPositivePrice(value) => write!(f, "preço {value} deve ser positivo"),
            Self::NegativeLots(value) => write!(f, "lots {value} não pode ser negativo"),
            Self::EmptyOrder => write!(f, "ordem precisa de pelo menos 1 lot"),
            Self::EmptyFill => write!(f, "execução precisa de pelo menos 1 lot"),
            Self::Overfill {
                remaining,
                requested,
            } => write!(
                f,
                "execução de {} lots excede os {} restantes",
                requested.get(),
                remaining.get()
            ),
            Self::IllegalTransition { from, to } => {
                write!(f, "transição ilegal de {from:?} para {to:?}")
            }
        }
    }
}

impl std::error::Error for DomainError {}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct Price(i64);

impl Price {
    /// # Errors
    /// [`DomainError::NonPositivePrice`] se `units_per_lot <= 0`.
    pub fn new(units_per_lot: i64) -> Result<Self, DomainError> {
        if units_per_lot <= 0 {
            Err(DomainError::NonPositivePrice(units_per_lot))
        } else {
            Ok(Self(units_per_lot))
        }
    }

    pub fn get(self) -> i64 {
        self.0
    }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct Lots(i64);

impl Lots {
    pub const ZERO: Self = Self(0);

    /// # Errors
    /// [`DomainError::NegativeLots`] se `value < 0`.
    pub fn new(value: i64) -> Result<Self, DomainError> {
        if value < 0 {
            Err(DomainError::NegativeLots(value))
        } else {
            Ok(Self(value))
        }
    }

    pub fn get(self) -> i64 {
        self.0
    }

    pub fn checked_sub(self, rhs: Self) -> Option<Self> {
        match self.0.checked_sub(rhs.0) {
            Some(n) if n >= 0 => Some(Self(n)),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct OrderId(u64);

impl OrderId {
    pub fn new(value: u64) -> Self {
        Self(value)
    }

    pub fn get(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct Seq(u64);

impl Seq {
    pub fn new(value: u64) -> Self {
        Self(value)
    }

    pub fn get(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct AccountId(u64);

impl AccountId {
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    pub fn get(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct InstrumentId(u32);

impl InstrumentId {
    pub const fn new(value: u32) -> Self {
        Self(value)
    }

    pub fn get(self) -> u32 {
        self.0
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Fill {
    pub taker: OrderId,
    pub maker: OrderId,
    pub price: Price,
    pub lots: Lots,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Side {
    Bid,
    Ask,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum OrderStatus {
    New,
    Rejected,
    Accepted,
    PartiallyFilled,
    Filled,
    Cancelled,
    Expired,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum TimeInForce {
    #[default]
    Gtc,
    Ioc,
    Fok,
}

impl OrderStatus {
    pub fn can_transition_to(self, to: Self) -> bool {
        matches!(
            (self, to),
            (Self::New, Self::Accepted | Self::Rejected)
                | (
                    Self::Accepted,
                    Self::PartiallyFilled | Self::Filled | Self::Cancelled | Self::Expired
                )
                | (
                    Self::PartiallyFilled,
                    Self::PartiallyFilled | Self::Filled | Self::Cancelled
                )
        )
    }
}

#[derive(Clone, Debug)]
pub struct Order {
    id: OrderId,
    account: AccountId,
    instrument: InstrumentId,
    side: Side,
    price: Price,
    total: Lots,
    remaining: Lots,
    seq: Seq,
    status: OrderStatus,
    time_in_force: TimeInForce,
}

impl Order {
    /// # Errors
    /// [`DomainError::EmptyOrder`] se `total` for zero.
    pub fn new(
        id: OrderId,
        account: AccountId,
        instrument: InstrumentId,
        side: Side,
        price: Price,
        total: Lots,
        seq: Seq,
    ) -> Result<Self, DomainError> {
        if total == Lots::ZERO {
            return Err(DomainError::EmptyOrder);
        }
        Ok(Self {
            id,
            account,
            instrument,
            side,
            price,
            total,
            remaining: total,
            seq,
            status: OrderStatus::New,
            time_in_force: TimeInForce::default(),
        })
    }

    #[must_use]
    pub fn with_time_in_force(mut self, time_in_force: TimeInForce) -> Self {
        self.time_in_force = time_in_force;
        self
    }

    pub fn id(&self) -> OrderId {
        self.id
    }

    pub fn account(&self) -> AccountId {
        self.account
    }

    pub fn instrument(&self) -> InstrumentId {
        self.instrument
    }

    pub fn side(&self) -> Side {
        self.side
    }

    pub fn price(&self) -> Price {
        self.price
    }

    pub fn total(&self) -> Lots {
        self.total
    }

    pub fn remaining(&self) -> Lots {
        self.remaining
    }

    pub fn seq(&self) -> Seq {
        self.seq
    }

    pub fn status(&self) -> OrderStatus {
        self.status
    }

    pub fn time_in_force(&self) -> TimeInForce {
        self.time_in_force
    }

    /// Reduz o restante e move o status junto (`PartiallyFilled` ou `Filled`).
    ///
    /// # Errors
    /// Verificados nesta ordem; em qualquer erro, nada muda:
    /// [`DomainError::EmptyFill`] se `lots` for zero,
    /// [`DomainError::Overfill`] se `lots` for maior que o restante,
    /// [`DomainError::IllegalTransition`] se o status atual não puder ir para o de destino.
    pub fn fill(&mut self, lots: Lots) -> Result<(), DomainError> {
        if lots == Lots::ZERO {
            return Err(DomainError::EmptyFill);
        }
        let remaining = self
            .remaining
            .checked_sub(lots)
            .ok_or(DomainError::Overfill {
                remaining: self.remaining,
                requested: lots,
            })?;
        let to = if remaining == Lots::ZERO {
            OrderStatus::Filled
        } else {
            OrderStatus::PartiallyFilled
        };
        if !self.status.can_transition_to(to) {
            return Err(DomainError::IllegalTransition {
                from: self.status,
                to,
            });
        }
        self.remaining = remaining;
        self.status = to;
        Ok(())
    }

    /// # Errors
    /// [`DomainError::IllegalTransition`] se a seção 3.4 do escopo não permitir; nada muda.
    pub fn transition(&mut self, to: OrderStatus) -> Result<(), DomainError> {
        if !self.status.can_transition_to(to) {
            return Err(DomainError::IllegalTransition {
                from: self.status,
                to,
            });
        }
        self.status = to;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn price_orders_ascending() {
        assert!(Price::new(100).unwrap() < Price::new(101).unwrap());
    }

    #[test]
    fn price_rejects_zero_and_negative() {
        assert_eq!(Price::new(0), Err(DomainError::NonPositivePrice(0)));
        assert_eq!(Price::new(-3), Err(DomainError::NonPositivePrice(-3)));
    }

    #[test]
    fn price_accepts_positive() {
        assert_eq!(Price::new(1).map(Price::get), Ok(1));
    }

    #[test]
    fn lots_rejects_negative() {
        assert_eq!(Lots::new(-1), Err(DomainError::NegativeLots(-1)));
    }

    #[test]
    fn lots_accepts_zero() {
        assert_eq!(Lots::new(0), Ok(Lots::ZERO));
    }

    #[test]
    fn checked_sub_subtracts_down_to_zero() {
        let five = Lots::new(5).unwrap();
        assert_eq!(five.checked_sub(Lots::new(2).unwrap()), Lots::new(3).ok());
        assert_eq!(five.checked_sub(five), Some(Lots::ZERO));
    }

    #[test]
    fn checked_sub_refuses_to_go_negative() {
        let two = Lots::new(2).unwrap();
        assert_eq!(two.checked_sub(Lots::new(5).unwrap()), None);
    }

    #[test]
    fn domain_error_messages_carry_the_value() {
        assert!(DomainError::NonPositivePrice(-3).to_string().contains("-3"));
        assert!(DomainError::NegativeLots(-1).to_string().contains("-1"));
    }

    const ALL_STATUSES: [OrderStatus; 7] = [
        OrderStatus::New,
        OrderStatus::Rejected,
        OrderStatus::Accepted,
        OrderStatus::PartiallyFilled,
        OrderStatus::Filled,
        OrderStatus::Cancelled,
        OrderStatus::Expired,
    ];

    const TERMINAL_STATUSES: [OrderStatus; 4] = [
        OrderStatus::Rejected,
        OrderStatus::Filled,
        OrderStatus::Cancelled,
        OrderStatus::Expired,
    ];

    fn order_with_total(total: i64) -> Result<Order, DomainError> {
        Order::new(
            OrderId::new(1),
            AccountId::new(1),
            InstrumentId::new(1),
            Side::Bid,
            Price::new(10_000).unwrap(),
            Lots::new(total).unwrap(),
            Seq::new(1),
        )
    }

    #[test]
    fn new_order_defaults_to_gtc() {
        let order = order_with_total(5).unwrap();

        assert_eq!(order.time_in_force(), TimeInForce::Gtc);
        assert_eq!(
            order.with_time_in_force(TimeInForce::Fok).time_in_force(),
            TimeInForce::Fok
        );
    }

    #[test]
    fn new_order_starts_new_with_everything_remaining() {
        let order = order_with_total(5).unwrap();

        assert_eq!(order.status(), OrderStatus::New);
        assert_eq!(order.remaining(), Lots::new(5).unwrap());
        assert_eq!(order.total(), Lots::new(5).unwrap());
    }

    #[test]
    fn order_rejects_zero_lots() {
        assert_eq!(order_with_total(0).unwrap_err(), DomainError::EmptyOrder);
    }

    fn accepted_order(total: i64) -> Order {
        let mut order = order_with_total(total).unwrap();
        order.transition(OrderStatus::Accepted).unwrap();
        order
    }

    #[test]
    fn partial_fill_reduces_remaining_and_moves_to_partially_filled() {
        let mut order = accepted_order(5);

        order.fill(Lots::new(2).unwrap()).unwrap();

        assert_eq!(order.remaining(), Lots::new(3).unwrap());
        assert_eq!(order.total(), Lots::new(5).unwrap());
        assert_eq!(order.status(), OrderStatus::PartiallyFilled);
    }

    #[test]
    fn complete_fill_moves_to_filled() {
        let mut order = accepted_order(5);

        order.fill(Lots::new(5).unwrap()).unwrap();

        assert_eq!(order.remaining(), Lots::ZERO);
        assert_eq!(order.status(), OrderStatus::Filled);
    }

    #[test]
    fn successive_partial_fills_end_filled() {
        let mut order = accepted_order(5);

        order.fill(Lots::new(2).unwrap()).unwrap();
        order.fill(Lots::new(2).unwrap()).unwrap();
        assert_eq!(order.status(), OrderStatus::PartiallyFilled);

        order.fill(Lots::new(1).unwrap()).unwrap();
        assert_eq!(order.status(), OrderStatus::Filled);
    }

    #[test]
    fn overfill_is_rejected_and_changes_nothing() {
        let mut order = accepted_order(2);

        let result = order.fill(Lots::new(3).unwrap());

        assert_eq!(
            result,
            Err(DomainError::Overfill {
                remaining: Lots::new(2).unwrap(),
                requested: Lots::new(3).unwrap(),
            })
        );
        assert_eq!(order.remaining(), Lots::new(2).unwrap());
        assert_eq!(order.status(), OrderStatus::Accepted);
    }

    #[test]
    fn empty_fill_is_rejected_and_changes_nothing() {
        let mut order = accepted_order(5);

        assert_eq!(order.fill(Lots::ZERO), Err(DomainError::EmptyFill));
        assert_eq!(order.remaining(), Lots::new(5).unwrap());
        assert_eq!(order.status(), OrderStatus::Accepted);
    }

    #[test]
    fn fill_on_order_not_yet_accepted_is_rejected_and_changes_nothing() {
        let mut order = order_with_total(5).unwrap();

        let result = order.fill(Lots::new(2).unwrap());

        assert_eq!(
            result,
            Err(DomainError::IllegalTransition {
                from: OrderStatus::New,
                to: OrderStatus::PartiallyFilled,
            })
        );
        assert_eq!(order.remaining(), Lots::new(5).unwrap());
        assert_eq!(order.status(), OrderStatus::New);
    }

    #[test]
    fn fill_on_filled_order_is_overfill() {
        let mut order = accepted_order(1);
        order.fill(Lots::new(1).unwrap()).unwrap();

        let result = order.fill(Lots::new(1).unwrap());

        assert_eq!(
            result,
            Err(DomainError::Overfill {
                remaining: Lots::ZERO,
                requested: Lots::new(1).unwrap(),
            })
        );
        assert_eq!(order.status(), OrderStatus::Filled);
    }

    #[test]
    fn happy_path_is_legal() {
        assert!(OrderStatus::New.can_transition_to(OrderStatus::Accepted));
        assert!(OrderStatus::Accepted.can_transition_to(OrderStatus::PartiallyFilled));
        assert!(OrderStatus::PartiallyFilled.can_transition_to(OrderStatus::Filled));
    }

    #[test]
    fn terminal_states_never_leave() {
        for from in TERMINAL_STATUSES {
            for to in ALL_STATUSES {
                assert!(!from.can_transition_to(to), "{from:?} -> {to:?}");
            }
        }
    }

    #[test]
    fn nothing_returns_to_new() {
        for from in ALL_STATUSES {
            assert!(!from.can_transition_to(OrderStatus::New), "{from:?} -> New");
        }
    }

    #[test]
    fn only_new_can_be_rejected_or_accepted() {
        for from in ALL_STATUSES {
            let is_new = from == OrderStatus::New;
            assert_eq!(
                from.can_transition_to(OrderStatus::Rejected),
                is_new,
                "{from:?}"
            );
            assert_eq!(
                from.can_transition_to(OrderStatus::Accepted),
                is_new,
                "{from:?}"
            );
        }
    }

    #[test]
    fn new_cannot_skip_acceptance() {
        for to in [
            OrderStatus::PartiallyFilled,
            OrderStatus::Filled,
            OrderStatus::Cancelled,
            OrderStatus::Expired,
        ] {
            assert!(!OrderStatus::New.can_transition_to(to), "New -> {to:?}");
        }
    }

    #[test]
    fn fills_can_complete_at_once_or_repeat_partially() {
        assert!(OrderStatus::Accepted.can_transition_to(OrderStatus::Filled));
        assert!(OrderStatus::PartiallyFilled.can_transition_to(OrderStatus::PartiallyFilled));
    }

    #[test]
    fn partial_order_can_be_cancelled_but_not_expired() {
        assert!(OrderStatus::Accepted.can_transition_to(OrderStatus::Cancelled));
        assert!(OrderStatus::PartiallyFilled.can_transition_to(OrderStatus::Cancelled));
        assert!(OrderStatus::Accepted.can_transition_to(OrderStatus::Expired));
        assert!(!OrderStatus::PartiallyFilled.can_transition_to(OrderStatus::Expired));
    }

    #[test]
    fn transition_applies_legal_move() {
        let mut order = order_with_total(5).unwrap();

        order.transition(OrderStatus::Accepted).unwrap();

        assert_eq!(order.status(), OrderStatus::Accepted);
    }

    #[test]
    fn transition_refuses_illegal_move_and_keeps_status() {
        let mut order = order_with_total(5).unwrap();

        let result = order.transition(OrderStatus::Filled);

        assert_eq!(
            result,
            Err(DomainError::IllegalTransition {
                from: OrderStatus::New,
                to: OrderStatus::Filled,
            })
        );
        assert_eq!(order.status(), OrderStatus::New);
    }
}
