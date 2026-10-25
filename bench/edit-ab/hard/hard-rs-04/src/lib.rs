//! Money in whole cents with exact rounding, allocation, tax and invoices.
//!
//! Nothing here uses floating point. Every ratio is computed in 128 bit
//! integers and rounded in one place with an explicit rule.

use std::fmt;
use std::iter::Sum;
use std::ops::{Add, AddAssign, Neg, Sub};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MoneyError {
    Parse(String),
    Overflow,
    DivideByZero,
    NoWeights,
}

impl fmt::Display for MoneyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MoneyError::Parse(t) => write!(f, "cannot read money from {t:?}"),
            MoneyError::Overflow => write!(f, "amount is too large"),
            MoneyError::DivideByZero => write!(f, "division by zero"),
            MoneyError::NoWeights => write!(f, "weights are empty or all zero"),
        }
    }
}

impl std::error::Error for MoneyError {}

// -- rounding ----------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rounding {
    /// Toward negative infinity.
    Floor,
    /// Toward positive infinity.
    Ceil,
    /// Toward zero.
    Truncate,
    /// To the nearest, halves away from zero.
    HalfUp,
    /// To the nearest, halves toward zero.
    HalfDown,
    /// To the nearest, halves to the even neighbour.
    HalfEven,
}

fn div_floor(n: i128, d: i128) -> i128 {
    let q = n / d;
    if (n % d != 0) && ((n < 0) != (d < 0)) {
        q - 1
    } else {
        q
    }
}

fn div_ceil(n: i128, d: i128) -> i128 {
    let q = n / d;
    if (n % d != 0) && ((n < 0) == (d < 0)) {
        q + 1
    } else {
        q
    }
}

fn div_half_up(n: i128, d: i128) -> i128 {
    let (q, r) = (n / d, n % d);
    if r.abs() * 2 >= d.abs() {
        q + n.signum() * d.signum()
    } else {
        q
    }
}

fn div_half_down(n: i128, d: i128) -> i128 {
    let (q, r) = (n / d, n % d);
    if r.abs() * 2 >= d.abs() {
        q + n.signum() * d.signum()
    } else {
        q
    }
}

fn div_half_even(n: i128, d: i128) -> i128 {
    let (q, r) = (n / d, n % d);
    let twice = r.abs() * 2;
    if twice > d.abs() || (twice == d.abs() && q % 2 != 0) {
        q + n.signum() * d.signum()
    } else {
        q
    }
}

/// Divide `n` by `d` and round the result with the given rule.
pub fn div_round(n: i128, d: i128, mode: Rounding) -> Result<i128, MoneyError> {
    if d == 0 {
        return Err(MoneyError::DivideByZero);
    }
    Ok(match mode {
        Rounding::Floor => div_floor(n, d),
        Rounding::Ceil => div_ceil(n, d),
        Rounding::Truncate => n / d,
        Rounding::HalfUp => div_half_up(n, d),
        Rounding::HalfDown => div_half_down(n, d),
        Rounding::HalfEven => div_half_even(n, d),
    })
}

// -- the money type --------------------------------------------------------------------

/// An amount in cents. Negative amounts are credits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Money(pub i64);

fn narrow(v: i128) -> Result<Money, MoneyError> {
    i64::try_from(v).map(Money).map_err(|_| MoneyError::Overflow)
}

fn group_thousands(digits: &str) -> String {
    let mut out = String::new();
    for (i, ch) in digits.chars().enumerate() {
        if (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

impl Money {
    pub const ZERO: Money = Money(0);

    pub fn from_units(units: i64, cents: i64) -> Money {
        Money(units * 100 + cents)
    }

    pub fn cents(self) -> i64 {
        self.0
    }

    pub fn abs(self) -> Money {
        Money(self.0.abs())
    }

    pub fn is_negative(self) -> bool {
        self.0 < 0
    }

    /// Read `12`, `12.5`, `-0.50`, `$1,234.56` or `-$3`. At most two decimals.
    pub fn parse(text: &str) -> Result<Money, MoneyError> {
        let bad = || MoneyError::Parse(text.to_string());
        let t = text.trim();
        let (neg, rest) = match t.strip_prefix('-') {
            Some(r) => (true, r),
            None => (false, t.strip_prefix('+').unwrap_or(t)),
        };
        let rest = rest.replace(',', "");
        let (whole, frac) = rest.split_once('.').unwrap_or((rest.as_str(), ""));
        let digits_only = |s: &str| s.chars().all(|c| c.is_ascii_digit());
        if whole.is_empty() && frac.is_empty() {
            return Err(bad());
        }
        if !digits_only(whole) || !digits_only(frac) || frac.len() > 2 {
            return Err(bad());
        }
        let whole: i128 = if whole.is_empty() { 0 } else { whole.parse().map_err(|_| bad())? };
        let frac: i128 = if frac.is_empty() {
            0
        } else {
            format!("{frac:0<2}").parse().map_err(|_| bad())?
        };
        let cents = whole * 100 + frac;
        narrow(if neg { -cents } else { cents })
    }

    pub fn checked_add(self, other: Money) -> Result<Money, MoneyError> {
        self.0.checked_add(other.0).map(Money).ok_or(MoneyError::Overflow)
    }

    pub fn checked_mul(self, factor: i64) -> Result<Money, MoneyError> {
        self.0.checked_mul(factor).map(Money).ok_or(MoneyError::Overflow)
    }

    /// Multiply by `num / den` and round.
    pub fn ratio(self, num: i64, den: i64, mode: Rounding) -> Result<Money, MoneyError> {
        narrow(div_round(self.0 as i128 * num as i128, den as i128, mode)?)
    }

    /// A share in basis points: 100 is one percent.
    pub fn percent(self, bps: u32, mode: Rounding) -> Money {
        let v = div_round(self.0 as i128 * bps as i128, 10_000, mode).expect("10000 is not zero");
        Money(v as i64)
    }

    /// Split into parts in proportion to the weights. The parts always add up
    /// to the whole. Cents that do not divide go to the parts with the largest
    /// remainder, the earlier part first on a tie.
    pub fn allocate(self, weights: &[u64]) -> Result<Vec<Money>, MoneyError> {
        let sum: i128 = weights.iter().map(|&w| w as i128).sum();
        if weights.is_empty() || sum == 0 {
            return Err(MoneyError::NoWeights);
        }
        let total = self.0 as i128;
        let sign = total.signum();
        let abs = total.abs();
        let mut parts: Vec<i128> = weights.iter().map(|&w| abs * w as i128 / sum).collect();
        let mut remainders: Vec<(usize, i128)> = weights
            .iter()
            .enumerate()
            .map(|(i, &w)| (i, abs * w as i128 % sum))
            .collect();
        let mut left = abs - parts.iter().sum::<i128>();
        for (i, _) in remainders {
            if left == 0 {
                break;
            }
            parts[i] += 1;
            left -= 1;
        }
        Ok(parts.into_iter().map(|p| Money((p * sign) as i64)).collect())
    }

    /// Split into `n` equal parts, the first ones one cent larger when needed.
    pub fn split_even(self, n: usize) -> Result<Vec<Money>, MoneyError> {
        self.allocate(&vec![1; n])
    }
}

impl fmt::Display for Money {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let abs = self.0.unsigned_abs();
        let sign = if self.0 < 0 { "-" } else { "" };
        write!(f, "{}{}.{:02}", sign, group_thousands(&(abs / 100).to_string()), abs % 100)
    }
}

impl Add for Money {
    type Output = Money;
    fn add(self, other: Money) -> Money {
        Money(self.0 + other.0)
    }
}

impl AddAssign for Money {
    fn add_assign(&mut self, other: Money) {
        self.0 += other.0;
    }
}

impl Sub for Money {
    type Output = Money;
    fn sub(self, other: Money) -> Money {
        Money(self.0 - other.0)
    }
}

impl Neg for Money {
    type Output = Money;
    fn neg(self) -> Money {
        Money(-self.0)
    }
}

impl Sum for Money {
    fn sum<I: Iterator<Item = Money>>(iter: I) -> Money {
        iter.fold(Money::ZERO, |a, b| a + b)
    }
}

// -- tax -----------------------------------------------------------------------------------

/// Tax on a net amount.
pub fn tax_on(net: Money, rate_bps: u32, mode: Rounding) -> Money {
    net.percent(rate_bps, mode)
}

/// The net part of a gross amount that already includes tax.
pub fn net_from_gross(gross: Money, rate_bps: u32, mode: Rounding) -> Result<Money, MoneyError> {
    narrow(div_round(gross.0 as i128 * 10_000, 10_000 + rate_bps as i128, mode)?)
}

/// Split a gross amount into (net, tax).
pub fn split_gross(gross: Money, rate_bps: u32, mode: Rounding) -> Result<(Money, Money), MoneyError> {
    let net = net_from_gross(gross, rate_bps, mode)?;
    Ok((net, gross - net))
}

// -- schedules ---------------------------------------------------------------------------------

/// The part of `amount` for `used` out of `total` days, rounded half up.
pub fn prorate(amount: Money, used: u32, total: u32) -> Result<Money, MoneyError> {
    amount.ratio(used as i64, total as i64, Rounding::HalfUp)
}

/// A late fee: `daily_bps` of the amount for each day, simple interest.
pub fn late_fee(amount: Money, days: u32, daily_bps: u32) -> Money {
    amount.percent(daily_bps.saturating_mul(days), Rounding::HalfUp)
}

/// Equal payments that add up to the total, in order, with any extra cents on
/// the earliest payments.
pub fn installments(total: Money, count: usize) -> Result<Vec<Money>, MoneyError> {
    total.split_even(count)
}

/// Volume pricing: the rate for a quantity is the one of the highest tier
/// whose minimum the quantity reaches.
#[derive(Debug, Clone)]
pub struct Tiers {
    tiers: Vec<(u32, u32)>,
}

impl Tiers {
    /// Tiers as (minimum quantity, discount in basis points).
    pub fn new(mut tiers: Vec<(u32, u32)>) -> Tiers {
        tiers.sort();
        Tiers { tiers }
    }

    pub fn discount_bps(&self, quantity: u32) -> u32 {
        self.tiers
            .iter()
            .rev()
            .find(|(min, _)| quantity >= *min)
            .map_or(0, |&(_, bps)| bps)
    }

    /// Price of `quantity` units at `unit` each, after the tier discount.
    pub fn price(&self, unit: Money, quantity: u32, mode: Rounding) -> Result<Money, MoneyError> {
        let gross = unit.checked_mul(quantity as i64)?;
        let off = gross.percent(self.discount_bps(quantity), mode);
        Ok(gross - off)
    }
}

// -- more helpers on amounts -----------------------------------------------------------------------

impl Money {
    /// Round to a multiple of `step` cents, for example 5 for cash rounding.
    pub fn round_to(self, step: i64, mode: Rounding) -> Result<Money, MoneyError> {
        let units = div_round(self.0 as i128, step as i128, mode)?;
        narrow(units * step as i128)
    }

    /// Convert with a rate in millionths: 1_250_000 means 1.25.
    pub fn convert(self, rate_micro: i64, mode: Rounding) -> Result<Money, MoneyError> {
        self.ratio(rate_micro, 1_000_000, mode)
    }

    pub fn min(self, other: Money) -> Money {
        Money(self.0.min(other.0))
    }

    pub fn max(self, other: Money) -> Money {
        Money(self.0.max(other.0))
    }
}

/// The mean of some amounts, rounded with the given rule.
pub fn average(amounts: &[Money], mode: Rounding) -> Result<Money, MoneyError> {
    let total: i128 = amounts.iter().map(|m| m.0 as i128).sum();
    narrow(div_round(total, amounts.len() as i128, mode)?)
}

/// One row of a repayment schedule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Payment {
    pub interest: Money,
    pub principal: Money,
    pub balance: Money,
}

/// Repay equal parts of the principal each month, with interest on what is
/// still owed. `annual_bps` is the yearly rate in basis points.
pub fn straight_line_schedule(
    principal: Money,
    annual_bps: u32,
    months: usize,
    mode: Rounding,
) -> Result<Vec<Payment>, MoneyError> {
    let parts = principal.split_even(months)?;
    let mut balance = principal;
    let mut out = Vec::with_capacity(months);
    for part in parts {
        let interest = div_round(balance.0 as i128 * annual_bps as i128, 10_000 * 12, mode)?;
        balance = balance - part;
        out.push(Payment {
            interest: narrow(interest)?,
            principal: part,
            balance,
        });
    }
    Ok(out)
}

/// Total interest over a schedule.
pub fn total_interest(schedule: &[Payment]) -> Money {
    schedule.iter().map(|p| p.interest).sum()
}

// -- invoices ------------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    pub description: String,
    pub quantity: u32,
    pub unit_price: Money,
    pub tax_bps: u32,
}

impl Line {
    pub fn new(description: &str, quantity: u32, unit_price: Money, tax_bps: u32) -> Line {
        Line {
            description: description.to_string(),
            quantity,
            unit_price,
            tax_bps,
        }
    }

    pub fn net(&self) -> Money {
        Money(self.unit_price.0 * self.quantity as i64)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Discount {
    /// A share of the subtotal in basis points.
    Percent(u32),
    /// A fixed amount, never more than the subtotal.
    Fixed(Money),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Totals {
    pub subtotal: Money,
    pub discount: Money,
    pub tax: Money,
    pub total: Money,
}

#[derive(Debug, Clone)]
pub struct Invoice {
    pub lines: Vec<Line>,
    pub discount: Option<Discount>,
    pub rounding: Rounding,
}

impl Invoice {
    pub fn new(rounding: Rounding) -> Invoice {
        Invoice {
            lines: Vec::new(),
            discount: None,
            rounding,
        }
    }

    pub fn add(&mut self, line: Line) -> &mut Invoice {
        self.lines.push(line);
        self
    }

    pub fn subtotal(&self) -> Money {
        self.lines.iter().map(Line::net).sum()
    }

    /// The discount in cents for a given subtotal.
    pub fn discount_amount(&self, subtotal: Money) -> Money {
        match self.discount {
            None => Money::ZERO,
            Some(Discount::Percent(bps)) => subtotal.percent(bps, self.rounding),
            Some(Discount::Fixed(amount)) => Money(amount.0.min(subtotal.0).max(0)),
        }
    }

    /// Subtotal, discount, tax and total. The discount is spread over the
    /// lines in proportion to their net, and tax is charged per line on what
    /// is left after its share of the discount.
    pub fn totals(&self) -> Result<Totals, MoneyError> {
        let nets: Vec<Money> = self.lines.iter().map(Line::net).collect();
        let subtotal: Money = nets.iter().copied().sum();
        let discount = self.discount_amount(subtotal);
        let mut tax = Money::ZERO;
        if !self.lines.is_empty() {
            let weights: Vec<u64> = nets.iter().map(|m| m.0.max(0) as u64).collect();
            let shares = if discount == Money::ZERO || weights.iter().all(|&w| w == 0) {
                vec![Money::ZERO; nets.len()]
            } else {
                discount.allocate(&weights)?
            };
            for ((line, net), share) in self.lines.iter().zip(&nets).zip(&shares) {
                let taxable = *net;
                tax += tax_on(taxable, line.tax_bps, self.rounding);
            }
        }
        Ok(Totals {
            subtotal,
            discount,
            tax,
            total: subtotal - discount + tax,
        })
    }

    /// Text lines for a printed invoice.
    pub fn render(&self) -> Result<Vec<String>, MoneyError> {
        let mut out = Vec::new();
        for line in &self.lines {
            out.push(format!(
                "{:<20} {:>4} x {:>10} = {:>12}",
                line.description,
                line.quantity,
                line.unit_price.to_string(),
                line.net().to_string()
            ));
        }
        let t = self.totals()?;
        out.push(format!("{:<20} {:>30}", "Subtotal", t.subtotal.to_string()));
        if t.discount != Money::ZERO {
            out.push(format!("{:<20} {:>30}", "Discount", (-t.discount).to_string()));
        }
        out.push(format!("{:<20} {:>30}", "Tax", t.tax.to_string()));
        out.push(format!("{:<20} {:>30}", "Total", t.total.to_string()));
        Ok(out)
    }
}
