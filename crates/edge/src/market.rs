use exchange_core::domain::{
    AccountId, Amount, AssetId, Bps, Instrument, InstrumentId, Lots, Price,
};
use exchange_core::engine::{
    Engine, EngineState, FeeSchedule, ListingConfig, RestoreError, RiskLimits,
};

use crate::market_data::MarketData;

pub const INSTRUMENTS: [(&str, InstrumentId); 2] = [
    ("TESTE/BRL", InstrumentId::new(1)),
    ("OUTRO/BRL", InstrumentId::new(2)),
];
pub const HOUSE: AccountId = AccountId::new(0);
pub const TESTE: AssetId = AssetId::new(1);
pub const BRL: AssetId = AssetId::new(2);
pub const OUTRO: AssetId = AssetId::new(3);
pub const ASSETS: [(&str, AssetId); 3] = [("TESTE", TESTE), ("BRL", BRL), ("OUTRO", OUTRO)];

// Preço em centavos por lot; colar sobre o melhor preço oposto.
// TESTE/BRL: tick de 10 centavos, 1 lot = 100 unidades de TESTE, até 1.000 lots, colar de 10%.
// OUTRO/BRL: tick de 5 centavos, 1 lot = 10 unidades de OUTRO, até 500 lots, colar de 5%.
// Especificações diferentes de propósito: usar a de um instrumento no outro quebra testes.
fn listings() -> [ListingConfig; 2] {
    [
        listing(INSTRUMENTS[0].1, TESTE, 10, 100, 1_000, 1_000),
        listing(INSTRUMENTS[1].1, OUTRO, 5, 10, 500, 500),
    ]
}

/// Um par `base/BRL`; os `expect` registram que os números fixos acima são válidos.
fn listing(
    id: InstrumentId,
    base: AssetId,
    tick: i64,
    lot_size: i64,
    max_lots: i64,
    collar_bps: u32,
) -> ListingConfig {
    let tick = Price::new(tick).expect("tick fixo é positivo");
    let lot_size = Amount::new(lot_size).expect("lot fixo é positivo");
    let instrument =
        Instrument::new(id, base, BRL, tick, lot_size).expect("especificação fixa é válida");
    let risk = RiskLimits {
        max_lots: Lots::new(max_lots).expect("limite fixo é válido"),
        collar: Bps::new(collar_bps).expect("colar fixo é menos que 100%"),
    };
    ListingConfig { instrument, risk }
}

// Taxa de taker de 10 bps (0,1%) para a casa: pequena, mas exercita o caminho (decisão 7).
fn fees() -> FeeSchedule {
    FeeSchedule {
        house: HOUSE,
        taker_bps: Bps::new(10).expect("10 bps é menos que 100%"),
    }
}

/// # Panics
/// Nunca: a configuração é fixa e válida, e os `expect` só registram isso.
pub fn new_engine() -> Engine {
    Engine::new(listings(), fees()).expect("instrumentos fixos não se repetem")
}

/// O engine de um snapshot sobre a configuração de agora (decisão 6).
///
/// # Errors
/// [`RestoreError`] se o estado não for válido nesta configuração.
pub fn restore_engine(state: &EngineState) -> Result<Engine, RestoreError> {
    Engine::restore(listings(), fees(), state)
}

/// O feed de market data com todos os instrumentos do mercado registrados, vazios.
pub fn new_market_data() -> MarketData {
    MarketData::new(INSTRUMENTS.iter().map(|&(_, instrument)| instrument))
}

/// O feed de todos os instrumentos do mercado com os livros de `state`.
pub fn restore_market_data(state: &EngineState) -> MarketData {
    MarketData::restore(INSTRUMENTS.iter().map(|&(_, instrument)| instrument), state)
}

pub fn asset_id(name: &str) -> Option<AssetId> {
    ASSETS
        .iter()
        .find(|(candidate, _)| *candidate == name)
        .map(|(_, asset)| *asset)
}

pub fn asset_name(asset: AssetId) -> &'static str {
    ASSETS
        .iter()
        .find(|(_, id)| *id == asset)
        .map_or("?", |(name, _)| name)
}

pub fn instrument_id(symbol: &str) -> Option<InstrumentId> {
    INSTRUMENTS
        .iter()
        .find(|(name, _)| *name == symbol)
        .map(|(_, instrument)| *instrument)
}

pub fn symbol(instrument: InstrumentId) -> &'static str {
    INSTRUMENTS
        .iter()
        .find(|(_, id)| *id == instrument)
        .map_or("?", |(name, _)| name)
}
