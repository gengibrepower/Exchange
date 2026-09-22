#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct Price(i64);

impl Price {
    pub fn new(units_per_lot: i64) -> Self {
        Self(units_per_lot)
    }

    pub fn get(self) -> i64 {
        self.0
    }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct Lots(i64);

impl Lots {
    pub fn new(value: i64) -> Self {
        Self(value)
    }

    pub fn get(self) -> i64 {
        self.0
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
        assert!(Price::new(100) < Price::new(101));
    }

    #[test]
    fn order_construction() {
        let order = Order {
            id: OrderId::new(1),
            account: AccountId::new(1),
            instrument: InstrumentId::new(1),
            side: Side::Bid,
            price: Price::new(10_000),
            total: Lots::new(5),
            remaining: Lots::new(5),
            seq: Seq::new(1),
            status: OrderStatus::New,
        };

        assert_eq!(order.remaining, Lots::new(5));
    }
}
