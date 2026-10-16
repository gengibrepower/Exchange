use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DomainError {
    NonPositivePrice(i64),
    NegativeLots(i64),
    NegativeAmount(i64),
    BpsAboveFull(u32),
    SameAsset(AssetId),
    NonPositiveLotSize,
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
            Self::NegativeAmount(value) => write!(f, "quantia {value} não pode ser negativa"),
            Self::BpsAboveFull(value) => {
                write!(f, "{value} bps passa de 100% (máximo {})", Bps::FULL)
            }
            Self::SameAsset(asset) => {
                write!(
                    f,
                    "base e quote não podem ser o mesmo ativo ({})",
                    asset.get()
                )
            }
            Self::NonPositiveLotSize => write!(f, "tamanho do lot deve ser positivo"),
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

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Serialize, Deserialize)]
#[serde(try_from = "i64", into = "i64")]
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

    /// Valor de `lots` a este preço, em unidades-mínimas da quote: `preço × lots`.
    pub fn times(self, lots: Lots) -> Option<Amount> {
        self.0.checked_mul(lots.0).map(Amount)
    }

    pub fn get(self) -> i64 {
        self.0
    }
}

impl TryFrom<i64> for Price {
    type Error = DomainError;

    fn try_from(value: i64) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<Price> for i64 {
    fn from(value: Price) -> Self {
        value.0
    }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Serialize, Deserialize)]
#[serde(try_from = "i64", into = "i64")]
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

    pub fn checked_add(self, rhs: Self) -> Option<Self> {
        self.0.checked_add(rhs.0).map(Self)
    }

    pub fn checked_sub(self, rhs: Self) -> Option<Self> {
        match self.0.checked_sub(rhs.0) {
            Some(n) if n >= 0 => Some(Self(n)),
            _ => None,
        }
    }
}

impl TryFrom<i64> for Lots {
    type Error = DomainError;

    fn try_from(value: i64) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<Lots> for i64 {
    fn from(value: Lots) -> Self {
        value.0
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
#[serde(transparent)]
pub struct OrderId(u64);

impl OrderId {
    pub fn new(value: u64) -> Self {
        Self(value)
    }

    pub fn get(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Seq(u64);

impl Seq {
    pub fn new(value: u64) -> Self {
        Self(value)
    }

    pub fn get(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AccountId(u64);

impl AccountId {
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    pub fn get(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
#[serde(transparent)]
pub struct InstrumentId(u32);

impl InstrumentId {
    pub const fn new(value: u32) -> Self {
        Self(value)
    }

    pub fn get(self) -> u32 {
        self.0
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AssetId(u32);

impl AssetId {
    pub const fn new(value: u32) -> Self {
        Self(value)
    }

    pub fn get(self) -> u32 {
        self.0
    }
}

/// Quantia de um ativo em unidades-mínimas (centavos de BRL, unidades de TESTE...).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Default, Serialize, Deserialize)]
#[serde(try_from = "i64", into = "i64")]
pub struct Amount(i64);

impl Amount {
    pub const ZERO: Self = Self(0);

    /// # Errors
    /// [`DomainError::NegativeAmount`] se `value < 0`.
    pub fn new(value: i64) -> Result<Self, DomainError> {
        if value < 0 {
            Err(DomainError::NegativeAmount(value))
        } else {
            Ok(Self(value))
        }
    }

    pub fn get(self) -> i64 {
        self.0
    }

    pub fn checked_add(self, rhs: Self) -> Option<Self> {
        self.0.checked_add(rhs.0).map(Self)
    }

    /// `self × lots`; usado com o `lot_size` para converter lots em unidades da base.
    pub fn times(self, lots: Lots) -> Option<Self> {
        self.0.checked_mul(lots.0).map(Self)
    }

    pub fn checked_sub(self, rhs: Self) -> Option<Self> {
        match self.0.checked_sub(rhs.0) {
            Some(n) if n >= 0 => Some(Self(n)),
            _ => None,
        }
    }
}

impl TryFrom<i64> for Amount {
    type Error = DomainError;

    fn try_from(value: i64) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<Amount> for i64 {
    fn from(value: Amount) -> Self {
        value.0
    }
}

/// Basis points: 1 bp = 0,01%. De 0 a 10.000 (100%).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Bps(u32);

impl Bps {
    pub const FULL: u32 = 10_000;

    /// # Errors
    /// [`DomainError::BpsAboveFull`] se `value` passar de 10.000.
    pub fn new(value: u32) -> Result<Self, DomainError> {
        if value > Self::FULL {
            Err(DomainError::BpsAboveFull(value))
        } else {
            Ok(Self(value))
        }
    }

    pub fn get(self) -> u32 {
        self.0
    }

    /// `⌈value × bps / 10.000⌉`: a regra de arredondamento do dinheiro do sistema,
    /// para cima, a favor da casa. Nunca passa de `value`.
    ///
    /// # Panics
    /// Nunca: com `bps <= 10.000` a taxa não passa de `value`, que cabe em `i64`.
    pub fn fee_on(self, value: Amount) -> Amount {
        let full = i128::from(Self::FULL);
        let fee = (i128::from(value.get()) * i128::from(self.0) + full - 1) / full;
        Amount(i64::try_from(fee).expect("com bps <= 100% a taxa não passa do valor, que é i64"))
    }
}

/// Par negociável: `base` muda de mão, `quote` paga. Preço em unidades-mínimas de
/// `quote` por lot; um lot são `lot_size` unidades-mínimas de `base`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Instrument {
    id: InstrumentId,
    base: AssetId,
    quote: AssetId,
    tick: Price,
    lot_size: Amount,
}

impl Instrument {
    /// # Errors
    /// Verificados nesta ordem:
    /// [`DomainError::SameAsset`] se `base == quote`,
    /// [`DomainError::NonPositiveLotSize`] se `lot_size` for zero.
    pub fn new(
        id: InstrumentId,
        base: AssetId,
        quote: AssetId,
        tick: Price,
        lot_size: Amount,
    ) -> Result<Self, DomainError> {
        if base == quote {
            return Err(DomainError::SameAsset(base));
        }
        if lot_size == Amount::ZERO {
            return Err(DomainError::NonPositiveLotSize);
        }
        Ok(Self {
            id,
            base,
            quote,
            tick,
            lot_size,
        })
    }

    pub fn id(&self) -> InstrumentId {
        self.id
    }

    pub fn base(&self) -> AssetId {
        self.base
    }

    pub fn quote(&self) -> AssetId {
        self.quote
    }

    pub fn tick(&self) -> Price {
        self.tick
    }

    pub fn lot_size(&self) -> Amount {
        self.lot_size
    }

    /// O preço é múltiplo do tick?
    pub fn accepts_price(&self, price: Price) -> bool {
        price.get() % self.tick.get() == 0
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Fill {
    pub taker: OrderId,
    pub maker: OrderId,
    pub price: Price,
    pub lots: Lots,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
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

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum OrderKind {
    Limit(Price),
    /// Sem preço próprio; `protection` é o pior preço aceitável (máximo na compra,
    /// mínimo na venda). É uma trava, não um preço de negócio: não precisa seguir o tick.
    Market {
        protection: Option<Price>,
    },
    /// Dormente fora do livro até o último negócio do instrumento chegar a `trigger` (na
    /// compra, `último >= trigger`; na venda, `último <= trigger`); aí entra como ordem
    /// limite em `limit` ou, sem ele, a mercado. Variante nova no fim do enum: o postcard
    /// grava o índice da variante, então logs antigos continuam legíveis.
    Stop {
        trigger: Price,
        limit: Option<Price>,
    },
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
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

/// O que o cliente pediu: dado puro, sem estado de execução. É isto que viaja no
/// comando e vai para o log; o `Order` com status e restante só existe dentro do núcleo.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct OrderRequest {
    pub id: OrderId,
    pub account: AccountId,
    pub instrument: InstrumentId,
    pub side: Side,
    pub kind: OrderKind,
    pub total: Lots,
    pub seq: Seq,
    pub time_in_force: TimeInForce,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Order {
    id: OrderId,
    account: AccountId,
    instrument: InstrumentId,
    side: Side,
    kind: OrderKind,
    total: Lots,
    remaining: Lots,
    seq: Seq,
    status: OrderStatus,
    time_in_force: TimeInForce,
}

impl Order {
    /// A única porta de entrada de uma ordem no núcleo: nasce `New`, com tudo restante.
    ///
    /// # Errors
    /// [`DomainError::EmptyOrder`] se `request.total` for zero.
    pub(crate) fn from_request(request: OrderRequest) -> Result<Self, DomainError> {
        if request.total == Lots::ZERO {
            return Err(DomainError::EmptyOrder);
        }
        Ok(Self {
            id: request.id,
            account: request.account,
            instrument: request.instrument,
            side: request.side,
            kind: request.kind,
            total: request.total,
            remaining: request.total,
            seq: request.seq,
            status: OrderStatus::New,
            time_in_force: request.time_in_force,
        })
    }

    #[cfg(test)]
    #[must_use]
    pub(crate) fn with_time_in_force(mut self, time_in_force: TimeInForce) -> Self {
        self.time_in_force = time_in_force;
        self
    }

    /// Dá a uma ordem a mercado sem proteção a proteção implícita do colar de preço.
    /// Qualquer outra ordem volta inalterada: só o núcleo decide isso, antes do livro.
    #[must_use]
    pub(crate) fn with_implicit_protection(mut self, bound: Price) -> Self {
        if self.kind == (OrderKind::Market { protection: None }) {
            self.kind = OrderKind::Market {
                protection: Some(bound),
            };
        }
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

    pub fn kind(&self) -> OrderKind {
        self.kind
    }

    /// Preço em que a ordem descansaria: só ordem limite tem. Um stop, enquanto dorme,
    /// não descansa em preço nenhum.
    pub fn limit_price(&self) -> Option<Price> {
        match self.kind {
            OrderKind::Limit(price) => Some(price),
            OrderKind::Market { .. } | OrderKind::Stop { .. } => None,
        }
    }

    /// Os preços que o cliente escreveu e que precisam cair no tick: o limite, ou o
    /// gatilho e o limite de um stop. A proteção de ordem a mercado é trava, não preço.
    pub fn quoted_prices(&self) -> [Option<Price>; 2] {
        match self.kind {
            OrderKind::Limit(price) => [Some(price), None],
            OrderKind::Market { .. } => [None, None],
            OrderKind::Stop { trigger, limit } => [Some(trigger), limit],
        }
    }

    /// O gatilho, se a ordem for um stop.
    pub fn trigger(&self) -> Option<Price> {
        match self.kind {
            OrderKind::Stop { trigger, .. } => Some(trigger),
            OrderKind::Limit(_) | OrderKind::Market { .. } => None,
        }
    }

    /// O stop acordado: vira ordem limite no seu limite ou a mercado sem proteção (o
    /// núcleo dá a proteção do colar na hora, como a qualquer ordem que chega). Mantém id,
    /// restante, status e seq. Qualquer outra ordem volta inalterada.
    #[must_use]
    pub(crate) fn triggered(mut self) -> Self {
        if let OrderKind::Stop { limit, .. } = self.kind {
            self.kind = match limit {
                Some(price) => OrderKind::Limit(price),
                None => OrderKind::Market { protection: None },
            };
        }
        self
    }

    /// Aperta a proteção de uma ordem a mercado para no máximo `cap` na compra (no mínimo,
    /// na venda). É o que mantém um stop disparado dentro da reserva feita na entrada.
    #[must_use]
    pub(crate) fn with_protection_within(mut self, cap: Price) -> Self {
        if let OrderKind::Market {
            protection: Some(bound),
        } = self.kind
        {
            let tighter = match self.side {
                Side::Bid => bound.min(cap),
                Side::Ask => bound.max(cap),
            };
            self.kind = OrderKind::Market {
                protection: Some(tighter),
            };
        }
        self
    }

    /// Pior preço que a ordem aceita: o limite, ou a proteção de uma ordem a mercado.
    /// Um stop dormente ainda não aceita preço nenhum.
    pub fn price_bound(&self) -> Option<Price> {
        match self.kind {
            OrderKind::Limit(bound)
            | OrderKind::Market {
                protection: Some(bound),
            } => Some(bound),
            OrderKind::Market { protection: None } | OrderKind::Stop { .. } => None,
        }
    }

    /// Esta ordem, como taker, cruza com um maker parado em `maker_price`? Um stop
    /// dormente não cruza nada: só depois de disparado ele vira taker.
    pub fn crosses(&self, maker_price: Price) -> bool {
        match (self.side, self.kind) {
            (_, OrderKind::Market { protection: None }) => true,
            (_, OrderKind::Stop { .. }) => false,
            (
                Side::Bid,
                OrderKind::Limit(bound)
                | OrderKind::Market {
                    protection: Some(bound),
                },
            ) => maker_price <= bound,
            (
                Side::Ask,
                OrderKind::Limit(bound)
                | OrderKind::Market {
                    protection: Some(bound),
                },
            ) => maker_price >= bound,
        }
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

    fn request(side: Side, kind: OrderKind, total: Lots) -> OrderRequest {
        OrderRequest {
            id: OrderId::new(1),
            account: AccountId::new(1),
            instrument: InstrumentId::new(1),
            side,
            kind,
            total,
            seq: Seq::new(1),
            time_in_force: TimeInForce::Gtc,
        }
    }

    fn order_with_total(total: i64) -> Result<Order, DomainError> {
        let kind = OrderKind::Limit(Price::new(10_000).unwrap());
        Order::from_request(request(Side::Bid, kind, Lots::new(total).unwrap()))
    }

    fn limit(side: Side, price: i64) -> Order {
        let kind = OrderKind::Limit(Price::new(price).unwrap());
        Order::from_request(request(side, kind, Lots::new(1).unwrap())).unwrap()
    }

    fn protected(side: Side, protection: Option<i64>) -> Order {
        let kind = OrderKind::Market {
            protection: protection.map(|bound| Price::new(bound).unwrap()),
        };
        Order::from_request(request(side, kind, Lots::new(1).unwrap())).unwrap()
    }

    fn market(side: Side) -> Order {
        protected(side, None)
    }

    #[test]
    fn protected_market_buy_crosses_only_up_to_its_protection() {
        let buy = protected(Side::Bid, Some(105));

        assert!(buy.crosses(Price::new(100).unwrap()));
        assert!(buy.crosses(Price::new(105).unwrap()));
        assert!(!buy.crosses(Price::new(106).unwrap()));
    }

    #[test]
    fn protected_market_sell_crosses_only_down_to_its_protection() {
        let sell = protected(Side::Ask, Some(95));

        assert!(sell.crosses(Price::new(100).unwrap()));
        assert!(sell.crosses(Price::new(95).unwrap()));
        assert!(!sell.crosses(Price::new(94).unwrap()));
    }

    #[test]
    fn price_bound_is_the_limit_or_the_protection() {
        assert_eq!(
            limit(Side::Bid, 100).price_bound(),
            Some(Price::new(100).unwrap())
        );
        assert_eq!(
            protected(Side::Bid, Some(105)).price_bound(),
            Some(Price::new(105).unwrap())
        );
        assert_eq!(market(Side::Bid).price_bound(), None);
        assert_eq!(protected(Side::Bid, Some(105)).limit_price(), None);
    }

    #[test]
    fn limit_bid_crosses_asks_at_or_below_its_price() {
        let bid = limit(Side::Bid, 100);

        assert!(bid.crosses(Price::new(99).unwrap()));
        assert!(bid.crosses(Price::new(100).unwrap()));
        assert!(!bid.crosses(Price::new(101).unwrap()));
    }

    #[test]
    fn limit_ask_crosses_bids_at_or_above_its_price() {
        let ask = limit(Side::Ask, 100);

        assert!(ask.crosses(Price::new(101).unwrap()));
        assert!(ask.crosses(Price::new(100).unwrap()));
        assert!(!ask.crosses(Price::new(99).unwrap()));
    }

    #[test]
    fn market_crosses_any_price_on_both_sides() {
        for side in [Side::Bid, Side::Ask] {
            let order = market(side);
            for price in [1, 100, i64::MAX] {
                assert!(
                    order.crosses(Price::new(price).unwrap()),
                    "{side:?} @ {price}"
                );
            }
        }
    }

    #[test]
    fn market_order_has_no_limit_price() {
        let order = market(Side::Bid);

        assert_eq!(order.kind(), OrderKind::Market { protection: None });
        assert_eq!(order.limit_price(), None);
        assert_eq!(
            limit(Side::Bid, 100).limit_price(),
            Some(Price::new(100).unwrap())
        );
    }

    #[test]
    fn market_order_rejects_zero_lots() {
        let kind = OrderKind::Market { protection: None };
        let result = Order::from_request(request(Side::Bid, kind, Lots::ZERO));

        assert_eq!(result.unwrap_err(), DomainError::EmptyOrder);
    }

    const TESTE: AssetId = AssetId::new(1);
    const BRL: AssetId = AssetId::new(2);

    fn instrument(
        base: AssetId,
        quote: AssetId,
        tick: i64,
        lot_size: i64,
    ) -> Result<Instrument, DomainError> {
        Instrument::new(
            InstrumentId::new(1),
            base,
            quote,
            Price::new(tick).unwrap(),
            Amount::new(lot_size).unwrap(),
        )
    }

    #[test]
    fn amount_checked_arithmetic_refuses_overflow_and_negatives() {
        let ten = Amount::new(10).unwrap();
        let max = Amount::new(i64::MAX).unwrap();

        assert_eq!(ten.checked_add(ten), Amount::new(20).ok());
        assert_eq!(max.checked_add(ten), None);
        assert_eq!(ten.checked_sub(ten), Some(Amount::ZERO));
        assert_eq!(ten.checked_sub(Amount::new(11).unwrap()), None);
    }

    #[test]
    fn multiplications_by_lots_refuse_overflow() {
        let lots = Lots::new(3).unwrap();

        assert_eq!(Price::new(100).unwrap().times(lots), Amount::new(300).ok());
        assert_eq!(Amount::new(7).unwrap().times(lots), Amount::new(21).ok());
        assert_eq!(Price::new(i64::MAX).unwrap().times(lots), None);
        assert_eq!(Amount::new(i64::MAX).unwrap().times(lots), None);
    }

    fn bps(value: u32) -> Bps {
        Bps::new(value).unwrap()
    }

    #[test]
    fn bps_rejects_more_than_full() {
        assert_eq!(Bps::new(10_001), Err(DomainError::BpsAboveFull(10_001)));
        assert_eq!(Bps::new(10_000).map(Bps::get), Ok(10_000));
    }

    #[test]
    fn fee_is_exact_when_it_divides() {
        assert_eq!(
            bps(50).fee_on(Amount::new(200).unwrap()),
            Amount::new(1).unwrap()
        );
        assert_eq!(
            bps(100).fee_on(Amount::new(10_000).unwrap()),
            Amount::new(100).unwrap()
        );
    }

    #[test]
    fn fee_rounds_up_any_fraction() {
        assert_eq!(
            bps(10).fee_on(Amount::new(1).unwrap()),
            Amount::new(1).unwrap()
        );
        assert_eq!(
            bps(50).fee_on(Amount::new(201).unwrap()),
            Amount::new(2).unwrap()
        );
    }

    #[test]
    fn fee_is_zero_only_for_zero_rate_or_zero_value() {
        assert_eq!(bps(0).fee_on(Amount::new(1_000).unwrap()), Amount::ZERO);
        assert_eq!(bps(50).fee_on(Amount::ZERO), Amount::ZERO);
    }

    #[test]
    fn full_rate_fee_is_the_whole_value_even_at_the_limit() {
        let max = Amount::new(i64::MAX).unwrap();

        assert_eq!(bps(10_000).fee_on(max), max);
        assert!(bps(9_999).fee_on(max) <= max);
    }

    #[test]
    fn amount_rejects_negative_and_accepts_zero() {
        assert_eq!(Amount::new(-1), Err(DomainError::NegativeAmount(-1)));
        assert_eq!(Amount::new(0), Ok(Amount::ZERO));
    }

    #[test]
    fn instrument_keeps_its_specification() {
        let spec = instrument(TESTE, BRL, 10, 100).unwrap();

        assert_eq!(spec.id(), InstrumentId::new(1));
        assert_eq!(spec.base(), TESTE);
        assert_eq!(spec.quote(), BRL);
        assert_eq!(spec.tick(), Price::new(10).unwrap());
        assert_eq!(spec.lot_size(), Amount::new(100).unwrap());
    }

    #[test]
    fn instrument_rejects_same_asset_on_both_sides() {
        assert_eq!(
            instrument(BRL, BRL, 10, 100),
            Err(DomainError::SameAsset(BRL))
        );
    }

    #[test]
    fn instrument_rejects_zero_lot_size() {
        assert_eq!(
            instrument(TESTE, BRL, 10, 0),
            Err(DomainError::NonPositiveLotSize)
        );
    }

    #[test]
    fn same_asset_is_checked_before_lot_size() {
        assert_eq!(
            instrument(BRL, BRL, 10, 0),
            Err(DomainError::SameAsset(BRL))
        );
    }

    #[test]
    fn instrument_accepts_only_multiples_of_tick() {
        let spec = instrument(TESTE, BRL, 10, 100).unwrap();

        assert!(spec.accepts_price(Price::new(10).unwrap()));
        assert!(spec.accepts_price(Price::new(10_000).unwrap()));
        assert!(!spec.accepts_price(Price::new(10_005).unwrap()));
        assert!(!spec.accepts_price(Price::new(5).unwrap()));
    }

    #[test]
    fn tick_of_one_accepts_every_price() {
        let spec = instrument(TESTE, BRL, 1, 1).unwrap();

        for price in [1, 7, 10_001, i64::MAX] {
            assert!(spec.accepts_price(Price::new(price).unwrap()), "{price}");
        }
    }

    #[test]
    fn order_keeps_the_requested_time_in_force() {
        let kind = OrderKind::Limit(Price::new(10_000).unwrap());
        let fok = OrderRequest {
            time_in_force: TimeInForce::Fok,
            ..request(Side::Bid, kind, Lots::new(5).unwrap())
        };

        let order = Order::from_request(fok).unwrap();

        assert_eq!(order.time_in_force(), TimeInForce::Fok);
        assert_eq!(
            order.with_time_in_force(TimeInForce::Ioc).time_in_force(),
            TimeInForce::Ioc
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
