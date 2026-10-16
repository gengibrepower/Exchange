//! O estado do engine como dado (decisão 6): só o que não dá para reconstruir. O índice do
//! livro, as chaves dos stops, as reservas e a oferta de cada ativo são recalculados na
//! carga, e por isso concordam com o resto por construção; o que sobra para validar são as
//! invariantes da seção 3.5, que um snapshot vindo do disco não tem por que respeitar.

use std::collections::{HashMap, HashSet};
use std::fmt;

use serde::{Deserialize, Serialize};

use super::{
    ConfigError, Engine, FeeSchedule, Listing, ListingConfig, Reservation, reservation_per_lot,
    stop_bound,
};
use crate::domain::{
    AccountId, Amount, AssetId, InstrumentId, Lots, Order, OrderId, OrderKind, OrderRequest, Price,
    Seq, Side, TimeInForce,
};
use crate::ledger::{Balance, Ledger};

/// Uma ordem aberta: o pedido como ela está agora e quanto ainda falta.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct OpenOrder {
    pub request: OrderRequest,
    pub remaining: Lots,
}

#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct InstrumentState {
    pub instrument: InstrumentId,
    pub last_trade: Option<Price>,
    /// Do melhor preço para o pior e, dentro do preço, na ordem da fila: a fila não sai do
    /// `seq`, porque um stop disparado entra com a prioridade do disparo.
    pub bids: Vec<OpenOrder>,
    pub asks: Vec<OpenOrder>,
    /// Em ordem de id; a ordem de disparo sai do gatilho e do `seq`.
    pub stops: Vec<OpenOrder>,
}

/// Toda lista vai numa ordem fixa, e a carga recusa qualquer outra: um estado tem um jeito
/// só de ser escrito, e snapshots com os mesmos bytes são o mesmo estado.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct EngineState {
    /// Em ordem de id.
    pub instruments: Vec<InstrumentState>,
    /// Em ordem de conta e ativo.
    pub balances: Vec<(AccountId, AssetId, Balance)>,
}

#[derive(Debug)]
pub enum StateDecodeError {
    /// Bytes que não formam um estado: truncados, variante inexistente ou newtype inválido.
    Malformed(postcard::Error),
    TrailingBytes(usize),
}

impl fmt::Display for StateDecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Malformed(error) => write!(f, "estado malformado: {error}"),
            Self::TrailingBytes(count) => write!(f, "{count} bytes sobrando depois do estado"),
        }
    }
}

impl std::error::Error for StateDecodeError {}

impl EngineState {
    /// # Panics
    /// Nunca: o estado só tem inteiros, enums e listas, que o postcard sempre serializa.
    pub fn to_bytes(&self) -> Vec<u8> {
        postcard::to_allocvec(self).expect("estado só tem inteiros, enums e listas")
    }

    /// # Errors
    /// [`StateDecodeError`] se os bytes não forem exatamente um estado.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, StateDecodeError> {
        let (state, rest) =
            postcard::take_from_bytes::<Self>(bytes).map_err(StateDecodeError::Malformed)?;
        if !rest.is_empty() {
            return Err(StateDecodeError::TrailingBytes(rest.len()));
        }
        Ok(state)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RestoreError {
    Config(ConfigError),
    /// Lista fora da ordem fixa ou com chave repetida.
    NotCanonical,
    UnknownInstrument(InstrumentId),
    DuplicateOrderId(OrderId),
    DuplicateSeq(Seq),
    InvalidOrder {
        order: OrderId,
        problem: OrderProblem,
    },
    /// A melhor compra no preço da melhor venda ou acima: teriam negociado.
    CrossedBook(InstrumentId),
    /// A reserva que a ordem precisaria não cabe em `i64`.
    ValueOverflow(OrderId),
    /// O reservado no ledger não é a soma das reservas das ordens abertas (seção 3.5).
    ReservationMismatch {
        account: AccountId,
        asset: AssetId,
    },
    SupplyOverflow(AssetId),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum OrderProblem {
    /// Restante zero ou maior que o total; num stop, diferente do total.
    Remaining,
    /// Na lista de outro instrumento ou de outro lado.
    Misplaced,
    /// No livro só descansa limite GTC; entre os stops, só stop.
    WrongKind,
    OffTick,
    TooLarge,
    HouseAccount,
    /// O último negócio já alcançou o gatilho: o stop teria disparado.
    AlreadyTriggered,
}

impl Engine {
    /// O estado completo, na forma canônica de [`EngineState`].
    pub fn state(&self) -> EngineState {
        let open = |order: &Order| OpenOrder {
            request: order.request(),
            remaining: order.remaining(),
        };
        let mut instruments: Vec<InstrumentState> = self
            .listings
            .values()
            .map(|listing| {
                let mut stops: Vec<OpenOrder> = listing.stops.orders().map(open).collect();
                stops.sort_by_key(|stop| stop.request.id.get());
                InstrumentState {
                    instrument: listing.instrument.id(),
                    last_trade: listing.last_trade,
                    bids: listing
                        .book
                        .resting(Side::Bid)
                        .into_iter()
                        .map(open)
                        .collect(),
                    asks: listing
                        .book
                        .resting(Side::Ask)
                        .into_iter()
                        .map(open)
                        .collect(),
                    stops,
                }
            })
            .collect();
        instruments.sort_by_key(|state| state.instrument.get());
        EngineState {
            instruments,
            balances: self.ledger.balances(),
        }
    }

    /// O engine de `state` sobre a configuração de agora, que vem do código como no replay
    /// do log. Instrumento configurado que não aparece no estado começa vazio.
    ///
    /// # Errors
    /// [`RestoreError`] com a primeira regra que o estado quebra; nada é carregado pela
    /// metade.
    pub fn restore(
        listings: impl IntoIterator<Item = ListingConfig>,
        fees: FeeSchedule,
        state: &EngineState,
    ) -> Result<Self, RestoreError> {
        check_canonical(state)?;
        let mut engine = Self::new(listings, fees).map_err(RestoreError::Config)?;
        let mut ids = HashSet::new();
        let mut seqs = HashSet::new();
        let mut expected: HashMap<(AccountId, AssetId), Amount> = HashMap::new();
        for instrument in &state.instruments {
            let listing = engine
                .listings
                .get_mut(&instrument.instrument)
                .ok_or(RestoreError::UnknownInstrument(instrument.instrument))?;
            listing.last_trade = instrument.last_trade;
            let mut opened = Vec::new();
            for (side, list) in [(Side::Bid, &instrument.bids), (Side::Ask, &instrument.asks)] {
                let mut previous: Option<Price> = None;
                for open in list {
                    let (order, price) = open_order(listing, fees, open, side)?;
                    let worse_first = previous.is_some_and(|previous| match side {
                        Side::Bid => price > previous,
                        Side::Ask => price < previous,
                    });
                    if worse_first {
                        return Err(RestoreError::NotCanonical);
                    }
                    previous = Some(price);
                    opened.push((order.clone(), Some(price)));
                    listing.book.rest(order, price);
                }
            }
            if let (Some(bid), Some(ask)) = (listing.book.best_bid(), listing.book.best_ask())
                && bid >= ask
            {
                return Err(RestoreError::CrossedBook(instrument.instrument));
            }
            for open in &instrument.stops {
                let (order, trigger, limit) = open_stop(listing, fees, open)?;
                let bound = stop_bound(order.side(), trigger, limit, listing.risk);
                opened.push((order.clone(), bound));
                listing.stops.arm(order, trigger);
            }
            for (order, bound) in opened {
                if !ids.insert(order.id()) {
                    return Err(RestoreError::DuplicateOrderId(order.id()));
                }
                if !seqs.insert(order.seq().get()) {
                    return Err(RestoreError::DuplicateSeq(order.seq()));
                }
                let overflow = RestoreError::ValueOverflow(order.id());
                let (asset, per_lot) =
                    reservation_per_lot(order.side(), bound, &listing.instrument, fees.taker_bps)
                        .map_err(|_| overflow)?;
                let amount = per_lot.times(order.remaining()).ok_or(overflow)?;
                let total = expected.entry((order.account(), asset)).or_default();
                *total = total.checked_add(amount).ok_or(overflow)?;
                engine.reservations.insert(
                    (instrument.instrument, order.id()),
                    Reservation {
                        account: order.account(),
                        asset,
                        amount,
                        per_lot,
                    },
                );
            }
        }
        engine.ledger = Ledger::restore(state.balances.iter().copied())
            .map_err(RestoreError::SupplyOverflow)?;
        check_reservations(&engine.ledger, &state.balances, &expected)?;
        Ok(engine)
    }
}

fn check_canonical(state: &EngineState) -> Result<(), RestoreError> {
    let increasing = |keys: &[u64]| keys.windows(2).all(|pair| pair[0] < pair[1]);
    let instruments: Vec<u64> = state
        .instruments
        .iter()
        .map(|instrument| u64::from(instrument.instrument.get()))
        .collect();
    let balances: Vec<(u64, u64)> = state
        .balances
        .iter()
        .map(|&(account, asset, _)| (account.get(), u64::from(asset.get())))
        .collect();
    let canonical = increasing(&instruments)
        && balances.windows(2).all(|pair| pair[0] < pair[1])
        && state.instruments.iter().all(|instrument| {
            let stops: Vec<u64> = instrument
                .stops
                .iter()
                .map(|stop| stop.request.id.get())
                .collect();
            increasing(&stops)
        });
    if canonical {
        Ok(())
    } else {
        Err(RestoreError::NotCanonical)
    }
}

/// O que vale para toda ordem aberta: no lugar certo, com restante coerente, preços no tick,
/// dentro do tamanho máximo e de quem pode negociar.
fn checked(
    listing: &Listing,
    fees: FeeSchedule,
    open: &OpenOrder,
    side: Option<Side>,
) -> Result<Order, RestoreError> {
    let request = open.request;
    let problem = |problem| RestoreError::InvalidOrder {
        order: request.id,
        problem,
    };
    if request.instrument != listing.instrument.id()
        || side.is_some_and(|side| side != request.side)
    {
        return Err(problem(OrderProblem::Misplaced));
    }
    let order = Order::restore(request, open.remaining).ok_or(problem(OrderProblem::Remaining))?;
    if order
        .quoted_prices()
        .into_iter()
        .flatten()
        .any(|price| !listing.instrument.accepts_price(price))
    {
        return Err(problem(OrderProblem::OffTick));
    }
    if order.total() > listing.risk.max_lots {
        return Err(problem(OrderProblem::TooLarge));
    }
    if order.account() == fees.house {
        return Err(problem(OrderProblem::HouseAccount));
    }
    Ok(order)
}

/// Uma ordem do livro, com o preço em que descansa.
fn open_order(
    listing: &Listing,
    fees: FeeSchedule,
    open: &OpenOrder,
    side: Side,
) -> Result<(Order, Price), RestoreError> {
    let order = checked(listing, fees, open, Some(side))?;
    match (order.kind(), order.time_in_force()) {
        (OrderKind::Limit(price), TimeInForce::Gtc) => Ok((order, price)),
        _ => Err(invalid(&order, OrderProblem::WrongKind)),
    }
}

/// Um stop dormente, com o gatilho e o limite.
fn open_stop(
    listing: &Listing,
    fees: FeeSchedule,
    open: &OpenOrder,
) -> Result<(Order, Price, Option<Price>), RestoreError> {
    let order = checked(listing, fees, open, None)?;
    let OrderKind::Stop { trigger, limit } = order.kind() else {
        return Err(invalid(&order, OrderProblem::WrongKind));
    };
    if order.remaining() != order.total() {
        return Err(invalid(&order, OrderProblem::Remaining));
    }
    let reached = listing.last_trade.is_some_and(|last| match order.side() {
        Side::Bid => last >= trigger,
        Side::Ask => last <= trigger,
    });
    if reached {
        return Err(invalid(&order, OrderProblem::AlreadyTriggered));
    }
    Ok((order, trigger, limit))
}

fn invalid(order: &Order, problem: OrderProblem) -> RestoreError {
    RestoreError::InvalidOrder {
        order: order.id(),
        problem,
    }
}

/// Seção 3.5, reserva consistente, nos dois sentidos: todo reservado do ledger é coberto
/// pelas ordens abertas, e toda reserva de ordem aberta está no ledger.
fn check_reservations(
    ledger: &Ledger,
    balances: &[(AccountId, AssetId, Balance)],
    expected: &HashMap<(AccountId, AssetId), Amount>,
) -> Result<(), RestoreError> {
    let keys = balances
        .iter()
        .map(|&(account, asset, _)| (account, asset))
        .chain(expected.keys().copied());
    for (account, asset) in keys {
        let reserved = expected.get(&(account, asset)).copied().unwrap_or_default();
        if ledger.balance(account, asset).reserved != reserved {
            return Err(RestoreError::ReservationMismatch { account, asset });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{Bps, Instrument};
    use crate::engine::{Command, RiskLimits};

    const TESTE_BRL: InstrumentId = InstrumentId::new(1);
    const TESTE: AssetId = AssetId::new(1);
    const BRL: AssetId = AssetId::new(2);
    const ALICE: AccountId = AccountId::new(1);
    const BOB: AccountId = AccountId::new(2);
    const HOUSE: AccountId = AccountId::new(9);

    fn price(units: i64) -> Price {
        Price::new(units).unwrap()
    }

    fn lots(value: i64) -> Lots {
        Lots::new(value).unwrap()
    }

    fn listing(max_lots: i64) -> ListingConfig {
        let instrument =
            Instrument::new(TESTE_BRL, TESTE, BRL, price(10), Amount::new(1).unwrap()).unwrap();
        ListingConfig {
            instrument,
            risk: RiskLimits {
                max_lots: lots(max_lots),
                collar: Bps::new(10_000).unwrap(),
            },
        }
    }

    fn fees() -> FeeSchedule {
        FeeSchedule {
            house: HOUSE,
            taker_bps: Bps::new(10).unwrap(),
        }
    }

    fn submit(id: u64, account: AccountId, side: Side, kind: OrderKind, total: i64) -> Command {
        Command::Submit(OrderRequest {
            id: OrderId::new(id),
            account,
            instrument: TESTE_BRL,
            side,
            kind,
            total: lots(total),
            seq: Seq::new(id),
            time_in_force: TimeInForce::Gtc,
        })
    }

    /// Uma compra parcialmente executada no livro (o último negócio fica em 100), uma venda
    /// intacta e dois stops dormentes, um de cada lado.
    fn engine() -> Engine {
        let mut engine = Engine::new([listing(100)], fees()).unwrap();
        for account in [ALICE, BOB] {
            for asset in [TESTE, BRL] {
                engine.apply(Command::Deposit {
                    account,
                    asset,
                    amount: Amount::new(1_000_000).unwrap(),
                });
            }
        }
        let stop = |trigger, limit: Option<i64>| OrderKind::Stop {
            trigger: price(trigger),
            limit: limit.map(price),
        };
        for command in [
            submit(1, ALICE, Side::Bid, OrderKind::Limit(price(100)), 5),
            submit(2, BOB, Side::Ask, OrderKind::Limit(price(120)), 3),
            submit(3, BOB, Side::Ask, OrderKind::Limit(price(100)), 1),
            submit(4, ALICE, Side::Bid, stop(130, None), 1),
            submit(5, BOB, Side::Ask, stop(90, Some(80)), 2),
        ] {
            engine.apply(command);
        }
        engine
    }

    fn restore(state: &EngineState) -> Result<Engine, RestoreError> {
        Engine::restore([listing(100)], fees(), state)
    }

    fn broken(change: impl FnOnce(&mut EngineState)) -> Result<Engine, RestoreError> {
        let mut state = engine().state();
        change(&mut state);
        restore(&state)
    }

    fn invalid(order: u64, problem: OrderProblem) -> Result<Engine, RestoreError> {
        Err(RestoreError::InvalidOrder {
            order: OrderId::new(order),
            problem,
        })
    }

    #[test]
    fn the_fixture_covers_every_kind_of_open_order() {
        let state = engine().state();
        let instrument = &state.instruments[0];
        assert_eq!(instrument.last_trade, Some(price(100)));
        assert_eq!(instrument.bids[0].remaining, lots(4));
        assert_eq!(instrument.asks.len(), 1);
        assert_eq!(instrument.stops.len(), 2);
        assert_eq!(restore(&state), Ok(engine()));
    }

    // Os `HashMap` de dois engines iguais iteram em ordens diferentes; os bytes não mudam.
    #[test]
    fn the_same_state_is_always_the_same_bytes() {
        assert_eq!(engine().state().to_bytes(), engine().state().to_bytes());
    }

    #[test]
    fn bytes_that_are_not_exactly_a_state_are_refused() {
        let bytes = engine().state().to_bytes();
        assert!(matches!(
            EngineState::from_bytes(&bytes[..bytes.len() - 1]),
            Err(StateDecodeError::Malformed(_))
        ));
        let mut longer = bytes.clone();
        longer.push(0);
        assert!(matches!(
            EngineState::from_bytes(&longer),
            Err(StateDecodeError::TrailingBytes(1))
        ));
    }

    #[test]
    fn lists_out_of_their_fixed_order_are_refused() {
        let not_canonical = Err(RestoreError::NotCanonical);
        assert_eq!(broken(|state| state.balances.swap(0, 1)), not_canonical);
        // Repetido, o saldo contaria duas vezes na oferta e uma só no ledger.
        assert_eq!(
            broken(|state| state.balances.insert(1, state.balances[0])),
            not_canonical
        );
        assert_eq!(
            broken(|state| state.instruments[0].stops.swap(0, 1)),
            not_canonical
        );
        assert_eq!(
            broken(|state| {
                let instrument = &mut state.instruments[0];
                let mut worse = instrument.bids[0];
                worse.request.id = OrderId::new(9);
                worse.request.seq = Seq::new(9);
                worse.request.kind = OrderKind::Limit(price(90));
                instrument.bids.insert(0, worse);
            }),
            not_canonical
        );
    }

    #[test]
    fn duplicated_instrument_in_the_configuration_is_refused() {
        let config = [listing(100), listing(100)];
        assert_eq!(
            Engine::restore(config, fees(), &engine().state()),
            Err(RestoreError::Config(ConfigError::DuplicateInstrument(
                TESTE_BRL
            )))
        );
    }

    #[test]
    fn instrument_missing_from_the_configuration_is_refused() {
        let unknown = InstrumentId::new(7);
        assert_eq!(
            broken(|state| state.instruments[0].instrument = unknown),
            Err(RestoreError::UnknownInstrument(unknown))
        );
    }

    #[test]
    fn order_ids_and_seqs_are_unique() {
        assert_eq!(
            broken(|state| {
                let instrument = &mut state.instruments[0];
                instrument.asks[0].request.id = instrument.bids[0].request.id;
            }),
            Err(RestoreError::DuplicateOrderId(OrderId::new(1)))
        );
        assert_eq!(
            broken(|state| {
                let instrument = &mut state.instruments[0];
                instrument.asks[0].request.seq = instrument.bids[0].request.seq;
            }),
            Err(RestoreError::DuplicateSeq(Seq::new(1)))
        );
    }

    #[test]
    fn remaining_must_be_between_one_and_the_total() {
        assert_eq!(
            broken(|state| state.instruments[0].bids[0].remaining = Lots::ZERO),
            invalid(1, OrderProblem::Remaining)
        );
        assert_eq!(
            broken(|state| state.instruments[0].bids[0].remaining = lots(6)),
            invalid(1, OrderProblem::Remaining)
        );
        assert_eq!(
            broken(|state| state.instruments[0].stops[0].remaining = Lots::ZERO),
            invalid(4, OrderProblem::Remaining)
        );
    }

    // Um stop nunca executa parcialmente: ou dorme inteiro, ou já disparou e saiu da lista.
    #[test]
    fn partially_filled_stop_is_refused() {
        assert_eq!(
            broken(|state| {
                let stop = &mut state.instruments[0].stops[1];
                stop.remaining = lots(1);
            }),
            invalid(5, OrderProblem::Remaining)
        );
    }

    #[test]
    fn order_on_the_wrong_side_or_instrument_is_refused() {
        assert_eq!(
            broken(|state| state.instruments[0].bids[0].request.side = Side::Ask),
            invalid(1, OrderProblem::Misplaced)
        );
        assert_eq!(
            broken(|state| state.instruments[0].stops[0].request.instrument = InstrumentId::new(7)),
            invalid(4, OrderProblem::Misplaced)
        );
    }

    #[test]
    fn only_gtc_limits_rest_and_only_stops_sleep() {
        assert_eq!(
            broken(|state| state.instruments[0].bids[0].request.time_in_force = TimeInForce::Ioc),
            invalid(1, OrderProblem::WrongKind)
        );
        assert_eq!(
            broken(|state| {
                state.instruments[0].asks[0].request.kind = OrderKind::Market { protection: None };
            }),
            invalid(2, OrderProblem::WrongKind)
        );
        assert_eq!(
            broken(|state| {
                state.instruments[0].stops[0].request.kind = OrderKind::Limit(price(130));
            }),
            invalid(4, OrderProblem::WrongKind)
        );
    }

    #[test]
    fn prices_off_the_tick_are_refused() {
        assert_eq!(
            broken(|state| state.instruments[0].asks[0].request.kind = OrderKind::Limit(price(125))),
            invalid(2, OrderProblem::OffTick)
        );
        assert_eq!(
            broken(|state| {
                state.instruments[0].stops[1].request.kind = OrderKind::Stop {
                    trigger: price(90),
                    limit: Some(price(85)),
                };
            }),
            invalid(5, OrderProblem::OffTick)
        );
    }

    #[test]
    fn order_above_the_current_maximum_is_refused() {
        assert_eq!(
            Engine::restore([listing(4)], fees(), &engine().state()),
            invalid(1, OrderProblem::TooLarge)
        );
    }

    #[test]
    fn house_account_never_has_open_orders() {
        assert_eq!(
            broken(|state| state.instruments[0].asks[0].request.account = HOUSE),
            invalid(2, OrderProblem::HouseAccount)
        );
    }

    #[test]
    fn stop_that_the_last_trade_already_reached_is_refused() {
        assert_eq!(
            broken(|state| state.instruments[0].last_trade = Some(price(130))),
            invalid(4, OrderProblem::AlreadyTriggered)
        );
        assert_eq!(
            broken(|state| state.instruments[0].last_trade = Some(price(90))),
            invalid(5, OrderProblem::AlreadyTriggered)
        );
    }

    #[test]
    fn crossed_book_is_refused() {
        assert_eq!(
            broken(|state| state.instruments[0].asks[0].request.kind = OrderKind::Limit(price(100))),
            Err(RestoreError::CrossedBook(TESTE_BRL))
        );
    }

    // Preço que o domínio aceita mas cuja reserva não cabe em i64: tem que ser erro, não
    // estouro nem panic.
    #[test]
    fn reservation_that_does_not_fit_is_an_error() {
        let config = [listing(i64::MAX)];
        let mut state = engine().state();
        state.instruments[0].asks.clear();
        let bid = &mut state.instruments[0].bids[0];
        bid.request.kind = OrderKind::Limit(price(i64::MAX - 7));
        assert_eq!(
            Engine::restore(config, fees(), &state),
            Err(RestoreError::ValueOverflow(OrderId::new(1)))
        );
    }

    // Seção 3.5 nos dois sentidos: reservado a mais sem ordem que o explique, e ordem cuja
    // reserva sumiu do ledger.
    #[test]
    fn reserved_balance_must_match_the_open_orders() {
        let mismatch = Err(RestoreError::ReservationMismatch {
            account: ALICE,
            asset: BRL,
        });
        assert_eq!(
            broken(|state| {
                let (_, _, balance) = state
                    .balances
                    .iter_mut()
                    .find(|(owner, held, _)| (*owner, *held) == (ALICE, BRL))
                    .unwrap();
                balance.reserved = balance
                    .reserved
                    .checked_add(Amount::new(1).unwrap())
                    .unwrap();
            }),
            mismatch
        );
        assert_eq!(
            broken(|state| {
                state.instruments[0].stops.remove(0);
            }),
            mismatch
        );
        assert_eq!(
            broken(|state| state
                .balances
                .retain(|&(owner, asset, _)| (owner, asset) != (ALICE, BRL))),
            mismatch
        );
    }

    #[test]
    fn supply_that_does_not_fit_is_refused() {
        let most = Amount::new(i64::MAX).unwrap();
        assert_eq!(
            broken(|state| {
                for (_, asset, balance) in &mut state.balances {
                    if *asset == TESTE {
                        balance.available = most;
                    }
                }
            }),
            Err(RestoreError::SupplyOverflow(TESTE))
        );
    }
}
