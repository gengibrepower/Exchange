use std::collections::HashMap;

use proptest::prelude::*;

use super::{Command, Engine, Event, FeeSchedule, ListingConfig, RiskLimits};
use crate::domain::{
    AccountId, Amount, AssetId, Bps, Fill, Instrument, InstrumentId, Lots, OrderId, OrderKind,
    OrderRequest, Price, Seq, Side, TimeInForce,
};

const TESTE_BRL: InstrumentId = InstrumentId::new(1);
const OUTRO_BRL: InstrumentId = InstrumentId::new(2);
const INSTRUMENTS: [InstrumentId; 2] = [TESTE_BRL, OUTRO_BRL];
const TESTE: AssetId = AssetId::new(1);
const BRL: AssetId = AssetId::new(2);
const OUTRO: AssetId = AssetId::new(3);
const ACCOUNTS: [AccountId; 2] = [AccountId::new(1), AccountId::new(2)];
const HOUSE: AccountId = AccountId::new(9);
const ASSETS: [AssetId; 3] = [TESTE, OUTRO, BRL];

// Dois pares que dividem a quote e têm lots diferentes: um erro de "lot do instrumento
// errado" ou de reserva vazando entre livros aparece nos saldos.
fn specs() -> [Instrument; 2] {
    let tick = Price::new(1).expect("tick positivo");
    let lot = |size| Amount::new(size).expect("lot positivo");
    [
        Instrument::new(TESTE_BRL, TESTE, BRL, tick, lot(3)).expect("especificação válida"),
        Instrument::new(OUTRO_BRL, OUTRO, BRL, tick, lot(7)).expect("especificação válida"),
    ]
}

fn spec() -> Instrument {
    specs()[0]
}

#[derive(Clone, Debug)]
enum Op {
    Deposit(usize, AssetId, i64),
    Submit {
        instrument: usize,
        account: usize,
        side: Side,
        price: Option<i64>,
        protection: Option<i64>,
        /// Com gatilho, é um stop: limite em `price`, ou a mercado sem ele.
        trigger: Option<i64>,
        lots: i64,
        tif: TimeInForce,
    },
    Cancel {
        instrument: usize,
        account: usize,
        target: prop::sample::Index,
    },
}

/// Operações sobre os `instruments` primeiros instrumentos de [`INSTRUMENTS`].
fn op(instruments: usize) -> impl Strategy<Value = Op> {
    let side = prop_oneof![Just(Side::Bid), Just(Side::Ask)];
    let tif = prop_oneof![
        3 => Just(TimeInForce::Gtc),
        1 => Just(TimeInForce::Ioc),
        1 => Just(TimeInForce::Fok),
    ];
    let price = prop_oneof![4 => (95..=105_i64).prop_map(Some), 1 => Just(None)];
    let protection = prop_oneof![Just(None), (95..=105_i64).prop_map(Some)];
    let trigger = prop_oneof![3 => Just(None), 1 => (95..=105_i64).prop_map(Some)];
    prop_oneof![
        2 => (0..2_usize, prop::sample::select(ASSETS.to_vec()), 0..=600_i64)
            .prop_map(|(account, asset, value)| Op::Deposit(account, asset, value)),
        6 => (0..instruments, 0..2_usize, side, price, protection, trigger, 1..=10_i64, tif)
            .prop_map(|(instrument, account, side, price, protection, trigger, lots, tif)| {
                Op::Submit {
                    instrument,
                    account,
                    side,
                    price,
                    protection,
                    trigger,
                    lots,
                    tif,
                }
            }),
        2 => (0..instruments, 0..2_usize, any::<prop::sample::Index>())
            .prop_map(|(instrument, account, target)| Op::Cancel { instrument, account, target }),
    ]
}

fn command(op: &Op, next_id: &mut u64) -> Command {
    match op {
        Op::Deposit(account, asset, value) => Command::Deposit {
            account: ACCOUNTS[*account],
            asset: *asset,
            amount: Amount::new(*value).expect("faixa não negativa"),
        },
        Op::Submit {
            instrument,
            account,
            side,
            price,
            protection,
            trigger,
            lots,
            tif,
        } => {
            let instrument = INSTRUMENTS[*instrument];
            let id = *next_id;
            *next_id += 1;
            let lots = Lots::new(*lots).expect("faixa positiva");
            let price_of = |value: i64| Price::new(value).expect("faixa positiva");
            let kind = match (trigger, price) {
                (Some(trigger), limit) => OrderKind::Stop {
                    trigger: price_of(*trigger),
                    limit: limit.map(price_of),
                },
                (None, Some(price)) => OrderKind::Limit(price_of(*price)),
                (None, None) => OrderKind::Market {
                    protection: protection.map(price_of),
                },
            };
            Command::Submit(OrderRequest {
                id: OrderId::new(id),
                account: ACCOUNTS[*account],
                instrument,
                side: *side,
                kind,
                total: lots,
                seq: Seq::new(id),
                time_in_force: *tif,
            })
        }
        Op::Cancel {
            instrument,
            account,
            target,
        } => Command::Cancel {
            instrument: INSTRUMENTS[*instrument],
            account: ACCOUNTS[*account],
            order: OrderId::new(1 + target.index(usize::try_from(*next_id).unwrap()) as u64),
        },
    }
}

/// Oráculo de saldos: recalcula, só a partir dos eventos e de quem é dono de cada ordem,
/// o total (disponível + reservado) que cada conta deveria ter de cada ativo. A taxa é
/// recalculada aqui com a fórmula própria, sem `fee_on` e sem confiar no valor do evento.
/// Pega o que a conservação agregada não pega: valor certo na conta errada.
struct BalanceOracle {
    totals: HashMap<(AccountId, AssetId), i128>,
    owners: HashMap<(InstrumentId, OrderId), (AccountId, Side)>,
    house: AccountId,
    taker_bps: u32,
}

impl BalanceOracle {
    fn new(house: AccountId, taker_bps: u32) -> Self {
        Self {
            totals: HashMap::new(),
            owners: HashMap::new(),
            house,
            taker_bps,
        }
    }

    fn record_owner(
        &mut self,
        instrument: InstrumentId,
        order: OrderId,
        account: AccountId,
        side: Side,
    ) {
        self.owners.insert((instrument, order), (account, side));
    }

    fn owner(&self, instrument: InstrumentId, order: OrderId) -> (AccountId, Side) {
        self.owners[&(instrument, order)]
    }

    fn credit(&mut self, account: AccountId, asset: AssetId, delta: i128) {
        *self.totals.entry((account, asset)).or_default() += delta;
    }

    fn deposit(&mut self, account: AccountId, asset: AssetId, amount: Amount) {
        self.credit(account, asset, i128::from(amount.get()));
    }

    /// Seção 4.2 do lado de quem confere: o que cada parte de um fill ganha e perde. O
    /// lado do taker vem da própria ordem, não do comando: num stop disparado, o taker não
    /// é quem mandou o comando.
    fn apply_fill(&mut self, spec: &Instrument, fill: &Fill) {
        let instrument = spec.id();
        let (taker, taker_side) = self.owner(instrument, fill.taker);
        let (maker, _) = self.owner(instrument, fill.maker);
        let (buyer, seller) = match taker_side {
            Side::Bid => (taker, maker),
            Side::Ask => (maker, taker),
        };

        let lots = i128::from(fill.lots.get());
        let base = lots * i128::from(spec.lot_size().get());
        let value = lots * i128::from(fill.price.get());
        let fee = (value * i128::from(self.taker_bps) + 9_999) / 10_000;
        let (buyer_fee, seller_fee) = match taker_side {
            Side::Bid => (fee, 0),
            Side::Ask => (0, fee),
        };

        self.credit(buyer, spec.base(), base);
        self.credit(seller, spec.base(), -base);
        self.credit(buyer, spec.quote(), -value - buyer_fee);
        self.credit(seller, spec.quote(), value - seller_fee);
        self.credit(self.house, spec.quote(), fee);
    }

    fn expected(&self, account: AccountId, asset: AssetId) -> i128 {
        self.totals
            .get(&(account, asset))
            .copied()
            .unwrap_or_default()
    }
}

// Borda do colar recalculada aqui, em i128 e sem a função do engine, para o teste não
// confirmar a si mesmo.
fn collar_edge_reference(reference: Price, collar_bps: u32, direction: i128) -> i128 {
    let reference = i128::from(reference.get());
    let band = (reference * i128::from(collar_bps) + 9_999) / 10_000;
    (reference + direction * band).clamp(1, i128::from(i64::MAX))
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(2_000))]

    // Seção 3.5 pelo Engine inteiro: conservação de valor (soma de cada ativo, contando a
    // conta da casa, = depósitos) e reserva consistente (reservado de cada conta = soma
    // das reservas das suas ordens abertas; reserva existe exatamente para as ordens que
    // estão no livro). E seção 4.1: nenhum fill sai do colar medido antes da ordem.
    #[test]
    fn value_is_conserved_and_reservations_are_consistent(
        taker_bps in prop_oneof![Just(0_u32), Just(1), Just(10), Just(50), Just(9_999), Just(10_000)],
        collar_bps in prop_oneof![Just(0_u32), Just(500), Just(1_000), Just(10_000)],
        max_lots in prop_oneof![Just(3_i64), Just(10)],
        ops in prop::collection::vec(op(1), 1..=80)
    ) {
        let fees = FeeSchedule { house: HOUSE, taker_bps: Bps::new(taker_bps).expect("até 100%") };
        let risk = RiskLimits {
            max_lots: Lots::new(max_lots).expect("positivo"),
            collar: Bps::new(collar_bps).expect("até 100%"),
        };
        let listing = ListingConfig { instrument: spec(), risk };
        let mut engine = Engine::new([listing], fees).expect("config válida");
        let mut next_id = 1;
        let mut armed: HashMap<OrderId, (Side, Price)> = HashMap::new();
        let mut last_trade: Option<Price> = None;
        for op in ops {
            let submitted = OrderId::new(next_id);
            let book = engine.book(TESTE_BRL).expect("instrumento registrado");
            let edge = match &op {
                Op::Submit { side: Side::Bid, .. } => book
                    .best_ask()
                    .map(|reference| (Side::Bid, collar_edge_reference(reference, collar_bps, 1))),
                Op::Submit { side: Side::Ask, .. } => book
                    .best_bid()
                    .map(|reference| (Side::Ask, collar_edge_reference(reference, collar_bps, -1))),
                _ => None,
            };

            let events = engine.apply(command(&op, &mut next_id));

            // O colar medido antes do comando vale para a ordem que chegou; um stop disparado
            // depois dela é medido contra o livro do momento do disparo.
            if let Some((side, edge)) = edge {
                for event in &events {
                    if let Event::Fill { fill, .. } = event
                        && fill.taker == submitted
                    {
                        let inside = match side {
                            Side::Bid => i128::from(fill.price.get()) <= edge,
                            Side::Ask => i128::from(fill.price.get()) >= edge,
                        };
                        prop_assert!(inside, "fill {:?} fora do colar {} depois de {:?}", fill, edge, op);
                    }
                }
            }

            for asset in ASSETS {
                let total: i128 = ACCOUNTS
                    .iter()
                    .chain([&HOUSE])
                    .map(|account| engine.balance(*account, asset))
                    .map(|b| i128::from(b.available.get()) + i128::from(b.reserved.get()))
                    .sum();
                prop_assert_eq!(total, i128::from(engine.ledger.supply(asset).get()));
            }

            let mut reserved_by_orders: HashMap<(AccountId, AssetId), i64> = HashMap::new();
            for reservation in engine.reservations.values() {
                *reserved_by_orders
                    .entry((reservation.account, reservation.asset))
                    .or_default() += reservation.amount.get();
            }
            for account in ACCOUNTS {
                for asset in ASSETS {
                    prop_assert_eq!(
                        engine.balance(account, asset).reserved.get(),
                        reserved_by_orders.get(&(account, asset)).copied().unwrap_or_default(),
                        "reservado de {:?}/{:?} diverge das ordens abertas depois de {:?}",
                        account,
                        asset,
                        op
                    );
                }
            }

            // Um stop dispara exatamente quando o último negócio alcança o gatilho: nunca
            // antes (cada `Triggered` tem o gatilho alcançado pelo último negócio até ali) e
            // nunca depois (ao fim do comando, nenhum dormente tem o gatilho alcançado).
            let reached = |side, trigger, last: Option<Price>| match side {
                Side::Bid => last.is_some_and(|last| last >= trigger),
                Side::Ask => last.is_some_and(|last| last <= trigger),
            };
            for event in &events {
                match *event {
                    Event::Armed { order, side, trigger, .. } => {
                        prop_assert!(!reached(side, trigger, last_trade), "stop armado já alcançado: {:?}", op);
                        armed.insert(order, (side, trigger));
                    }
                    Event::Triggered { order, .. } => {
                        let (side, trigger) = armed.remove(&order).expect("só dispara stop armado");
                        prop_assert!(reached(side, trigger, last_trade), "stop {:?} disparou antes do gatilho", order);
                    }
                    Event::Cancelled { order, .. } => {
                        armed.remove(&order);
                    }
                    Event::Fill { fill, .. } => last_trade = Some(fill.price),
                    _ => {}
                }
            }
            for (order, &(side, trigger)) in &armed {
                prop_assert!(!reached(side, trigger, last_trade), "stop {:?} alcançado e não disparou", order);
            }

            let listing = &engine.listings[&TESTE_BRL];
            let mut resting: Vec<u64> = listing
                .book
                .resting_ids()
                .chain(listing.stops.ids())
                .map(OrderId::get)
                .collect();
            let mut reserved: Vec<u64> = engine.reservations.keys().map(|(_, id)| id.get()).collect();
            resting.sort_unstable();
            reserved.sort_unstable();
            prop_assert_eq!(resting, reserved, "reserva sem ordem no livro ou stop dormente, ou o contrário");
        }
    }

    #[test]
    fn balances_match_an_independent_oracle(
        taker_bps in prop_oneof![Just(0_u32), Just(1), Just(10), Just(50), Just(9_999), Just(10_000)],
        ops in prop::collection::vec(op(2), 1..=80)
    ) {
        let fees = FeeSchedule { house: HOUSE, taker_bps: Bps::new(taker_bps).expect("até 100%") };
        let risk = RiskLimits {
            max_lots: Lots::new(10).expect("positivo"),
            collar: Bps::new(10_000).expect("até 100%"),
        };
        let listings = specs().map(|instrument| ListingConfig { instrument, risk });
        let mut engine = Engine::new(listings, fees).expect("config válida");
        let mut oracle = BalanceOracle::new(HOUSE, taker_bps);
        let mut next_id = 1;
        for op in ops {
            if let Op::Submit { instrument, account, side, .. } = &op {
                let order = OrderId::new(next_id);
                oracle.record_owner(INSTRUMENTS[*instrument], order, ACCOUNTS[*account], *side);
            }

            let requester = match op {
                Op::Deposit(account, ..) | Op::Submit { account, .. } | Op::Cancel { account, .. } => {
                    ACCOUNTS[account]
                }
            };
            for event in engine.apply(command(&op, &mut next_id)) {
                match event {
                    Event::Deposited { account, asset, amount } => {
                        oracle.deposit(account, asset, amount);
                    }
                    Event::Fill { instrument, fill, taker_account, maker_account, .. } => {
                        prop_assert_eq!(taker_account, oracle.owner(instrument, fill.taker).0);
                        prop_assert_eq!(maker_account, oracle.owner(instrument, fill.maker).0);
                        let spec = engine.instrument(instrument).expect("instrumento registrado");
                        oracle.apply_fill(spec, &fill);
                    }
                    Event::Rejected { account, .. } => prop_assert_eq!(account, requester),
                    Event::Rested { instrument, order, account, .. }
                    | Event::Cancelled { instrument, order, account, .. }
                    | Event::Expired { instrument, order, account, .. }
                    | Event::Armed { instrument, order, account, .. }
                    | Event::Triggered { instrument, order, account } => {
                        prop_assert_eq!(account, oracle.owner(instrument, order).0);
                    }
                    Event::DepositRejected { .. } => {}
                }
            }

            for account in ACCOUNTS.into_iter().chain([HOUSE]) {
                for asset in ASSETS {
                    let balance = engine.balance(account, asset);
                    let actual = i128::from(balance.available.get()) + i128::from(balance.reserved.get());
                    prop_assert_eq!(
                        actual,
                        oracle.expected(account, asset),
                        "saldo de {:?}/{:?} diverge do oráculo depois de {:?}",
                        account,
                        asset,
                        op
                    );
                }
            }
        }
    }
}
