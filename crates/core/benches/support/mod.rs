use exchange_core::domain::{
    AccountId, Amount, AssetId, Bps, Instrument, InstrumentId, Lots, OrderId, OrderKind,
    OrderRequest, Price, Seq, Side, TimeInForce,
};
use exchange_core::engine::{Command, Engine, Event, FeeSchedule, ListingConfig, RiskLimits};

pub const SEED: u64 = 0x5EED_CAFE;
pub const INSTRUMENT: InstrumentId = InstrumentId::new(1);
pub const ACCOUNTS: u64 = 100;
pub const MID: i64 = 10_000;
pub const TICK: i64 = 10;

const BASE: AssetId = AssetId::new(1);
const QUOTE: AssetId = AssetId::new(2);
const HOUSE: AccountId = AccountId::new(0);

/// Proporção do fluxo, em pontos percentuais: quanto cruza o spread e quanto é
/// cancelamento. O resto é ordem limite que descansa sem cruzar.
#[derive(Clone, Copy, Debug)]
pub struct Mix {
    pub crossing_pct: u64,
    pub cancel_pct: u64,
}

pub const SCENARIOS: [(&str, Mix); 3] = [
    (
        "resting",
        Mix {
            crossing_pct: 0,
            cancel_pct: 0,
        },
    ),
    (
        "crossing",
        Mix {
            crossing_pct: 50,
            cancel_pct: 0,
        },
    ),
    (
        "with_cancels",
        Mix {
            crossing_pct: 40,
            cancel_pct: 20,
        },
    ),
];

/// xorshift64: determinístico pela semente, sem dependência. Carga de benchmark não
/// precisa de aleatoriedade boa, precisa ser a mesma em toda rodada.
pub struct Xorshift(u64);

impl Xorshift {
    pub fn new(seed: u64) -> Self {
        Self(seed.max(1))
    }

    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    /// Em `0..bound`; o viés do módulo não importa para gerar carga.
    pub fn below(&mut self, bound: u64) -> u64 {
        self.next_u64() % bound
    }

    /// Um índice válido de uma lista com `len` itens.
    pub fn pick(&mut self, len: usize) -> usize {
        let len = u64::try_from(len).expect("usize cabe em u64");
        usize::try_from(self.below(len)).expect("menor que len, que veio de usize")
    }

    /// Em `low..=high`.
    pub fn between(&mut self, low: i64, high: i64) -> i64 {
        let span = u64::try_from(high - low + 1).expect("intervalo não vazio");
        low + i64::try_from(self.below(span)).expect("menor que span, que veio de i64")
    }
}

/// Mercado de um instrumento com risco e taxa de produção (10% de colar, 10 bps) e
/// [`ACCOUNTS`] contas com saldo de sobra: o benchmark mede matching, não rejeição.
pub fn funded_engine() -> Engine {
    let instrument = Instrument::new(
        INSTRUMENT,
        BASE,
        QUOTE,
        Price::new(TICK).expect("tick positivo"),
        Amount::new(100).expect("lot positivo"),
    )
    .expect("especificação válida");
    let listing = ListingConfig {
        instrument,
        risk: RiskLimits {
            max_lots: Lots::new(1_000).expect("limite positivo"),
            collar: Bps::new(1_000).expect("10% é menos que 100%"),
        },
    };
    let fees = FeeSchedule {
        house: HOUSE,
        taker_bps: Bps::new(10).expect("10 bps é menos que 100%"),
    };
    let mut engine = Engine::new([listing], fees).expect("instrumento único");
    for account in 1..=ACCOUNTS {
        for (asset, amount) in [(BASE, 1_000_000_000_000), (QUOTE, 1_000_000_000_000_000)] {
            engine.apply(Command::Deposit {
                account: AccountId::new(account),
                asset,
                amount: Amount::new(amount).expect("depósito positivo"),
            });
        }
    }
    engine
}

/// Ordem limite GTC; `id` também é a sequência, que só precisa crescer (decisão 8).
pub fn limit(id: u64, account: u64, side: Side, price: i64, lots: i64) -> Command {
    Command::Submit(OrderRequest {
        id: OrderId::new(id),
        account: AccountId::new(account),
        instrument: INSTRUMENT,
        side,
        kind: OrderKind::Limit(Price::new(price).expect("preço positivo")),
        total: Lots::new(lots).expect("lots positivos"),
        seq: Seq::new(id),
        time_in_force: TimeInForce::Gtc,
    })
}

pub fn cancel(account: u64, order: u64) -> Command {
    Command::Cancel {
        instrument: INSTRUMENT,
        account: AccountId::new(account),
        order: OrderId::new(order),
    }
}

/// `count` comandos determinísticos pela semente, na proporção de `mix`.
///
/// Requisitos: ids crescentes e únicos; preços múltiplos de [`TICK`] e dentro de 10% de
/// [`MID`] (o colar); só cancelar ordem que ainda pode estar no livro.
pub fn order_flow(seed: u64, mix: Mix, count: usize) -> Vec<Command> {
    let mut rng = Xorshift::new(seed);
    let mut commands = Vec::with_capacity(count);
    let mut resting: Vec<(u64, u64)> = Vec::new();
    let mut next_id = 1;
    while commands.len() < count {
        let roll = rng.below(100);
        let account = 1 + rng.below(ACCOUNTS);
        if roll < mix.cancel_pct && !resting.is_empty() {
            let index = rng.pick(resting.len());
            let (owner, id) = resting.swap_remove(index);
            commands.push(cancel(owner, id));
        } else {
            let side = if rng.below(2) == 0 {
                Side::Bid
            } else {
                Side::Ask
            };
            let crossing = roll < mix.cancel_pct + mix.crossing_pct;
            let ticks = if crossing {
                rng.between(1, 3)
            } else {
                rng.between(1, 100)
            };
            let direction = match (side, crossing) {
                (Side::Bid, false) | (Side::Ask, true) => -1,
                (Side::Ask, false) | (Side::Bid, true) => 1,
            };
            let price = MID + direction * ticks * TICK;
            let lots = rng.between(1, 10);
            commands.push(limit(next_id, account, side, price, lots));
            if !crossing {
                resting.push((account, next_id));
            }
            next_id += 1;
        }
    }
    commands
}

/// Roda o fluxo uma vez e falha se mais de 1% virar rejeição: um gerador com bug
/// mediria o caminho barato da rejeição e daria um número bonito e falso.
///
/// # Panics
/// Se o fluxo tiver rejeição demais; é o objetivo.
pub fn check_flow(flow: &[Command]) {
    let mut engine = funded_engine();
    let rejected = flow
        .iter()
        .flat_map(|&command| engine.apply(command))
        .filter(|event| matches!(event, Event::Rejected { .. }))
        .count();
    assert!(
        rejected * 100 <= flow.len(),
        "{rejected} de {} comandos rejeitados; o gerador está errado",
        flow.len()
    );
}
