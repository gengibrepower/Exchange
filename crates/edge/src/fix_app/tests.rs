use exchange_core::domain::Amount;

use super::*;
use crate::market::{BRL, OUTRO, TESTE};
use crate::sequencer::Sequencer;

const SELLER: AccountId = AccountId::new(1);
const BUYER: AccountId = AccountId::new(2);

struct Exchange {
    core: Sequencer,
    next_seq: u64,
    /// Os eventos do último comando, para entregar às outras sessões como os relatórios
    /// privados entregariam.
    last: Vec<Event>,
}

impl Exchange {
    fn new() -> Self {
        let mut core = Sequencer::in_memory();
        for account in [SELLER, BUYER] {
            for asset in [TESTE, OUTRO, BRL] {
                let amount = Amount::new(10_000_000).expect("quantia de teste válida");
                core.submit(Intent::Deposit {
                    account,
                    asset,
                    amount,
                })
                .expect("depósito em memória não falha");
            }
        }
        Self {
            core,
            next_seq: 1,
            last: Vec::new(),
        }
    }

    fn send(&mut self, orders: &mut FixOrders, app: &AppMessage) -> Vec<AppMessage> {
        let seq = SeqNum::new(self.next_seq).expect("seq de teste não é zero");
        self.next_seq += 1;
        match orders.request(seq, app) {
            Request::Reply(reply) => vec![reply],
            Request::Submit { intent, pending } => {
                self.last = self
                    .core
                    .submit(intent)
                    .expect("núcleo em memória não falha");
                orders.outcome(pending, &self.last)
            }
        }
    }
}

fn message(msg_type: &str, fields: &[(u32, &str)]) -> AppMessage {
    let fields = fields
        .iter()
        .map(|&(number, value)| Field::new(tag(number), value).expect("campo de teste válido"))
        .collect();
    AppMessage::new(Message::new(msg_type, fields).expect("mensagem de teste válida"))
        .expect("tipo de aplicação")
}

fn limit(cl_ord_id: &str, side: &str, lots: &str, price: &str) -> AppMessage {
    limit_on("TESTE/BRL", cl_ord_id, side, lots, price)
}

fn limit_on(symbol: &str, cl_ord_id: &str, side: &str, lots: &str, price: &str) -> AppMessage {
    message(
        "D",
        &[
            (11, cl_ord_id),
            (55, symbol),
            (54, side),
            (38, lots),
            (40, "2"),
            (44, price),
        ],
    )
}

fn cancel(cl_ord_id: &str, orig: &str) -> AppMessage {
    message(
        "F",
        &[(11, cl_ord_id), (41, orig), (55, "TESTE/BRL"), (54, "2")],
    )
}

fn get(app: &AppMessage, number: u32) -> Option<&str> {
    app.message()
        .body()
        .iter()
        .find(|field| field.tag() == tag(number))
        .map(|field| std::str::from_utf8(field.value()).expect("saída é ASCII ou UTF-8"))
}

fn msg_type(app: &AppMessage) -> &str {
    std::str::from_utf8(app.message().msg_type()).expect("tipo é ASCII")
}

/// `(ExecType, OrdStatus, CumQty, LeavesQty)` de cada relatório, para comparar de uma vez.
fn executions(reports: &[AppMessage]) -> Vec<(&str, &str, &str, &str)> {
    reports
        .iter()
        .map(|report| {
            assert_eq!(msg_type(report), "8");
            let field = |number| get(report, number).expect("campo obrigatório do relatório");
            (field(150), field(39), field(14), field(151))
        })
        .collect()
}

#[test]
fn resting_limit_order_is_acknowledged_as_new() {
    let mut exchange = Exchange::new();
    let mut seller = FixOrders::new(SELLER);
    let reports = exchange.send(&mut seller, &limit("v1", "2", "5", "1000"));
    assert_eq!(executions(&reports), vec![("0", "0", "0", "5")]);
    assert_eq!(get(&reports[0], 11), Some("v1"));
    assert_eq!(get(&reports[0], 55), Some("TESTE/BRL"));
    assert_eq!(get(&reports[0], 54), Some("2"));
    assert_eq!(get(&reports[0], 38), Some("5"));
}

// 2005 não cai no tick de 10 do TESTE/BRL, mas cai no de 5 do OUTRO/BRL: se o símbolo
// fosse parar no instrumento errado, a ordem seria rejeitada.
#[test]
fn second_instrument_trades_under_its_own_symbol_and_tick() {
    let mut exchange = Exchange::new();
    let mut seller = FixOrders::new(SELLER);
    let mut buyer = FixOrders::new(BUYER);
    exchange.send(&mut seller, &limit_on("OUTRO/BRL", "v1", "2", "3", "2005"));
    let reports = exchange.send(&mut buyer, &limit_on("OUTRO/BRL", "c1", "1", "2", "2005"));
    assert_eq!(
        executions(&reports),
        vec![("0", "0", "0", "2"), ("F", "2", "2", "0")]
    );
    assert_eq!(get(&reports[1], 55), Some("OUTRO/BRL"));
    assert_eq!(get(&reports[1], 31), Some("2005"));
}

#[test]
fn crossing_order_reports_new_then_each_trade() {
    let mut exchange = Exchange::new();
    let mut seller = FixOrders::new(SELLER);
    let mut buyer = FixOrders::new(BUYER);
    exchange.send(&mut seller, &limit("v1", "2", "3", "1000"));
    exchange.send(&mut seller, &limit("v2", "2", "3", "1010"));
    let reports = exchange.send(&mut buyer, &limit("c1", "1", "4", "1010"));
    assert_eq!(
        executions(&reports),
        vec![
            ("0", "0", "0", "4"),
            ("F", "1", "3", "1"),
            ("F", "2", "4", "0")
        ]
    );
    assert_eq!(get(&reports[1], 31), Some("1000"));
    assert_eq!(get(&reports[1], 32), Some("3"));
    assert_eq!(get(&reports[2], 31), Some("1010"));
    assert_eq!(get(&reports[2], 32), Some("1"));
    let exec_ids: Vec<_> = reports
        .iter()
        .filter_map(|report| get(report, 17))
        .collect();
    assert_eq!(exec_ids, vec!["E1", "E2", "E3"]);
}

#[test]
fn self_trade_reports_both_sides() {
    let mut exchange = Exchange::new();
    let mut trader = FixOrders::new(SELLER);
    exchange.send(&mut trader, &limit("v1", "2", "2", "1000"));
    let reports = exchange.send(&mut trader, &limit("c1", "1", "2", "1000"));
    let ids: Vec<_> = reports.iter().map(|report| get(report, 11)).collect();
    assert_eq!(ids, vec![Some("c1"), Some("c1"), Some("v1")]);
    assert_eq!(
        executions(&reports),
        vec![
            ("0", "0", "0", "2"),
            ("F", "2", "2", "0"),
            ("F", "2", "2", "0")
        ]
    );
}

#[test]
fn reused_cl_ord_id_is_rejected_without_reaching_the_core() {
    let mut exchange = Exchange::new();
    let mut seller = FixOrders::new(SELLER);
    exchange.send(&mut seller, &limit("v1", "2", "5", "1000"));
    let reports = exchange.send(&mut seller, &limit("v1", "2", "5", "1000"));
    assert_eq!(executions(&reports), vec![("8", "8", "0", "0")]);
    assert_eq!(get(&reports[0], 37), Some("NONE"));
    assert_eq!(get(&reports[0], 103), Some("6"));

    let mut buyer = FixOrders::new(BUYER);
    let crossed = exchange.send(&mut buyer, &limit("c1", "1", "10", "1000"));
    assert_eq!(
        get(crossed.last().expect("houve negócio"), 14),
        Some("5"),
        "só a primeira v1 entrou no livro"
    );
}

#[test]
fn core_rejection_becomes_rejected_report_with_reason() {
    let mut exchange = Exchange::new();
    let mut seller = FixOrders::new(SELLER);
    let reports = exchange.send(&mut seller, &limit("v1", "2", "5", "1005"));
    assert_eq!(executions(&reports), vec![("8", "8", "0", "0")]);
    assert_eq!(get(&reports[0], 103), Some("99"));
    assert_eq!(get(&reports[0], 58), Some("preço 1005 fora do tick 10"));
    assert_ne!(get(&reports[0], 37), Some("NONE"));
}

fn with_time_in_force(cl_ord_id: &str, lots: &str, tif: &str) -> AppMessage {
    message(
        "D",
        &[
            (11, cl_ord_id),
            (55, "TESTE/BRL"),
            (54, "1"),
            (38, lots),
            (40, "2"),
            (44, "1000"),
            (59, tif),
        ],
    )
}

#[test]
fn immediate_or_cancel_remainder_is_cancelled_after_the_fill() {
    let mut exchange = Exchange::new();
    let mut seller = FixOrders::new(SELLER);
    let mut buyer = FixOrders::new(BUYER);
    exchange.send(&mut seller, &limit("v1", "2", "1", "1000"));
    let reports = exchange.send(&mut buyer, &with_time_in_force("c1", "3", "3"));
    assert_eq!(
        executions(&reports),
        vec![
            ("0", "0", "0", "3"),
            ("F", "1", "1", "2"),
            ("4", "4", "1", "0")
        ]
    );
    assert_eq!(
        get(&reports[2], 41),
        None,
        "cancelada pelo núcleo, não por pedido"
    );
}

#[test]
fn fill_or_kill_without_enough_liquidity_expires() {
    let mut exchange = Exchange::new();
    let mut seller = FixOrders::new(SELLER);
    let mut buyer = FixOrders::new(BUYER);
    exchange.send(&mut seller, &limit("v1", "2", "1", "1000"));
    let reports = exchange.send(&mut buyer, &with_time_in_force("c1", "3", "4"));
    assert_eq!(
        executions(&reports),
        vec![("0", "0", "0", "3"), ("C", "C", "0", "0")]
    );
}

#[test]
fn cancel_reports_the_new_and_original_cl_ord_ids() {
    let mut exchange = Exchange::new();
    let mut seller = FixOrders::new(SELLER);
    let placed = exchange.send(&mut seller, &limit("v1", "2", "5", "1000"));
    let reports = exchange.send(&mut seller, &cancel("x1", "v1"));
    assert_eq!(executions(&reports), vec![("4", "4", "0", "0")]);
    assert_eq!(get(&reports[0], 11), Some("x1"));
    assert_eq!(get(&reports[0], 41), Some("v1"));
    assert_eq!(get(&reports[0], 37), get(&placed[0], 37));

    let again = exchange.send(&mut seller, &cancel("x2", "v1"));
    assert_eq!(msg_type(&again[0]), "9");
    assert_eq!(get(&again[0], 102), Some("0"), "já cancelada: tarde demais");
    assert_eq!(get(&again[0], 39), Some("4"));
}

#[test]
fn cancel_of_unknown_order_is_rejected() {
    let mut exchange = Exchange::new();
    let mut seller = FixOrders::new(SELLER);
    let reports = exchange.send(&mut seller, &cancel("x1", "nunca-existiu"));
    assert_eq!(msg_type(&reports[0]), "9");
    assert_eq!(get(&reports[0], 37), Some("NONE"));
    assert_eq!(get(&reports[0], 102), Some("1"));
    assert_eq!(get(&reports[0], 434), Some("1"));
}

#[test]
fn cancel_reusing_a_cl_ord_id_is_rejected() {
    let mut exchange = Exchange::new();
    let mut seller = FixOrders::new(SELLER);
    exchange.send(&mut seller, &limit("v1", "2", "5", "1000"));
    let reports = exchange.send(&mut seller, &cancel("v1", "v1"));
    assert_eq!(msg_type(&reports[0]), "9");
    assert_eq!(get(&reports[0], 102), Some("6"));
}

#[test]
fn missing_and_invalid_fields_become_business_rejects() {
    let mut exchange = Exchange::new();
    let mut seller = FixOrders::new(SELLER);
    let without_cl_ord_id = message("D", &[(55, "TESTE/BRL"), (54, "2"), (38, "5"), (40, "1")]);
    let reports = exchange.send(&mut seller, &without_cl_ord_id);
    assert_eq!(msg_type(&reports[0]), "j");
    assert_eq!(get(&reports[0], 45), Some("1"));
    assert_eq!(get(&reports[0], 372), Some("D"));
    assert_eq!(get(&reports[0], 380), Some("5"));
    assert_eq!(get(&reports[0], 58), Some("tag 11 ausente"));

    for (number, bad) in [(55, "XYZ/BRL"), (54, "7"), (38, "-5"), (40, "P"), (59, "0")] {
        let mut fields = vec![
            (11, "v9"),
            (55, "TESTE/BRL"),
            (54, "2"),
            (38, "5"),
            (40, "1"),
        ];
        fields.retain(|&(tag, _)| tag != number);
        fields.push((number, bad));
        let reports = exchange.send(&mut seller, &message("D", &fields));
        assert_eq!(msg_type(&reports[0]), "j", "tag {number}={bad}");
        assert_eq!(get(&reports[0], 380), Some("0"));
        assert_eq!(
            get(&reports[0], 58),
            Some(format!("tag {number} inválida").as_str())
        );
    }
}

#[test]
fn limit_without_price_is_missing_a_field() {
    let mut exchange = Exchange::new();
    let mut seller = FixOrders::new(SELLER);
    let no_price = message(
        "D",
        &[
            (11, "v1"),
            (55, "TESTE/BRL"),
            (54, "2"),
            (38, "5"),
            (40, "2"),
        ],
    );
    let reports = exchange.send(&mut seller, &no_price);
    assert_eq!(get(&reports[0], 58), Some("tag 44 ausente"));
}

#[test]
fn unsupported_message_type_is_business_rejected() {
    let mut exchange = Exchange::new();
    let mut seller = FixOrders::new(SELLER);
    let reports = exchange.send(&mut seller, &message("G", &[(11, "x")]));
    assert_eq!(msg_type(&reports[0]), "j");
    assert_eq!(get(&reports[0], 372), Some("G"));
    assert_eq!(get(&reports[0], 380), Some("3"));
}

fn stop(cl_ord_id: &str, side: &str, lots: &str, stop_px: &str, limit: Option<&str>) -> AppMessage {
    let mut fields = vec![
        (11, cl_ord_id),
        (55, "TESTE/BRL"),
        (54, side),
        (38, lots),
        (99, stop_px),
    ];
    match limit {
        Some(price) => fields.extend([(40, "4"), (44, price)]),
        None => fields.push((40, "3")),
    }
    message("D", &fields)
}

// Dormente, o stop é só `New`: a venda que chega não negocia contra ele.
#[test]
fn armed_stop_is_new_and_stays_out_of_the_book() {
    let mut exchange = Exchange::new();
    let mut seller = FixOrders::new(SELLER);
    let mut buyer = FixOrders::new(BUYER);
    let reports = exchange.send(&mut buyer, &stop("c1", "1", "2", "1000", Some("1000")));
    assert_eq!(executions(&reports), vec![("0", "0", "0", "2")]);

    let reports = exchange.send(&mut seller, &limit("v1", "2", "2", "1000"));
    assert_eq!(executions(&reports), vec![("0", "0", "0", "2")]);
}

#[test]
fn stop_fired_by_its_own_session_reports_triggered_then_trades() {
    let mut exchange = Exchange::new();
    let mut seller = FixOrders::new(SELLER);
    let mut buyer = FixOrders::new(BUYER);
    exchange.send(&mut seller, &limit("v1", "2", "1", "1000"));
    exchange.send(&mut seller, &limit("v2", "2", "5", "1010"));
    exchange.send(&mut buyer, &stop("c1", "1", "2", "1000", None));

    let reports = exchange.send(&mut buyer, &limit("c2", "1", "1", "1000"));

    assert_eq!(
        executions(&reports),
        vec![
            ("0", "0", "0", "1"),
            ("F", "2", "1", "0"),
            ("L", "0", "0", "2"),
            ("F", "2", "2", "0")
        ]
    );
    let ids: Vec<_> = reports.iter().map(|report| get(report, 11)).collect();
    assert_eq!(ids, vec![Some("c2"), Some("c2"), Some("c1"), Some("c1")]);
    assert_eq!(get(&reports[3], 31), Some("1010"));
}

// O stop disparado pela ordem de outra sessão não aparece para ela.
#[test]
fn stop_of_another_session_is_not_reported() {
    let mut exchange = Exchange::new();
    let mut seller = FixOrders::new(SELLER);
    let mut buyer = FixOrders::new(BUYER);
    exchange.send(&mut seller, &limit("v1", "2", "1", "1000"));
    exchange.send(&mut seller, &limit("v2", "2", "5", "1010"));
    exchange.send(&mut buyer, &stop("c1", "1", "2", "1000", None));
    let mut other = FixOrders::new(BUYER);

    let reports = exchange.send(&mut other, &limit("o1", "1", "1", "1000"));

    assert_eq!(
        executions(&reports),
        vec![("0", "0", "0", "1"), ("F", "2", "1", "0")]
    );
}

#[test]
fn stop_needs_its_stop_price_and_stop_limit_its_price() {
    let mut exchange = Exchange::new();
    let mut buyer = FixOrders::new(BUYER);
    let no_stop_px = message(
        "D",
        &[
            (11, "c1"),
            (55, "TESTE/BRL"),
            (54, "1"),
            (38, "1"),
            (40, "3"),
        ],
    );
    let reports = exchange.send(&mut buyer, &no_stop_px);
    assert_eq!(get(&reports[0], 58), Some("tag 99 ausente"));

    let no_price = message(
        "D",
        &[
            (11, "c2"),
            (55, "TESTE/BRL"),
            (54, "1"),
            (38, "1"),
            (40, "4"),
            (99, "1000"),
        ],
    );
    let reports = exchange.send(&mut buyer, &no_price);
    assert_eq!(get(&reports[0], 58), Some("tag 44 ausente"));
}

#[test]
fn resting_order_taken_by_another_session_is_reported_to_its_own() {
    let mut exchange = Exchange::new();
    let mut seller = FixOrders::new(SELLER);
    let mut buyer = FixOrders::new(BUYER);
    exchange.send(&mut seller, &limit("v1", "2", "5", "1000"));
    exchange.send(&mut buyer, &limit("c1", "1", "2", "1000"));

    let reports = seller.executions(&exchange.last);

    assert_eq!(executions(&reports), vec![("F", "1", "2", "3")]);
    assert_eq!(get(&reports[0], 11), Some("v1"));
    assert_eq!(get(&reports[0], 31), Some("1000"));
    assert_eq!(get(&reports[0], 32), Some("2"));
}

// A mesma conta por outra porta (o JSON) não tem `ClOrdID` nesta sessão.
#[test]
fn order_that_did_not_enter_through_this_session_is_not_reported() {
    let mut exchange = Exchange::new();
    let mut seller = FixOrders::new(SELLER);
    let mut buyer = FixOrders::new(BUYER);
    exchange.send(&mut seller, &limit("v1", "2", "5", "1000"));
    exchange.send(&mut buyer, &limit("c1", "1", "2", "1000"));

    assert_eq!(
        FixOrders::new(SELLER).executions(&exchange.last),
        Vec::new()
    );
}

#[test]
fn stop_fired_by_another_session_reports_triggered_then_trade_to_its_owner() {
    let mut exchange = Exchange::new();
    let mut seller = FixOrders::new(SELLER);
    let mut buyer = FixOrders::new(BUYER);
    exchange.send(&mut seller, &limit("v1", "2", "1", "1000"));
    exchange.send(&mut seller, &limit("v2", "2", "5", "1010"));
    exchange.send(&mut buyer, &stop("c1", "1", "2", "1000", None));
    let mut other = FixOrders::new(BUYER);
    exchange.send(&mut other, &limit("o1", "1", "1", "1000"));

    let reports = buyer.executions(&exchange.last);

    assert_eq!(
        executions(&reports),
        vec![("L", "0", "0", "2"), ("F", "2", "2", "0")]
    );
    assert_eq!(get(&reports[1], 31), Some("1010"));
}
