use bank::{Bank, BankError, DEFAULT_RATE_BP};

fn sample() -> Bank {
    let mut b = Bank::new();
    b.open("ann", 10_000);
    b.open("bob", 500);
    b
}

#[test]
fn deposit_and_withdraw() {
    let mut b = sample();
    assert_eq!(b.deposit("ann", 250), Ok(10_250));
    assert_eq!(b.withdraw("ann", 250), Ok(10_000));
}

#[test]
fn withdraw_everything() {
    let mut b = sample();
    assert_eq!(b.withdraw("bob", 500), Ok(0));
}

#[test]
fn withdraw_too_much() {
    let mut b = sample();
    assert_eq!(
        b.withdraw("bob", 501),
        Err(BankError::InsufficientFunds { needed: 501, available: 500 })
    );
}

#[test]
fn transfer_moves_money_from_the_sender() {
    let mut b = sample();
    assert_eq!(b.transfer("ann", "bob", 1_000), Ok(()));
    assert_eq!(b.balance("ann"), Ok(9_000));
    assert_eq!(b.balance("bob"), Ok(1_500));
}

#[test]
fn failed_transfer_changes_nothing() {
    let mut b = sample();
    assert!(b.transfer("bob", "ann", 600).is_err());
    assert_eq!(b.balance("ann"), Ok(10_000));
    assert_eq!(b.balance("bob"), Ok(500));
    assert_eq!(b.transfer("ann", "ann", 1), Err(BankError::SameAccount));
    assert_eq!(b.transfer("ann", "zed", 1), Err(BankError::NoSuchAccount("zed".into())));
}

#[test]
fn default_interest_is_two_and_a_half_percent() {
    let mut b = sample();
    b.add_interest(DEFAULT_RATE_BP);
    assert_eq!(b.balance("ann"), Ok(10_250));
    assert_eq!(b.balance("bob"), Ok(512));
    assert_eq!(DEFAULT_RATE_BP, 250);
}

#[test]
fn totals_and_richest() {
    let mut b = sample();
    assert_eq!(b.total(), 10_500);
    assert_eq!(b.richest(), Some(("ann", 10_000)));
    assert_eq!(b.close("ann"), Ok(10_000));
    assert_eq!(b.names(), vec!["bob"]);
}
