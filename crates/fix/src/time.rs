use std::fmt;

const MILLIS_PER_SECOND: u64 = 1_000;
const SECONDS_PER_DAY: u64 = 86_400;

/// Milissegundos desde 1970-01-01T00:00:00Z. Vai até o fim do ano 9999 porque o formato
/// do FIX (`YYYYMMDD-HH:MM:SS.sss`) só tem quatro dígitos de ano.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct UtcTimestamp(u64);

impl UtcTimestamp {
    pub const MAX: Self =
        Self(days_from_civil(10_000, 1, 1) * SECONDS_PER_DAY * MILLIS_PER_SECOND - 1);

    pub const fn from_unix_millis(millis: u64) -> Option<Self> {
        if millis <= Self::MAX.0 {
            Some(Self(millis))
        } else {
            None
        }
    }

    pub const fn as_unix_millis(self) -> u64 {
        self.0
    }

    /// Aceita `YYYYMMDD-HH:MM:SS.sss` e também a forma sem milissegundos, que o FIX 4.4
    /// permite. Datas antes de 1970 ficam de fora: não existem num simulador que nasceu
    /// depois disso, e assim toda a aritmética fica em `u64`.
    pub fn parse(text: &[u8]) -> Option<Self> {
        let (date_time, millis) = match text.len() {
            17 => (text, 0),
            21 if text[17] == b'.' => (&text[..17], digits(&text[18..])?),
            _ => return None,
        };
        if [date_time[8], date_time[11], date_time[14]] != *b"-::" {
            return None;
        }
        let year = digits(&date_time[0..4])?;
        let month = digits(&date_time[4..6])?;
        let day = digits(&date_time[6..8])?;
        let hour = digits(&date_time[9..11])?;
        let minute = digits(&date_time[12..14])?;
        let second = digits(&date_time[15..17])?;

        let valid = year >= 1970
            && (1..=12).contains(&month)
            && (1..=days_in_month(year, month)).contains(&day)
            && hour < 24
            && minute < 60
            && second < 60;
        if !valid {
            return None;
        }
        let seconds = days_from_civil(year, month, day) * SECONDS_PER_DAY
            + hour * 3_600
            + minute * 60
            + second;
        Some(Self(seconds * MILLIS_PER_SECOND + millis))
    }
}

impl fmt::Display for UtcTimestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let seconds = self.0 / MILLIS_PER_SECOND;
        let millis = self.0 % MILLIS_PER_SECOND;
        let (year, month, day) = civil_from_days(seconds / SECONDS_PER_DAY);
        let of_day = seconds % SECONDS_PER_DAY;
        write!(
            f,
            "{year:04}{month:02}{day:02}-{:02}:{:02}:{:02}.{millis:03}",
            of_day / 3_600,
            of_day / 60 % 60,
            of_day % 60
        )
    }
}

fn digits(text: &[u8]) -> Option<u64> {
    text.iter().try_fold(0u64, |number, &digit| {
        digit
            .is_ascii_digit()
            .then(|| number * 10 + u64::from(digit - b'0'))
    })
}

fn is_leap(year: u64) -> bool {
    year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400))
}

fn days_in_month(year: u64, month: u64) -> u64 {
    match month {
        2 if is_leap(year) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// Algoritmo de Howard Hinnant (`days_from_civil`): conta o ano a partir de março, para
/// que o 29 de fevereiro caia no fim do ano e os meses tenham uma fórmula fechada, e
/// agrupa os anos em eras de 400, que é o período em que o calendário gregoriano se
/// repete. Só vale para datas a partir de 1970, já que o resultado é `u64`.
const fn days_from_civil(year: u64, month: u64, day: u64) -> u64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year / 400;
    let year_of_era = year % 400;
    let month_from_march = (month + 9) % 12;
    let day_of_year = (153 * month_from_march + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// O inverso de [`days_from_civil`].
fn civil_from_days(days: u64) -> (u64, u64, u64) {
    let days = days + 719_468;
    let era = days / 146_097;
    let day_of_era = days % 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_from_march = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_from_march + 2) / 5 + 1;
    let month = if month_from_march < 10 {
        month_from_march + 3
    } else {
        month_from_march - 9
    };
    let year = era * 400 + year_of_era + u64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn at(millis: u64) -> UtcTimestamp {
        UtcTimestamp::from_unix_millis(millis).expect("instante de teste dentro do limite")
    }

    #[test]
    fn epoch_is_midnight_of_1970() {
        assert_eq!(at(0).to_string(), "19700101-00:00:00.000");
    }

    #[test]
    fn leap_day_of_2000_exists() {
        assert_eq!(at(951_782_400_000).to_string(), "20000229-00:00:00.000");
        assert_eq!(
            UtcTimestamp::parse(b"20000229-00:00:00.000"),
            Some(at(951_782_400_000))
        );
    }

    #[test]
    fn last_instant_is_end_of_9999() {
        assert_eq!(UtcTimestamp::MAX.to_string(), "99991231-23:59:59.999");
        assert_eq!(
            UtcTimestamp::from_unix_millis(UtcTimestamp::MAX.as_unix_millis() + 1),
            None
        );
    }

    #[test]
    fn accepts_form_without_millis() {
        assert_eq!(UtcTimestamp::parse(b"19700101-00:00:01"), Some(at(1_000)));
    }

    #[test]
    fn rejects_impossible_dates_and_times() {
        for text in [
            "20260230-00:00:00.000",
            "21000229-00:00:00.000",
            "20261301-00:00:00.000",
            "20261000-00:00:00.000",
            "20261015-24:00:00.000",
            "20261015-12:60:00.000",
            "20261015-12:00:60.000",
            "19691231-23:59:59.999",
            "20261015 12:00:00.000",
            "20261015-12:00:00,000",
            "20261015-12:00:00.0",
            "2026-10-15T12:00:00",
            "20261015-12:00:0x.000",
        ] {
            assert_eq!(UtcTimestamp::parse(text.as_bytes()), None, "{text}");
        }
    }

    proptest! {
        #[test]
        fn parse_inverts_display(millis in 0..=UtcTimestamp::MAX.as_unix_millis()) {
            let timestamp = at(millis);
            prop_assert_eq!(
                UtcTimestamp::parse(timestamp.to_string().as_bytes()),
                Some(timestamp)
            );
        }
    }
}
