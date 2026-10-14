use exchange_core::domain::{
    AccountId, Amount, AssetId, Bps, Instrument, InstrumentId, Lots, Price,
};
use exchange_core::engine::{Engine, FeeSchedule, ListingConfig, RiskLimits};

use crate::market_data::MarketData;

pub const INSTRUMENTS: [(&str, InstrumentId); 1] = [("TESTE/BRL", InstrumentId::new(1))];
pub const HOUSE: AccountId = AccountId::new(0);
pub const TESTE: AssetId = AssetId::new(1);
pub const BRL: AssetId = AssetId::new(2);
pub const ASSETS: [(&str, AssetId); 2] = [("TESTE", TESTE), ("BRL", BRL)];

// TESTE/BRL: preço em centavos por lot, tick de 10 centavos, 1 lot = 100 unidades de TESTE.
// Risco: no máximo 1.000 lots por ordem e colar de 10% sobre o melhor preço oposto.
fn listings() -> [ListingConfig; 1] {
    let tick = Price::new(10).expect("tick fixo é positivo");
    let lot_size = Amount::new(100).expect("lot fixo é positivo");
    let instrument = Instrument::new(INSTRUMENTS[0].1, TESTE, BRL, tick, lot_size)
        .expect("especificação fixa é válida");
    let risk = RiskLimits {
        max_lots: Lots::new(1_000).expect("limite fixo é válido"),
        collar: Bps::new(1_000).expect("10% é menos que 100%"),
    };
    [ListingConfig { instrument, risk }]
}

/// # Panics
/// Nunca: a configuração é fixa e válida, e os `expect` só registram isso.
pub fn new_engine() -> Engine {
    // Taxa de taker de 10 bps (0,1%) para a casa: pequena, mas exercita o caminho (decisão 7).
    let fees = FeeSchedule {
        house: HOUSE,
        taker_bps: Bps::new(10).expect("10 bps é menos que 100%"),
    };
    Engine::new(listings(), fees).expect("instrumentos fixos não se repetem")
}

/// O feed de market data com todos os instrumentos do mercado registrados, vazios.
pub fn new_market_data() -> MarketData {
    MarketData::new(INSTRUMENTS.iter().map(|&(_, instrument)| instrument))
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
