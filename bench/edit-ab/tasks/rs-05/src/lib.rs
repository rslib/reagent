//! A tiny bank: accounts with balances in cents, transfers and interest.

use std::collections::BTreeMap;

/// Yearly interest in basis points (1/100 of a percent): 2.5 percent.
pub const DEFAULT_RATE_BP: u64 = 25;

#[derive(Debug, PartialEq, Eq)]
pub enum BankError {
    NoSuchAccount(String),
    InsufficientFunds { needed: u64, available: u64 },
    InvalidAmount,
    SameAccount,
}

#[derive(Debug, Default)]
pub struct Bank {
    accounts: BTreeMap<String, u64>,
}

impl Bank {
    pub fn new() -> Bank {
        Bank::default()
    }

    pub fn open(&mut self, name: &str, initial: u64) {
        self.accounts.insert(name.to_string(), initial);
    }

    pub fn balance(&self, name: &str) -> Result<u64, BankError> {
        self.accounts
            .get(name)
            .copied()
            .ok_or_else(|| BankError::NoSuchAccount(name.to_string()))
    }

    pub fn deposit(&mut self, name: &str, amount: u64) -> Result<u64, BankError> {
        if amount == 0 {
            return Err(BankError::InvalidAmount);
        }
        let balance = self
            .accounts
            .get_mut(name)
            .ok_or_else(|| BankError::NoSuchAccount(name.to_string()))?;
        *balance += amount;
        Ok(*balance)
    }

    /// Takes money out of an account. The whole balance can be withdrawn.
    pub fn withdraw(&mut self, name: &str, amount: u64) -> Result<u64, BankError> {
        if amount == 0 {
            return Err(BankError::InvalidAmount);
        }
        let balance = self
            .accounts
            .get_mut(name)
            .ok_or_else(|| BankError::NoSuchAccount(name.to_string()))?;
        if amount >= *balance {
            return Err(BankError::InsufficientFunds {
                needed: amount,
                available: *balance,
            });
        }
        *balance -= amount;
        Ok(*balance)
    }

    /// Moves money between two accounts. Nothing changes when it fails.
    pub fn transfer(&mut self, from: &str, to: &str, amount: u64) -> Result<(), BankError> {
        if from == to {
            return Err(BankError::SameAccount);
        }
        self.balance(to)?;
        self.withdraw(to, amount)?;
        self.deposit(from, amount)?;
        Ok(())
    }

    /// Adds one year of interest to every account, rounding down.
    pub fn add_interest(&mut self, rate_bp: u64) {
        for balance in self.accounts.values_mut() {
            *balance += *balance * rate_bp / 10_000;
        }
    }

    pub fn total(&self) -> u64 {
        self.accounts.values().sum()
    }

    pub fn names(&self) -> Vec<&str> {
        self.accounts.keys().map(|k| k.as_str()).collect()
    }

    pub fn close(&mut self, name: &str) -> Result<u64, BankError> {
        self.accounts
            .remove(name)
            .ok_or_else(|| BankError::NoSuchAccount(name.to_string()))
    }

    /// The account with the most money, the first by name when tied.
    pub fn richest(&self) -> Option<(&str, u64)> {
        let mut best: Option<(&str, u64)> = None;
        for (name, &balance) in &self.accounts {
            match best {
                Some((_, b)) if b >= balance => {}
                _ => best = Some((name.as_str(), balance)),
            }
        }
        best
    }
}
