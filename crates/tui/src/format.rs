//! Números para gente. Só formatação: nenhum valor daqui volta para o servidor, e a divisão
//! por 100 é exata porque o resto vira os centavos.

/// Centavos em reais, com vírgula decimal e ponto de milhar: `997_980` → `"9.979,80"`.
pub fn brl(cents: i64) -> String {
    let sign = if cents < 0 { "-" } else { "" };
    let cents = cents.unsigned_abs();
    format!("{sign}{},{:02}", grouped(cents / 100), cents % 100)
}

/// Inteiro com ponto de milhar: `1_000_000` → `"1.000.000"`.
pub fn grouped(value: u64) -> String {
    let digits = value.to_string();
    let mut text = String::with_capacity(digits.len() + digits.len() / 3);
    for (position, digit) in digits.chars().enumerate() {
        if position > 0 && (digits.len() - position).is_multiple_of(3) {
            text.push('.');
        }
        text.push(digit);
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cents_become_reais() {
        assert_eq!(brl(10_050), "100,50");
        assert_eq!(brl(997_980), "9.979,80");
        assert_eq!(brl(100_000_000), "1.000.000,00");
    }

    #[test]
    fn less_than_a_real_keeps_the_leading_zero() {
        assert_eq!(brl(0), "0,00");
        assert_eq!(brl(5), "0,05");
        assert_eq!(brl(60), "0,60");
    }

    #[test]
    fn negative_keeps_its_sign() {
        assert_eq!(brl(-123_456), "-1.234,56");
        assert_eq!(brl(i64::MIN), "-92.233.720.368.547.758,08");
    }

    #[test]
    fn groups_of_three_from_the_right() {
        assert_eq!(grouped(0), "0");
        assert_eq!(grouped(999), "999");
        assert_eq!(grouped(1_000), "1.000");
        assert_eq!(grouped(12_345_678), "12.345.678");
    }
}
