#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DomainError {
    NonPositivePrice(i64),
    NegativeLots(i64),
}

impl std::fmt::Display for DomainError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NonPositivePrice(value) => write!(f, "preço {value} deve ser positivo"),
            Self::NegativeLots(value) => write!(f, "lots {value} não pode ser negativo"),
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
    pub fn new(value: u64) -> Self {
        Self(value)
    }

    pub fn get(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct InstrumentId(u32);

impl InstrumentId {
    pub fn new(value: u32) -> Self {
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

#[derive(Clone, Debug)]
pub struct Order {
    pub id: OrderId,
    pub account: AccountId,
    pub instrument: InstrumentId,
    pub side: Side,
    pub price: Price,
    pub total: Lots,
    pub remaining: Lots,
    pub seq: Seq,
    pub status: OrderStatus,
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

    #[test]
    fn order_construction() {
        let order = Order {
            id: OrderId::new(1),
            account: AccountId::new(1),
            instrument: InstrumentId::new(1),
            side: Side::Bid,
            price: Price::new(10_000).unwrap(),
            total: Lots::new(5).unwrap(),
            remaining: Lots::new(5).unwrap(),
            seq: Seq::new(1),
            status: OrderStatus::New,
        };

        assert_eq!(order.remaining, Lots::new(5).unwrap());
    }
}
