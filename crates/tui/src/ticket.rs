//! A boleta: os campos de uma ordem, editados sem modo de inserção. Os dígitos entram pela
//! direita, como numa caixa registradora (`1`, `0`, `0`, `5`, `0` dá 100,50), e o valor
//! nunca passa por um estado que não seja número.

use exchange_protocol::{ClientMessage, OrderType, Side, Tif};

use crate::format::brl;
use crate::orders::Pending;

/// Mais que isso não cabe no campo nem faz sentido no mercado; o servidor recusa o resto.
const MAX_DIGITS: u32 = 12;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Field {
    Side,
    Kind,
    Trigger,
    Price,
    Lots,
    Tif,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Ticket {
    pub side: Side,
    pub kind: OrderType,
    /// Zero é vazio: no stop, preço vazio quer dizer stop a mercado.
    pub trigger: i64,
    pub price: i64,
    pub lots: i64,
    pub tif: Tif,
    pub field: Field,
    /// O campo acabou de ganhar o cursor (ou de ser ajustado): o próximo dígito começa um
    /// valor novo em vez de ir para a direita do que já estava lá.
    fresh: bool,
}

impl Default for Ticket {
    fn default() -> Self {
        Self {
            side: Side::Buy,
            kind: OrderType::Limit,
            trigger: 0,
            price: 0,
            lots: 1,
            tif: Tif::Gtc,
            field: Field::Price,
            fresh: true,
        }
    }
}

impl Ticket {
    /// Os campos que o tipo da ordem usa, na ordem da tela.
    pub fn fields(&self) -> &'static [Field] {
        match self.kind {
            OrderType::Limit => &[
                Field::Side,
                Field::Kind,
                Field::Price,
                Field::Lots,
                Field::Tif,
            ],
            OrderType::Market => &[Field::Side, Field::Kind, Field::Lots, Field::Tif],
            OrderType::Stop => &[
                Field::Side,
                Field::Kind,
                Field::Trigger,
                Field::Price,
                Field::Lots,
                Field::Tif,
            ],
        }
    }

    /// O cursor chegou à boleta.
    pub fn enter(&mut self) {
        self.fresh = true;
    }

    /// Para o campo ao lado, sem dar a volta.
    pub fn step_field(&mut self, forward: bool) {
        self.fresh = true;
        let fields = self.fields();
        let index = fields
            .iter()
            .position(|&field| field == self.field)
            .unwrap_or(0);
        let index = if forward {
            (index + 1).min(fields.len() - 1)
        } else {
            index.saturating_sub(1)
        };
        self.field = fields[index];
    }

    /// `up` sobe o valor: o preço anda de tick em tick, os lots de um em um, e os campos de
    /// escolha alternam.
    pub fn adjust(&mut self, up: bool, tick: i64) {
        self.fresh = true;
        let step = |value: i64, by: i64, floor: i64| {
            if up {
                value.saturating_add(by)
            } else {
                value.saturating_sub(by).max(floor)
            }
        };
        match self.field {
            Field::Side => {
                self.side = match self.side {
                    Side::Buy => Side::Sell,
                    Side::Sell => Side::Buy,
                };
            }
            Field::Kind => {
                let kinds = [OrderType::Limit, OrderType::Market, OrderType::Stop];
                self.kind = cycle(&kinds, self.kind, up);
                if !self.fields().contains(&self.field) {
                    self.field = Field::Kind;
                }
            }
            Field::Tif => self.tif = cycle(&[Tif::Gtc, Tif::Ioc, Tif::Fok], self.tif, up),
            Field::Trigger => self.trigger = step(self.trigger, tick, tick),
            Field::Price => self.price = step(self.price, tick, tick),
            Field::Lots => self.lots = step(self.lots, 1, 1),
        }
    }

    pub fn digit(&mut self, digit: u32) {
        let fresh = std::mem::take(&mut self.fresh);
        if let Some(value) = self.number()
            && fresh
        {
            *value = 0;
        }
        if let Some(value) = self.number()
            && value.checked_ilog10().unwrap_or(0) + 1 < MAX_DIGITS
        {
            *value = *value * 10 + i64::from(digit);
        }
    }

    pub fn backspace(&mut self) {
        self.fresh = false;
        if let Some(value) = self.number() {
            *value /= 10;
        }
    }

    fn number(&mut self) -> Option<&mut i64> {
        match self.field {
            Field::Trigger => Some(&mut self.trigger),
            Field::Price => Some(&mut self.price),
            Field::Lots => Some(&mut self.lots),
            Field::Side | Field::Kind | Field::Tif => None,
        }
    }

    /// O preço de um nível do livro: no stop vira o gatilho; numa ordem a mercado, que não
    /// tem preço, transforma a boleta em limite.
    pub fn take_price(&mut self, price: i64) {
        self.fresh = true;
        match self.kind {
            OrderType::Stop => {
                self.trigger = price;
                self.field = Field::Trigger;
            }
            OrderType::Market | OrderType::Limit => {
                self.kind = OrderType::Limit;
                self.price = price;
                self.field = Field::Price;
            }
        }
    }

    /// A mensagem para o servidor, marcada com `tag`, e o que a TUI espera ver voltar dela.
    ///
    /// # Errors
    /// Um texto para a linha de status se faltar um campo que o tipo exige.
    pub fn order(
        &self,
        instrument: &str,
        tag: u64,
    ) -> Result<(ClientMessage, Pending), &'static str> {
        if self.lots <= 0 {
            return Err("informe os lots");
        }
        let instrument = instrument.to_string();
        let (message, price, trigger) = match self.kind {
            OrderType::Limit if self.price <= 0 => return Err("informe o preço"),
            OrderType::Stop if self.trigger <= 0 => return Err("informe o gatilho"),
            OrderType::Limit => (
                ClientMessage::Limit {
                    instrument: instrument.clone(),
                    side: self.side,
                    price: self.price,
                    lots: self.lots,
                    tif: self.tif,
                    tag: Some(tag),
                },
                Some(self.price),
                None,
            ),
            OrderType::Market => (
                ClientMessage::Market {
                    instrument: instrument.clone(),
                    side: self.side,
                    lots: self.lots,
                    protection: None,
                    tif: self.tif,
                    tag: Some(tag),
                },
                None,
                None,
            ),
            OrderType::Stop => {
                let limit = (self.price > 0).then_some(self.price);
                (
                    ClientMessage::Stop {
                        instrument: instrument.clone(),
                        side: self.side,
                        trigger: self.trigger,
                        limit,
                        lots: self.lots,
                        tif: self.tif,
                        tag: Some(tag),
                    },
                    limit,
                    Some(self.trigger),
                )
            }
        };
        let pending = Pending {
            instrument,
            side: self.side,
            kind: self.kind,
            price,
            trigger,
            tif: self.tif,
            lots: self.lots,
        };
        Ok((message, pending))
    }

    /// A ordem em palavras, para a linha de status: "compra 5 lots @ 100,50 lim GTC".
    pub fn describe(&self) -> String {
        let side = match self.side {
            Side::Buy => "compra",
            Side::Sell => "venda",
        };
        let tif = tif_label(self.tif);
        let lots = self.lots;
        match self.kind {
            OrderType::Limit => format!("{side} {lots} lots @ {} lim {tif}", brl(self.price)),
            OrderType::Market => format!("{side} {lots} lots a mercado {tif}"),
            OrderType::Stop => {
                let limit = if self.price > 0 {
                    format!("limite {}", brl(self.price))
                } else {
                    "a mercado".to_string()
                };
                format!(
                    "{side} {lots} lots stop {} {limit} {tif}",
                    brl(self.trigger)
                )
            }
        }
    }
}

pub fn tif_label(tif: Tif) -> &'static str {
    match tif {
        Tif::Gtc => "GTC",
        Tif::Ioc => "IOC",
        Tif::Fok => "FOK",
    }
}

fn cycle<T: Copy + PartialEq>(options: &[T], current: T, forward: bool) -> T {
    let index = options
        .iter()
        .position(|&option| option == current)
        .unwrap_or(0);
    let next = if forward {
        (index + 1) % options.len()
    } else {
        (index + options.len() - 1) % options.len()
    };
    options[next]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn typed(ticket: &mut Ticket, digits: &str) {
        for digit in digits.chars() {
            ticket.digit(digit.to_digit(10).unwrap());
        }
    }

    #[test]
    fn digits_enter_from_the_right_as_cents() {
        let mut ticket = Ticket::default();

        typed(&mut ticket, "10050");
        assert_eq!(ticket.price, 10_050);
        ticket.backspace();
        assert_eq!(ticket.price, 1_005);
    }

    #[test]
    fn first_digit_in_a_field_replaces_its_value() {
        let mut ticket = Ticket {
            field: Field::Lots,
            ..Ticket::default()
        };

        typed(&mut ticket, "4");
        assert_eq!(ticket.lots, 4);
        typed(&mut ticket, "2");
        assert_eq!(ticket.lots, 42);

        ticket.adjust(true, 10);
        typed(&mut ticket, "7");
        assert_eq!(ticket.lots, 7);

        ticket.step_field(false);
        ticket.step_field(true);
        ticket.backspace();
        typed(&mut ticket, "3");
        assert_eq!(ticket.lots, 3);
    }

    #[test]
    fn digits_stop_at_the_field_limit() {
        let mut ticket = Ticket::default();

        typed(&mut ticket, "99999999999999999");

        assert_eq!(ticket.price, 999_999_999_999);
    }

    #[test]
    fn digits_are_ignored_on_choice_fields() {
        let mut ticket = Ticket {
            field: Field::Tif,
            ..Ticket::default()
        };

        typed(&mut ticket, "5");

        assert_eq!(ticket.tif, Tif::Gtc);
        assert_eq!(ticket.price, 0);
        assert_eq!(ticket.lots, 1);
    }

    #[test]
    fn price_moves_by_tick_and_never_below_one_tick() {
        let mut ticket = Ticket::default();

        ticket.adjust(true, 10);
        ticket.adjust(true, 10);
        assert_eq!(ticket.price, 20);
        ticket.adjust(false, 10);
        ticket.adjust(false, 10);
        ticket.adjust(false, 10);
        assert_eq!(ticket.price, 10);
    }

    #[test]
    fn lots_never_go_below_one() {
        let mut ticket = Ticket {
            field: Field::Lots,
            ..Ticket::default()
        };

        ticket.adjust(false, 10);

        assert_eq!(ticket.lots, 1);
    }

    #[test]
    fn kind_cycles_and_the_fields_follow_it() {
        let mut ticket = Ticket {
            field: Field::Kind,
            ..Ticket::default()
        };

        ticket.adjust(true, 10);
        assert_eq!(ticket.kind, OrderType::Market);
        assert!(!ticket.fields().contains(&Field::Price));
        ticket.adjust(true, 10);
        assert_eq!(ticket.kind, OrderType::Stop);
        assert!(ticket.fields().contains(&Field::Trigger));
        ticket.adjust(true, 10);
        assert_eq!(ticket.kind, OrderType::Limit);
    }

    #[test]
    fn h_and_l_walk_the_visible_fields_without_wrapping() {
        let mut ticket = Ticket::default();

        ticket.step_field(true);
        ticket.step_field(true);
        ticket.step_field(true);
        assert_eq!(ticket.field, Field::Tif);
        for _ in 0..9 {
            ticket.step_field(false);
        }
        assert_eq!(ticket.field, Field::Side);
    }

    #[test]
    fn price_from_the_book_fills_price_or_trigger() {
        let mut ticket = Ticket {
            kind: OrderType::Market,
            ..Ticket::default()
        };

        ticket.take_price(10_050);
        assert_eq!((ticket.kind, ticket.price), (OrderType::Limit, 10_050));

        ticket.kind = OrderType::Stop;
        ticket.take_price(9_500);
        assert_eq!((ticket.trigger, ticket.field), (9_500, Field::Trigger));
    }

    #[test]
    fn each_kind_becomes_its_message() {
        let limit = Ticket {
            price: 10_050,
            lots: 5,
            ..Ticket::default()
        };
        assert_eq!(
            limit.order("TESTE/BRL", 1).unwrap().0,
            ClientMessage::Limit {
                instrument: "TESTE/BRL".to_string(),
                side: Side::Buy,
                price: 10_050,
                lots: 5,
                tif: Tif::Gtc,
                tag: Some(1),
            }
        );

        let market = Ticket {
            kind: OrderType::Market,
            side: Side::Sell,
            tif: Tif::Ioc,
            ..limit.clone()
        };
        assert_eq!(
            market.order("TESTE/BRL", 1).unwrap().0,
            ClientMessage::Market {
                instrument: "TESTE/BRL".to_string(),
                side: Side::Sell,
                lots: 5,
                protection: None,
                tif: Tif::Ioc,
                tag: Some(1),
            }
        );

        let stop_market = Ticket {
            kind: OrderType::Stop,
            trigger: 9_500,
            price: 0,
            ..limit
        };
        let (message, pending) = stop_market.order("TESTE/BRL", 1).unwrap();
        assert_eq!(
            message,
            ClientMessage::Stop {
                instrument: "TESTE/BRL".to_string(),
                side: Side::Buy,
                trigger: 9_500,
                limit: None,
                lots: 5,
                tif: Tif::Gtc,
                tag: Some(1),
            }
        );
        assert_eq!(pending.trigger, Some(9_500));
        assert_eq!(pending.price, None);
    }

    #[test]
    fn missing_fields_are_refused_before_reaching_the_server() {
        assert_eq!(
            Ticket::default().order("TESTE/BRL", 1).unwrap_err(),
            "informe o preço"
        );
        let stop = Ticket {
            kind: OrderType::Stop,
            ..Ticket::default()
        };
        assert_eq!(stop.order("TESTE/BRL", 1).unwrap_err(), "informe o gatilho");
        let empty = Ticket {
            price: 100,
            lots: 0,
            ..Ticket::default()
        };
        assert_eq!(empty.order("TESTE/BRL", 1).unwrap_err(), "informe os lots");
    }

    #[test]
    fn description_reads_like_the_order() {
        let ticket = Ticket {
            price: 10_050,
            lots: 5,
            ..Ticket::default()
        };
        assert_eq!(ticket.describe(), "compra 5 lots @ 100,50 lim GTC");
        let stop = Ticket {
            kind: OrderType::Stop,
            side: Side::Sell,
            trigger: 9_500,
            price: 9_400,
            ..ticket
        };
        assert_eq!(stop.describe(), "venda 5 lots stop 95,00 limite 94,00 GTC");
    }
}
