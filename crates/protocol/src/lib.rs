//! Protocolo da borda, fase 1 (seção 4.4): uma mensagem JSON por linha. Os tipos daqui são
//! o contrato com o cliente e mudam devagar. Não dependem do núcleo: os números viajam crus,
//! e quem recebe decide se eles fazem sentido no domínio. Assim o núcleo pode ser refatorado
//! sem mudar o fio, e um cliente que usa esta crate não tem como chegar ao núcleo
//! (decisão 13).

#![forbid(unsafe_code)]

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    Buy,
    Sell,
}

/// O lado de um fill: o taker chegou e cruzou, o maker estava no livro.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Taker,
    Maker,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Feed {
    L1,
    L2,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tif {
    #[default]
    Gtc,
    Ioc,
    Fok,
}

/// Mensagem do cliente. `type` decide a variante e campo desconhecido é erro: nada é
/// adivinhado. Ordem limite e a mercado são tipos distintos para que um `price` esquecido
/// nunca transforme uma limite numa ordem a mercado.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ClientMessage {
    Login {
        account: u64,
    },
    Deposit {
        asset: String,
        amount: i64,
    },
    Limit {
        instrument: String,
        side: Side,
        price: i64,
        lots: i64,
        #[serde(default)]
        tif: Tif,
    },
    Market {
        instrument: String,
        side: Side,
        lots: i64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        protection: Option<i64>,
        #[serde(default)]
        tif: Tif,
    },
    /// Dormente até o último negócio chegar a `trigger`; aí entra como limite em `limit`
    /// ou, sem ele, a mercado. Tipo próprio, e não um campo em `limit`/`market`: um stop
    /// não tem proteção explícita, e o `deny_unknown_fields` recusa a mistura.
    Stop {
        instrument: String,
        side: Side,
        trigger: i64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        limit: Option<i64>,
        lots: i64,
        #[serde(default)]
        tif: Tif,
    },
    Cancel {
        instrument: String,
        order: u64,
    },
    /// Variante com chaves vazias, não unitária: só assim o `deny_unknown_fields` vale
    /// para ela, e `{"type":"balance","account":1}` é erro em vez de ter o campo ignorado.
    Balance {},
    /// Pública: vale antes do login. Assinar de novo o mesmo fluxo devolve um snapshot novo,
    /// que é como o cliente se recupera de um buraco na sequência.
    Subscribe {
        instrument: String,
        feed: Feed,
    },
    /// Pública, como o feed: a especificação de cada instrumento negociado, para o cliente
    /// não precisar repetir tick e tamanho de lot que só o servidor sabe.
    Instruments {},
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct Level {
    pub price: i64,
    pub lots: i64,
}

/// Preço em unidades-mínimas da quote por lot, sempre múltiplo de `tick`; um lot vale
/// `lot_size` unidades-mínimas da base.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct InstrumentSpec {
    pub symbol: String,
    pub base: String,
    pub quote: String,
    pub tick: i64,
    pub lot_size: i64,
    pub max_lots: i64,
}

#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct AssetBalance {
    pub asset: String,
    pub available: i64,
    pub reserved: i64,
}

/// Mensagem do servidor. `code` dos rejeitados é estável, para máquina; `message` é texto
/// para gente e pode mudar.
///
/// É o mesmo tipo dos dois lados: o servidor serializa, o cliente desserializa. Ao contrário
/// de [`ClientMessage`], campo desconhecido é aceito: o servidor é rigoroso com o que recebe,
/// o cliente é tolerante com o que lê, para que campos novos não quebrem clientes antigos.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    LoggedIn {
        account: u64,
    },
    Deposited {
        asset: String,
        amount: i64,
    },
    DepositRejected {
        asset: String,
        amount: i64,
    },
    /// Fala só da ordem de quem recebe: `fee` é o que ele pagou, zero para o maker.
    Fill {
        instrument: String,
        role: Role,
        order: u64,
        lots: i64,
        price: i64,
        fee: i64,
    },
    Rested {
        instrument: String,
        order: u64,
        lots: i64,
    },
    Cancelled {
        instrument: String,
        order: u64,
        lots: i64,
    },
    Expired {
        instrument: String,
        order: u64,
        lots: i64,
    },
    Rejected {
        order: u64,
        code: String,
        message: String,
    },
    /// Stop aceito, dormente até o último negócio chegar ao gatilho.
    Armed {
        instrument: String,
        order: u64,
        side: Side,
        trigger: i64,
        lots: i64,
    },
    /// O stop disparou; os eventos dele como ordem comum vêm em seguida.
    Triggered {
        instrument: String,
        order: u64,
    },
    Balances {
        balances: Vec<AssetBalance>,
    },
    Instruments {
        instruments: Vec<InstrumentSpec>,
    },
    Error {
        message: String,
    },
    Snapshot {
        instrument: String,
        feed: Feed,
        seq: u64,
        bids: Vec<Level>,
        asks: Vec<Level>,
    },
    /// Nível 2: quantidade absoluta no preço; zero quer dizer que o nível sumiu.
    Depth {
        instrument: String,
        feed: Feed,
        seq: u64,
        side: Side,
        price: i64,
        lots: i64,
    },
    /// Nível 1: o topo do livro, `null` no lado vazio.
    Top {
        instrument: String,
        feed: Feed,
        seq: u64,
        bid: Option<Level>,
        ask: Option<Level>,
    },
    Trade {
        instrument: String,
        feed: Feed,
        seq: u64,
        price: i64,
        lots: i64,
    },
}

impl ClientMessage {
    /// # Errors
    /// Se a linha não for exatamente uma [`ClientMessage`]: JSON quebrado, `type` ou campo
    /// desconhecido, campo faltando, número com casas decimais.
    pub fn from_line(line: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(line)
    }

    /// # Panics
    /// Nunca: a mensagem só tem números, textos e enums, que o `serde_json` sempre
    /// serializa.
    pub fn to_line(&self) -> String {
        line_of(self)
    }
}

impl ServerMessage {
    /// # Errors
    /// Se a linha não for JSON de uma [`ServerMessage`] conhecida.
    pub fn from_line(line: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(line)
    }

    /// # Panics
    /// Nunca: a mensagem só tem números, textos e enums, que o `serde_json` sempre
    /// serializa.
    pub fn to_line(&self) -> String {
        line_of(self)
    }
}

fn line_of(message: &impl Serialize) -> String {
    let mut line = serde_json::to_string(message).expect("mensagem só tem números e textos");
    line.push('\n');
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode(line: &str) -> Result<ClientMessage, String> {
        ClientMessage::from_line(line).map_err(|error| error.to_string())
    }

    fn malformed(line: &str) -> String {
        match decode(line) {
            Err(detail) => detail,
            Ok(message) => panic!("{line} deveria ser malformada, veio {message:?}"),
        }
    }

    #[test]
    fn decodes_every_client_message() {
        assert_eq!(
            decode(r#"{"type":"login","account":1}"#),
            Ok(ClientMessage::Login { account: 1 })
        );
        assert_eq!(
            decode(r#"{"type":"deposit","asset":"BRL","amount":500}"#),
            Ok(ClientMessage::Deposit {
                asset: "BRL".to_string(),
                amount: 500
            })
        );
        assert_eq!(
            decode(
                r#"{"type":"limit","instrument":"TESTE/BRL","side":"buy","price":10000,"lots":5,"tif":"ioc"}"#
            ),
            Ok(ClientMessage::Limit {
                instrument: "TESTE/BRL".to_string(),
                side: Side::Buy,
                price: 10_000,
                lots: 5,
                tif: Tif::Ioc
            })
        );
        assert_eq!(
            decode(
                r#"{"type":"market","instrument":"TESTE/BRL","side":"sell","lots":2,"protection":9500}"#
            ),
            Ok(ClientMessage::Market {
                instrument: "TESTE/BRL".to_string(),
                side: Side::Sell,
                lots: 2,
                protection: Some(9_500),
                tif: Tif::Gtc
            })
        );
        assert_eq!(
            decode(
                r#"{"type":"stop","instrument":"TESTE/BRL","side":"sell","trigger":9500,"limit":9400,"lots":5}"#
            ),
            Ok(ClientMessage::Stop {
                instrument: "TESTE/BRL".to_string(),
                side: Side::Sell,
                trigger: 9_500,
                limit: Some(9_400),
                lots: 5,
                tif: Tif::Gtc
            })
        );
        assert_eq!(
            decode(r#"{"type":"cancel","instrument":"TESTE/BRL","order":3}"#),
            Ok(ClientMessage::Cancel {
                instrument: "TESTE/BRL".to_string(),
                order: 3
            })
        );
        assert_eq!(
            decode(r#"{"type":"balance"}"#),
            Ok(ClientMessage::Balance {})
        );
        assert_eq!(
            decode(r#"{"type":"subscribe","instrument":"TESTE/BRL","feed":"l2"}"#),
            Ok(ClientMessage::Subscribe {
                instrument: "TESTE/BRL".to_string(),
                feed: Feed::L2
            })
        );
        assert_eq!(
            decode(r#"{"type":"instruments"}"#),
            Ok(ClientMessage::Instruments {})
        );
    }

    #[test]
    fn optional_fields_take_their_defaults() {
        assert_eq!(
            decode(r#"{"type":"market","instrument":"TESTE/BRL","side":"buy","lots":1}"#),
            Ok(ClientMessage::Market {
                instrument: "TESTE/BRL".to_string(),
                side: Side::Buy,
                lots: 1,
                protection: None,
                tif: Tif::Gtc
            })
        );
    }

    #[test]
    fn field_order_and_whitespace_do_not_matter() {
        assert_eq!(
            decode(r#"  { "account" : 1 , "type" : "login" }  "#),
            Ok(ClientMessage::Login { account: 1 })
        );
    }

    #[test]
    fn broken_json_is_malformed() {
        malformed(r#"{"type":"login","account":1"#);
        malformed("buy TESTE/BRL 10000 5");
        malformed("");
    }

    #[test]
    fn unknown_type_and_missing_type_are_malformed() {
        malformed(r#"{"type":"withdraw","asset":"BRL","amount":1}"#);
        malformed(r#"{"account":1}"#);
    }

    #[test]
    fn unknown_field_is_malformed_not_ignored() {
        let detail = malformed(
            r#"{"type":"limit","instrument":"TESTE/BRL","side":"buy","price":10000,"lot":5}"#,
        );
        assert!(detail.contains("lot"), "{detail}");
        malformed(r#"{"type":"balance","account":1}"#);
        malformed(r#"{"type":"instruments","symbol":"TESTE/BRL"}"#);
    }

    #[test]
    fn limit_without_price_and_market_with_price_are_malformed() {
        malformed(r#"{"type":"limit","instrument":"TESTE/BRL","side":"buy","lots":5}"#);
        malformed(
            r#"{"type":"market","instrument":"TESTE/BRL","side":"buy","price":10000,"lots":5}"#,
        );
    }

    #[test]
    fn stop_without_trigger_or_with_protection_is_malformed() {
        malformed(r#"{"type":"stop","instrument":"TESTE/BRL","side":"sell","lots":5}"#);
        malformed(
            r#"{"type":"stop","instrument":"TESTE/BRL","side":"sell","trigger":9500,"protection":9000,"lots":5}"#,
        );
    }

    #[test]
    fn numbers_are_integers() {
        malformed(
            r#"{"type":"limit","instrument":"TESTE/BRL","side":"buy","price":100.5,"lots":5}"#,
        );
        malformed(
            r#"{"type":"limit","instrument":"TESTE/BRL","side":"buy","price":"10000","lots":5}"#,
        );
        malformed(r#"{"type":"login","account":-1}"#);
    }

    #[test]
    fn side_tif_and_feed_use_the_wire_words() {
        malformed(
            r#"{"type":"limit","instrument":"TESTE/BRL","side":"bid","price":10000,"lots":5}"#,
        );
        malformed(
            r#"{"type":"limit","instrument":"TESTE/BRL","side":"buy","price":10000,"lots":5,"tif":"GTC"}"#,
        );
        malformed(r#"{"type":"subscribe","instrument":"TESTE/BRL","feed":"l3"}"#);
        malformed(r#"{"type":"subscribe","instrument":"TESTE/BRL"}"#);
    }

    #[test]
    fn client_messages_serialize_without_absent_options() {
        let market = ClientMessage::Market {
            instrument: "TESTE/BRL".to_string(),
            side: Side::Buy,
            lots: 2,
            protection: None,
            tif: Tif::Gtc,
        };
        assert_eq!(
            market.to_line(),
            "{\"type\":\"market\",\"instrument\":\"TESTE/BRL\",\"side\":\"buy\",\"lots\":2,\"tif\":\"gtc\"}\n"
        );
        assert_eq!(
            ClientMessage::Balance {}.to_line(),
            "{\"type\":\"balance\"}\n"
        );
    }

    #[test]
    fn client_messages_survive_a_round_trip() {
        let messages = [
            ClientMessage::Login { account: 1 },
            ClientMessage::Limit {
                instrument: "OUTRO/BRL".to_string(),
                side: Side::Sell,
                price: 505,
                lots: 3,
                tif: Tif::Fok,
            },
            ClientMessage::Stop {
                instrument: "TESTE/BRL".to_string(),
                side: Side::Buy,
                trigger: 10_100,
                limit: None,
                lots: 1,
                tif: Tif::Ioc,
            },
            ClientMessage::Balance {},
        ];
        for message in messages {
            let line = message.to_line();
            assert_eq!(ClientMessage::from_line(&line).unwrap(), message, "{line}");
        }
    }

    #[test]
    fn session_messages_serialize_as_documented() {
        assert_eq!(
            ServerMessage::LoggedIn { account: 7 }.to_line(),
            "{\"type\":\"logged_in\",\"account\":7}\n"
        );
        let balances = ServerMessage::Balances {
            balances: vec![AssetBalance {
                asset: "BRL".to_string(),
                available: 10,
                reserved: 5,
            }],
        };
        assert_eq!(
            balances.to_line(),
            "{\"type\":\"balances\",\"balances\":[{\"asset\":\"BRL\",\"available\":10,\"reserved\":5}]}\n"
        );
        assert_eq!(
            ServerMessage::Error {
                message: "x".to_string()
            }
            .to_line(),
            "{\"type\":\"error\",\"message\":\"x\"}\n"
        );
    }

    #[test]
    fn instruments_serialize_as_documented() {
        let instruments = ServerMessage::Instruments {
            instruments: vec![InstrumentSpec {
                symbol: "TESTE/BRL".to_string(),
                base: "TESTE".to_string(),
                quote: "BRL".to_string(),
                tick: 10,
                lot_size: 100,
                max_lots: 1_000,
            }],
        };
        let line = instruments.to_line();

        assert_eq!(
            line,
            "{\"type\":\"instruments\",\"instruments\":[{\"symbol\":\"TESTE/BRL\",\"base\":\"TESTE\",\
             \"quote\":\"BRL\",\"tick\":10,\"lot_size\":100,\"max_lots\":1000}]}\n"
        );
        assert_eq!(ServerMessage::from_line(&line).unwrap(), instruments);
    }

    #[test]
    fn empty_top_side_is_null_on_the_wire() {
        let top = ServerMessage::Top {
            instrument: "TESTE/BRL".to_string(),
            feed: Feed::L1,
            seq: 7,
            bid: Some(Level {
                price: 9_900,
                lots: 3,
            }),
            ask: None,
        };
        assert_eq!(
            top.to_line(),
            "{\"type\":\"top\",\"instrument\":\"TESTE/BRL\",\"feed\":\"l1\",\"seq\":7,\
             \"bid\":{\"price\":9900,\"lots\":3},\"ask\":null}\n"
        );
    }

    #[test]
    fn message_text_with_quotes_stays_one_valid_line() {
        let error = ServerMessage::Error {
            message: "ativo desconhecido \"OU\nRO\"".to_string(),
        };

        let line = error.to_line();

        assert_eq!(line.matches('\n').count(), 1);
        assert!(line.ends_with('\n'));
        assert_eq!(ServerMessage::from_line(&line).unwrap(), error);
    }

    #[test]
    fn client_reading_server_messages_ignores_unknown_fields() {
        assert_eq!(
            ServerMessage::from_line(r#"{"type":"logged_in","account":1,"motd":"oi"}"#).unwrap(),
            ServerMessage::LoggedIn { account: 1 }
        );
    }
}
