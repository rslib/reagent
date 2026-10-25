use invoicing::*;

fn m(text: &str) -> Money {
    Money::parse(text).unwrap()
}

#[test]
fn parsing_amounts() {
    assert_eq!(m("12"), Money(1200));
    assert_eq!(m("12.5"), Money(1250));
    assert_eq!(m("12.05"), Money(1205));
    assert_eq!(m("0.99"), Money(99));
    assert_eq!(m(".5"), Money(50));
    assert_eq!(m("  7.00 "), Money(700));
    assert_eq!(m("+3"), Money(300));
    assert_eq!(m("-0.50"), Money(-50));
    assert_eq!(m("-12.34"), Money(-1234));
}

#[test]
fn dollar_signs_and_commas() {
    assert_eq!(m("$1,234.56"), Money(123456));
    assert_eq!(m("-$3"), Money(-300));
    assert_eq!(m("$0.07"), Money(7));
    assert_eq!(m("1,000,000"), Money(100_000_000));
}

#[test]
fn bad_amounts() {
    for bad in ["", "abc", "1.234", "1.2.3", "--1", "$", ".", "1 000", "12a"] {
        assert!(matches!(Money::parse(bad), Err(MoneyError::Parse(_))), "{bad:?}");
    }
    assert_eq!(Money::parse("99999999999999999999"), Err(MoneyError::Overflow));
}

#[test]
fn display_groups_thousands() {
    assert_eq!(Money(0).to_string(), "0.00");
    assert_eq!(Money(5).to_string(), "0.05");
    assert_eq!(Money(-5).to_string(), "-0.05");
    assert_eq!(Money(123456).to_string(), "1,234.56");
    assert_eq!(Money(-123456).to_string(), "-1,234.56");
    assert_eq!(Money(100_000_00).to_string(), "100,000.00");
    assert_eq!(Money(12_345_678_90).to_string(), "12,345,678.90");
}

#[test]
fn display_of_whole_groups_has_no_leading_comma() {
    assert_eq!(Money(999_00).to_string(), "999.00");
    assert_eq!(Money(123_456_00).to_string(), "123,456.00");
    assert_eq!(Money(-123_456_00).to_string(), "-123,456.00");
    assert_eq!(Money(123_456_789_00).to_string(), "123,456,789.00");
    for cents in [100_00, 999_99, 1_000_00, 123_456_00, 999_999_99] {
        assert_eq!(Money::parse(&Money(cents).to_string()), Ok(Money(cents)));
    }
}

#[test]
fn arithmetic() {
    assert_eq!(Money(150) + Money(275), Money(425));
    assert_eq!(Money(150) - Money(275), Money(-125));
    assert_eq!(-Money(5), Money(-5));
    assert_eq!([Money(1), Money(2), Money(3)].into_iter().sum::<Money>(), Money(6));
    assert_eq!(Money::from_units(3, 5), Money(305));
    assert_eq!(Money(i64::MAX).checked_add(Money(1)), Err(MoneyError::Overflow));
    assert_eq!(Money(10).min(Money(3)), Money(3));
    assert_eq!(Money(-10).abs(), Money(10));
}

#[test]
fn halves_go_the_documented_way() {
    assert_eq!(div_round(5, 2, Rounding::HalfUp), Ok(3));
    assert_eq!(div_round(-5, 2, Rounding::HalfUp), Ok(-3));
    assert_eq!(div_round(5, 2, Rounding::HalfDown), Ok(2));
    assert_eq!(div_round(-5, 2, Rounding::HalfDown), Ok(-2));
    assert_eq!(div_round(5, 2, Rounding::HalfEven), Ok(2));
    assert_eq!(div_round(7, 2, Rounding::HalfEven), Ok(4));
    assert_eq!(div_round(-5, 2, Rounding::HalfEven), Ok(-2));
}

#[test]
fn half_down_leaves_a_tie_alone_and_rounds_the_rest() {
    assert_eq!(div_round(25, 10, Rounding::HalfDown), Ok(2));
    assert_eq!(div_round(26, 10, Rounding::HalfDown), Ok(3));
    assert_eq!(div_round(24, 10, Rounding::HalfDown), Ok(2));
    assert_eq!(div_round(-25, 10, Rounding::HalfDown), Ok(-2));
    assert_eq!(div_round(-26, 10, Rounding::HalfDown), Ok(-3));
    assert_eq!(div_round(3, 2, Rounding::HalfDown), Ok(1));
}

#[test]
fn ceil_floor_and_truncate() {
    assert_eq!(div_round(7, 2, Rounding::Floor), Ok(3));
    assert_eq!(div_round(-7, 2, Rounding::Floor), Ok(-4));
    assert_eq!(div_round(7, 2, Rounding::Ceil), Ok(4));
    assert_eq!(div_round(-7, 2, Rounding::Ceil), Ok(-3));
    assert_eq!(div_round(-7, 2, Rounding::Truncate), Ok(-3));
    assert_eq!(div_round(8, 2, Rounding::Ceil), Ok(4));
}

#[test]
fn percentages_in_basis_points() {
    assert_eq!(Money(10_000).percent(825, Rounding::HalfUp), Money(825));
    assert_eq!(Money(1999).percent(825, Rounding::HalfUp), Money(165));
    assert_eq!(Money(1999).percent(825, Rounding::Floor), Money(164));
    assert_eq!(Money(1999).percent(825, Rounding::Ceil), Money(165));
    assert_eq!(Money(-1999).percent(825, Rounding::HalfUp), Money(-165));
}

#[test]
fn allocation_keeps_every_cent() {
    assert_eq!(Money(100).allocate(&[1, 1, 1]).unwrap(), vec![Money(34), Money(33), Money(33)]);
    assert_eq!(Money(100).allocate(&[70, 25, 5]).unwrap(), vec![Money(70), Money(25), Money(5)]);
    assert_eq!(Money(-100).allocate(&[1, 1, 1]).unwrap(), vec![Money(-34), Money(-33), Money(-33)]);
    assert_eq!(Money(5).allocate(&[1, 0, 1]).unwrap(), vec![Money(3), Money(0), Money(2)]);
    assert_eq!(Money(5).allocate(&[]), Err(MoneyError::NoWeights));
    assert_eq!(Money(5).allocate(&[0, 0]), Err(MoneyError::NoWeights));
}

#[test]
fn leftover_cents_go_to_the_largest_remainders() {
    assert_eq!(Money(5).allocate(&[2, 3, 5]).unwrap(), vec![Money(1), Money(2), Money(2)]);
    assert_eq!(Money(10).allocate(&[1, 1, 8, 0]).unwrap(), vec![Money(1), Money(1), Money(8), Money(0)]);
    assert_eq!(Money(8).allocate(&[1, 2, 4]).unwrap(), vec![Money(1), Money(2), Money(5)]);
    assert_eq!(Money(2).allocate(&[1, 9, 5]).unwrap(), vec![Money(0), Money(1), Money(1)]);
}

#[test]
fn allocation_always_adds_up() {
    let mut seed = 99u64;
    let mut next = move |n: u64| {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
        (seed >> 33) % n
    };
    for _ in 0..200 {
        let total = Money(next(100_000) as i64 - 50_000);
        let weights: Vec<u64> = (0..1 + next(6)).map(|_| 1 + next(50)).collect();
        let parts = total.allocate(&weights).unwrap();
        assert_eq!(parts.iter().copied().sum::<Money>(), total);
        assert_eq!(parts.len(), weights.len());
    }
}

#[test]
fn even_splits_and_installments() {
    assert_eq!(Money(1000).split_even(3).unwrap(), vec![Money(334), Money(333), Money(333)]);
    assert_eq!(installments(Money(1001), 4).unwrap(), vec![Money(251), Money(250), Money(250), Money(250)]);
    assert_eq!(Money(10).split_even(0), Err(MoneyError::NoWeights));
}

#[test]
fn tax_and_gross_amounts() {
    assert_eq!(tax_on(Money(1999), 2000, Rounding::HalfUp), Money(400));
    assert_eq!(net_from_gross(Money(12000), 2000, Rounding::HalfUp), Ok(Money(10000)));
    assert_eq!(split_gross(Money(1999), 825, Rounding::HalfUp), Ok((Money(1847), Money(152))));
    assert_eq!(split_gross(Money(-1200), 2000, Rounding::HalfUp), Ok((Money(-1000), Money(-200))));
}

#[test]
fn schedules() {
    assert_eq!(prorate(Money(3000), 10, 30), Ok(Money(1000)));
    assert_eq!(prorate(Money(1000), 1, 3), Ok(Money(333)));
    assert_eq!(prorate(Money(1000), 2, 3), Ok(Money(667)));
    assert_eq!(prorate(Money(1000), 1, 0), Err(MoneyError::DivideByZero));
    assert_eq!(late_fee(Money(10_000), 10, 5), Money(50));
    assert_eq!(late_fee(Money(333), 3, 50), Money(5));
}

#[test]
fn rounding_to_a_step_and_converting() {
    assert_eq!(Money(1003).round_to(5, Rounding::HalfUp), Ok(Money(1005)));
    assert_eq!(Money(1002).round_to(5, Rounding::HalfUp), Ok(Money(1000)));
    assert_eq!(Money(1002).round_to(5, Rounding::Ceil), Ok(Money(1005)));
    assert_eq!(Money(1000).convert(1_250_000, Rounding::HalfUp), Ok(Money(1250)));
    assert_eq!(Money(999).convert(920_000, Rounding::HalfUp), Ok(Money(919)));
    assert_eq!(average(&[Money(100), Money(101)], Rounding::HalfUp), Ok(Money(101)));
    assert_eq!(average(&[], Rounding::HalfUp), Err(MoneyError::DivideByZero));
}

#[test]
fn repayment_schedule() {
    let plan = straight_line_schedule(Money(120_000), 1200, 4, Rounding::HalfUp).unwrap();
    assert_eq!(plan.len(), 4);
    assert_eq!(plan[0], Payment { interest: Money(1200), principal: Money(30_000), balance: Money(90_000) });
    assert_eq!(plan[3].balance, Money(0));
    assert_eq!(total_interest(&plan), Money(1200 + 900 + 600 + 300));
}

#[test]
fn volume_tiers() {
    let tiers = Tiers::new(vec![(100, 1000), (10, 500), (50, 750)]);
    assert_eq!(tiers.discount_bps(1), 0);
    assert_eq!(tiers.discount_bps(10), 500);
    assert_eq!(tiers.discount_bps(49), 500);
    assert_eq!(tiers.discount_bps(50), 750);
    assert_eq!(tiers.discount_bps(1000), 1000);
    assert_eq!(tiers.price(Money(200), 10, Rounding::HalfUp), Ok(Money(1900)));
}

fn sample() -> Invoice {
    let mut inv = Invoice::new(Rounding::HalfUp);
    inv.add(Line::new("widget", 3, Money(1999), 2000));
    inv.add(Line::new("gadget", 1, Money(5000), 1000));
    inv.add(Line::new("book", 2, Money(1250), 0));
    inv
}

#[test]
fn invoice_without_a_discount() {
    let t = sample().totals().unwrap();
    assert_eq!(t.subtotal, Money(5997 + 5000 + 2500));
    assert_eq!(t.discount, Money(0));
    assert_eq!(t.tax, Money(1199 + 500));
    assert_eq!(t.total, Money(13497 + 1699));
}

#[test]
fn a_discount_lowers_the_taxable_amount() {
    let mut inv = sample();
    inv.discount = Some(Discount::Percent(1000));
    let t = inv.totals().unwrap();
    assert_eq!(t.discount, Money(1350));
    // shares of 1350 over nets 5997, 5000, 2500 are 600, 500, 250
    let widget_tax = Money(5997 - 600).percent(2000, Rounding::HalfUp);
    let gadget_tax = Money(5000 - 500).percent(1000, Rounding::HalfUp);
    assert_eq!(t.tax, widget_tax + gadget_tax);
    assert_eq!(t.tax, Money(1529));
    assert_eq!(t.total, Money(13497) - Money(1350) + t.tax);
}

#[test]
fn fixed_discounts_are_capped_at_the_subtotal() {
    let mut inv = Invoice::new(Rounding::HalfUp);
    inv.add(Line::new("one", 1, Money(1000), 2000));
    inv.discount = Some(Discount::Fixed(Money(5000)));
    let t = inv.totals().unwrap();
    assert_eq!(t.discount, Money(1000));
    assert_eq!(t.tax, Money(0));
    assert_eq!(t.total, Money(0));
    inv.discount = Some(Discount::Fixed(Money(250)));
    let t = inv.totals().unwrap();
    assert_eq!(t.tax, Money(150));
    assert_eq!(t.total, Money(1000 - 250 + 150));
}

#[test]
fn an_empty_invoice_and_rendering() {
    let inv = Invoice::new(Rounding::HalfUp);
    let t = inv.totals().unwrap();
    assert_eq!((t.subtotal, t.tax, t.total), (Money(0), Money(0), Money(0)));
    let text = sample().render().unwrap();
    assert_eq!(text.len(), 6);
    assert!(text[0].starts_with("widget"));
    assert!(text[5].trim_end().ends_with("151.96"));
}
