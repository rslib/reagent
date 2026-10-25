import datetime
import unittest

from ledger import (Ledger, LedgerError, budget_report, dedupe, format_amount, merge_ledgers,
                    over_budget, parse_amount, parse_budget, parse_date, parse_line,
                    quote_field, reconcile, split_entry, sum_credits, sum_debits,
                    write_line)

D = datetime.date

SAMPLE = """date,account,amount,memo,txn
# opening
2024-01-02,assets:bank,"1,000.00",Opening balance,
2024-01-02,equity:opening,-1000.00,Opening balance,
2024-01-05,expenses:food,42.50,"Market, fresh",t1
2024-01-05,assets:bank,-42.50,"Market, fresh",t1
2024-01-20,expenses:rent,600,January rent,t2
2024-02-01,income:salary,-2500.00,Pay,t3
2024-02-01,assets:bank2,2500.00,Pay,t3
"""


class CsvTest(unittest.TestCase):
    def test_quotes_inside_fields(self):
        self.assertEqual(parse_line('a,"b ""q"" c",d'), ["a", 'b "q" c', "d"])
        self.assertEqual(parse_line('"x,y",,z'), ["x,y", "", "z"])
        self.assertEqual(parse_line(""), [""])

    def test_unterminated_quote(self):
        with self.assertRaises(LedgerError):
            parse_line('a,"b')

    def test_write_round_trip(self):
        fields = ["plain", 'has "quote"', "has,comma", ""]
        self.assertEqual(parse_line(write_line(fields)), fields)
        self.assertEqual(quote_field("ok"), "ok")


class MoneyTest(unittest.TestCase):
    def test_parse_amount(self):
        cases = {"12.5": 1250, "7": 700, "0.05": 5, ".5": 50, "$1,234.5": 123450,
                 "(12.50)": -1250, "-3": -300, "+4.1": 410, "9.99": 999}
        for text, cents in cases.items():
            self.assertEqual(parse_amount(text), cents, text)

    def test_bad_amounts(self):
        for text in ("", "abc", "1.234", "1.2.3", "--4"):
            with self.assertRaises(LedgerError, msg=text):
                parse_amount(text)

    def test_format_amount(self):
        self.assertEqual(format_amount(123456), "1,234.56")
        self.assertEqual(format_amount(-5), "-0.05")
        self.assertEqual(format_amount(100, 8), "    1.00")

    def test_dates(self):
        self.assertEqual(parse_date("2024-02-29"), D(2024, 2, 29))
        with self.assertRaises(LedgerError):
            parse_date("2023-02-29")
        with self.assertRaises(LedgerError):
            parse_date("2023/02/10")


class LoadTest(unittest.TestCase):
    def test_from_csv(self):
        ledger = Ledger.from_csv(SAMPLE)
        self.assertEqual(len(ledger), 7)
        first = ledger.entries[0]
        self.assertEqual((first.amount, first.memo), (100000, "Opening balance"))
        self.assertEqual(ledger.entries[2].memo, "Market, fresh")
        self.assertEqual(ledger.entries[4].amount, 60000)

    def test_columns_in_any_order(self):
        text = "memo,amount,account,date\nhi,5.00,a:b,2024-03-01\n"
        entry = Ledger.from_csv(text).entries[0]
        self.assertEqual((entry.account, entry.amount, entry.memo), ("a:b", 500, "hi"))

    def test_errors_name_the_line(self):
        with self.assertRaises(LedgerError) as caught:
            Ledger.from_csv("date,account,amount\n2024-01-01,a,1\n2024-13-01,a,1\n")
        self.assertIn("line 3", str(caught.exception))
        with self.assertRaises(LedgerError):
            Ledger.from_csv("date,amount\n2024-01-01,1\n")

    def test_csv_round_trip(self):
        ledger = Ledger.from_csv(SAMPLE)
        again = Ledger.from_csv(ledger.to_csv())
        self.assertEqual([e.key() for e in again.entries], [e.key() for e in ledger.entries])


class BalanceTest(unittest.TestCase):
    def setUp(self):
        self.ledger = Ledger.from_csv(SAMPLE)

    def test_single_account(self):
        self.assertEqual(self.ledger.balance("assets:bank"), 100000 - 4250)
        self.assertEqual(self.ledger.balance("assets:bank", upto=D(2024, 1, 2)), 100000)

    def test_rollup_does_not_leak_into_similar_names(self):
        self.assertEqual(self.ledger.rollup("assets:bank"), 95750)
        self.assertEqual(self.ledger.rollup("assets"), 95750 + 250000)
        self.assertEqual(self.ledger.rollup("assets:ban"), 0)

    def test_tree(self):
        tree = self.ledger.tree()
        self.assertEqual(tree["assets"], 345750)
        self.assertEqual(tree["assets:bank"], 95750)
        self.assertEqual(tree["expenses"], 4250 + 60000)

    def test_where_filters(self):
        found = self.ledger.where(root="expenses", text="MARKET")
        self.assertEqual([e.account for e in found], ["expenses:food"])

    def test_debits_and_credits(self):
        self.assertEqual(sum_debits(self.ledger.entries), 414250)
        self.assertEqual(sum_credits(self.ledger.entries), 354250)
        rows, debits, credits = self.ledger.trial_balance()
        self.assertEqual(dict((r[0], r[2]) for r in rows)["assets:bank"], 4250)
        self.assertEqual(debits - credits, sum(e.amount for e in self.ledger.entries))

    def test_running_balance(self):
        rows = self.ledger.running("assets:bank")
        self.assertEqual([total for _, total in rows], [100000, 95750])

    def test_by_month_and_quarter(self):
        self.assertEqual(self.ledger.by_month("expenses"), {"2024-01": 64250})
        self.assertEqual(self.ledger.by_quarter(), {"2024-Q1": 60000})

    def test_income_statement(self):
        stmt = self.ledger.income_statement()
        self.assertEqual(stmt, {"income": 250000, "expenses": 64250, "net": 185750})

    def test_top_accounts(self):
        top = self.ledger.top_accounts("assets", n=1)
        self.assertEqual(top, [("assets:bank2", 250000)])


class CheckTest(unittest.TestCase):
    def test_transfer_is_balanced(self):
        ledger = Ledger()
        txn = ledger.transfer(D(2024, 5, 1), "assets:a", "assets:b", 1500, "move")
        self.assertEqual(ledger.balance("assets:a"), -1500)
        self.assertEqual(ledger.unbalanced(), [])
        self.assertEqual([e.txn for e in ledger.entries], [txn, txn])
        with self.assertRaises(LedgerError):
            ledger.transfer(D(2024, 5, 1), "a", "b", 0)

    def test_unbalanced_and_duplicates(self):
        ledger = Ledger()
        ledger.add(D(2024, 1, 1), "a", 500, "x", "t1")
        ledger.add(D(2024, 1, 1), "b", -400, "x", "t1")
        ledger.add(D(2024, 1, 2), "c", 10, "dup")
        ledger.add(D(2024, 1, 2), "c", 10, "dup")
        self.assertEqual(ledger.unbalanced(), ["t1"])
        self.assertEqual(len(ledger.duplicates()), 1)
        problems = ledger.check()
        self.assertEqual(len(problems), 3)
        self.assertEqual(len(dedupe(ledger)), 3)

    def test_between_includes_both_ends(self):
        ledger = Ledger.from_csv(SAMPLE)
        part = ledger.between(D(2024, 1, 5), D(2024, 1, 20))
        self.assertEqual(len(part), 3)

    def test_split_and_merge(self):
        ledger = Ledger.from_csv(SAMPLE)
        index = 4
        split_entry(ledger, index, [("expenses:rent:base", 50000), ("expenses:rent:fees", 10000)])
        self.assertEqual(ledger.rollup("expenses:rent"), 60000)
        self.assertEqual(len(ledger), 8)
        with self.assertRaises(LedgerError):
            split_entry(ledger, 0, [("a", 1)])
        merged = merge_ledgers(ledger, Ledger.from_csv(SAMPLE))
        self.assertEqual(len(merged), 15)
        self.assertEqual(merged.entries[0].date, D(2024, 1, 2))


class BudgetTest(unittest.TestCase):
    def test_budget_report(self):
        ledger = Ledger.from_csv(SAMPLE)
        budget = parse_budget("expenses:food = 40.00  # groceries\nexpenses:rent = 700\n")
        rows = budget_report(ledger, budget, "2024-01")
        self.assertEqual(rows, [("expenses:food", 4000, 4250, -250),
                                ("expenses:rent", 70000, 60000, 10000)])
        self.assertEqual(over_budget(ledger, budget, "2024-01"), ["expenses:food"])
        self.assertEqual(over_budget(ledger, budget, "2024-02"), [])

    def test_bad_budget_line(self):
        with self.assertRaises(LedgerError):
            parse_budget("expenses:food 40")


class ReconcileTest(unittest.TestCase):
    def test_window_is_inclusive(self):
        ledger = Ledger()
        ledger.add(D(2024, 3, 1), "assets:bank", -2000, "a")
        bank = Ledger()
        bank.add(D(2024, 3, 3), "bank", -2000, "A")
        matched, left, extra = reconcile(ledger.entries, bank.entries, window_days=2)
        self.assertEqual(len(matched), 1)
        self.assertEqual((left, extra), ([], []))
        matched, left, extra = reconcile(ledger.entries, bank.entries, window_days=1)
        self.assertEqual((len(matched), len(left), len(extra)), (0, 1, 1))

    def test_closest_date_wins_and_each_matches_once(self):
        ledger = Ledger()
        ledger.add(D(2024, 3, 10), "x", 500, "first")
        ledger.add(D(2024, 3, 12), "x", 500, "second")
        bank = Ledger()
        bank.add(D(2024, 3, 11), "b", 500)
        bank.add(D(2024, 3, 12), "b", 500)
        matched, left, extra = reconcile(ledger.entries, bank.entries)
        pairs = [(a.memo, b.date.day) for a, b in matched]
        self.assertEqual(pairs, [("first", 11), ("second", 12)])
        self.assertEqual((left, extra), ([], []))


if __name__ == "__main__":
    unittest.main()
