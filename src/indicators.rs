//! The two numbers the entry rule needs, worked out from candle closes.

use rust_decimal::Decimal;

/// Exponential moving average of `closes` (oldest first) over `periods`.
///
/// Like a plain average, but recent prices count more: each new close moves
/// the average `2 / (periods + 1)` of the way towards it. With 14 hourly
/// closes that is about 13% per hour. `None` until we have `periods` closes.
pub fn ema(closes: &[Decimal], periods: u32) -> Option<Decimal> {
    if periods == 0 || closes.len() < periods as usize {
        return None;
    }
    let alpha = Decimal::TWO / Decimal::from(periods + 1);
    let mut average = closes[0];
    for &close in &closes[1..] {
        average += alpha * (close - average);
    }
    Some(average)
}

/// Is the last daily close above the plain average of the last `days` closes?
/// This is the trend filter: "up" means it is. `None` without enough days.
pub fn trend_is_up(daily: &[Decimal], days: usize) -> Option<bool> {
    if days == 0 || daily.len() < days {
        return None;
    }
    let recent = &daily[daily.len() - days..];
    let average = recent.iter().sum::<Decimal>() / Decimal::from(days);
    Some(*recent.last()? > average)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(s: &str) -> Decimal {
        s.parse().unwrap()
    }

    #[test]
    fn a_flat_price_has_that_average() {
        assert_eq!(ema(&[d("100"); 20], 14), Some(d("100")));
    }

    #[test]
    fn the_ema_moves_part_of_the_way_to_each_close() {
        // periods 3: alpha = 2/4 = 0.5. 100 -> 110: 105; -> 120: 112.5
        assert_eq!(ema(&[d("100"), d("110"), d("120")], 3), Some(d("112.5")));
    }

    #[test]
    fn no_ema_without_enough_closes() {
        assert_eq!(ema(&[d("100"); 13], 14), None);
        assert_eq!(ema(&[], 14), None);
    }

    #[test]
    fn the_trend_is_up_when_the_last_close_beats_the_average() {
        let rising: Vec<Decimal> = (1..=10).map(Decimal::from).collect();
        assert_eq!(trend_is_up(&rising, 5), Some(true));
        let falling: Vec<Decimal> = (1..=10).rev().map(Decimal::from).collect();
        assert_eq!(trend_is_up(&falling, 5), Some(false));
        assert_eq!(trend_is_up(&rising, 50), None, "not enough days yet");
    }
}
