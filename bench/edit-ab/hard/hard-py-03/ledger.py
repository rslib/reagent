"""A small double-entry ledger that reads and writes CSV text.

Money is kept as integer cents. An entry with a positive amount is a debit and
one with a negative amount is a credit. Accounts form a tree with ':' between
the parts, for example 'assets:bank:checking'.
"""

import datetime


class LedgerError(Exception):
    """Raised for text or data the ledger cannot accept."""


# -- money -----------------------------------------------------------------


def parse_amount(text):
    """Read '1,234.50', '-3', '(12.5)' or '$7' as integer cents."""
    raw = text.strip()
    if not raw:
        raise LedgerError("empty amount")
    negative = False
    if raw.startswith("(") and raw.endswith(")"):
        negative = True
        raw = raw[1:-1].strip()
    if raw.startswith("-"):
        negative = not negative
        raw = raw[1:]
    elif raw.startswith("+"):
        raw = raw[1:]
    raw = raw.lstrip("$").replace(",", "").strip()
    whole, _, frac = raw.partition(".")
    if not whole:
        whole = "0"
    if not whole.isdigit() or (frac and not frac.isdigit()):
        raise LedgerError("bad amount %r" % text)
    if len(frac) > 2:
        raise LedgerError("too many decimals in %r" % text)
    cents = int(whole) * 100 + int(frac.zfill(2))
    return -cents if negative else cents


def format_amount(cents, width=0):
    """Format cents as '1,234.50', with '-' for negative amounts."""
    sign = "-" if cents < 0 else ""
    whole, frac = divmod(abs(cents), 100)
    text = "%s%s.%02d" % (sign, format(whole, ","), frac)
    return text.rjust(width)


# -- dates --------------------------------------------------------------------


def parse_date(text):
    """Read YYYY-MM-DD."""
    parts = text.strip().split("-")
    if len(parts) != 3 or not all(p.isdigit() for p in parts):
        raise LedgerError("bad date %r" % text)
    try:
        return datetime.date(int(parts[0]), int(parts[1]), int(parts[2]))
    except ValueError:
        raise LedgerError("bad date %r" % text)


def month_key(day):
    return "%04d-%02d" % (day.year, day.month)


def quarter_key(day):
    return "%04d-Q%d" % (day.year, (day.month - 1) // 3 + 1)


# -- CSV text -------------------------------------------------------------------


def parse_line(line):
    """Split one CSV line. Fields may be quoted and may hold '""' for a quote."""
    fields = []
    buf = []
    in_quotes = False
    i = 0
    while i < len(line):
        ch = line[i]
        if in_quotes:
            if ch == '"':
                if i + 1 < len(line) and line[i + 1] == '"':
                    buf.append('"')
                else:
                    in_quotes = False
            else:
                buf.append(ch)
        elif ch == '"':
            in_quotes = True
        elif ch == ",":
            fields.append("".join(buf))
            buf = []
        else:
            buf.append(ch)
        i += 1
    if in_quotes:
        raise LedgerError("unterminated quote in %r" % line)
    fields.append("".join(buf))
    return fields


def quote_field(text):
    if any(c in text for c in ',"\n'):
        return '"%s"' % text.replace('"', '""')
    return text


def write_line(fields):
    return ",".join(quote_field(str(f)) for f in fields)


# -- entries -----------------------------------------------------------------------


class Entry:
    """One line of the ledger."""

    __slots__ = ("date", "account", "amount", "memo", "txn", "line")

    def __init__(self, date, account, amount, memo="", txn="", line=0):
        self.date = date
        self.account = account
        self.amount = amount
        self.memo = memo
        self.txn = txn
        self.line = line

    def __repr__(self):
        return "Entry(%s %s %s)" % (self.date, self.account, format_amount(self.amount))

    def key(self):
        return (self.date, self.account, self.amount, self.memo)


def account_parts(account):
    return [p for p in account.split(":") if p]


def under(account, root):
    """True when account is root or sits somewhere below it."""
    return account == root or account.startswith(root)


def sum_debits(entries):
    """Total of the positive amounts."""
    total = 0
    for entry in entries:
        if entry.amount > 0:
            total += entry.amount
    return total


def sum_credits(entries):
    """Total of the negative amounts, as a positive number."""
    total = 0
    for entry in entries:
        if entry.amount > 0:
            total -= entry.amount
    return total


class Ledger:
    """A list of entries with the reports one needs on top of them."""

    COLUMNS = ("date", "account", "amount", "memo", "txn")

    def __init__(self):
        self.entries = []
        self._next_txn = 1

    def __len__(self):
        return len(self.entries)

    # -- loading and saving ----------------------------------------------------

    @classmethod
    def from_csv(cls, text):
        """Read a ledger. The first line names the columns, in any order.

        'date', 'account' and 'amount' are required. Blank lines and lines
        that start with '#' are skipped.
        """
        ledger = cls()
        header = None
        for number, line in enumerate(text.splitlines(), start=1):
            if not line.strip() or line.lstrip().startswith("#"):
                continue
            fields = parse_line(line)
            if header is None:
                header = [f.strip().lower() for f in fields]
                for need in ("date", "account", "amount"):
                    if need not in header:
                        raise LedgerError("missing column %r" % need)
                continue
            if len(fields) > len(header):
                raise LedgerError("line %d: too many fields" % number)
            row = dict(zip(header, fields))
            try:
                ledger.add(parse_date(row["date"]), row["account"].strip(),
                           parse_amount(row.get("amount", "")), row.get("memo", "").strip(),
                           row.get("txn", "").strip(), line=number)
            except (LedgerError, KeyError) as error:
                raise LedgerError("line %d: %s" % (number, error))
        return ledger

    def to_csv(self):
        lines = [write_line(self.COLUMNS)]
        for e in self.entries:
            lines.append(write_line([e.date.isoformat(), e.account,
                                     format_amount(e.amount).replace(",", ""), e.memo, e.txn]))
        return "\n".join(lines) + "\n"

    def add(self, date, account, amount, memo="", txn="", line=0):
        if not account_parts(account):
            raise LedgerError("empty account")
        entry = Entry(date, ":".join(account_parts(account)), amount, memo, txn, line)
        self.entries.append(entry)
        return entry

    def transfer(self, date, source, target, amount, memo=""):
        """Move money between two accounts as one balanced transaction."""
        if amount <= 0:
            raise LedgerError("transfer needs a positive amount")
        txn = "t%d" % self._next_txn
        self._next_txn += 1
        self.add(date, source, -amount, memo, txn)
        self.add(date, target, amount, memo, txn)
        return txn

    # -- balances ----------------------------------------------------------------

    def accounts(self):
        return sorted({e.account for e in self.entries})

    def balance(self, account, upto=None):
        """Sum of one account alone, optionally up to and including a date."""
        total = 0
        for e in self.entries:
            if e.account == account and (upto is None or e.date <= upto):
                total += e.amount
        return total

    def balances(self):
        totals = {}
        for e in self.entries:
            totals[e.account] = totals.get(e.account, 0) + e.amount
        return totals

    def rollup(self, root):
        """Balance of root plus every account below it."""
        return sum(e.amount for e in self.entries if under(e.account, root))

    def tree(self):
        """Balance of every account and every parent, as {account: cents}."""
        totals = {}
        for e in self.entries:
            parts = account_parts(e.account)
            for depth in range(1, len(parts) + 1):
                name = ":".join(parts[:depth])
                totals[name] = totals.get(name, 0) + e.amount
        return totals

    def running(self, account):
        """Entries of one account in date order with a running balance."""
        rows = []
        total = 0
        mine = [e for e in self.entries if e.account == account]
        for e in sorted(mine, key=lambda e: (e.date, e.line)):
            total += e.amount
            rows.append((e, total))
        return rows

    # -- slicing -------------------------------------------------------------------

    def between(self, start=None, end=None):
        """A new ledger with the entries from start to end, both included."""
        out = Ledger()
        for e in self.entries:
            if start is not None and e.date < start:
                continue
            if end is not None and e.date > end:
                continue
            out.entries.append(e)
        return out

    def where(self, root=None, text=None):
        """Entries below an account root and whose memo has some text."""
        found = []
        for e in self.entries:
            if root is not None and not under(e.account, root):
                continue
            if text is not None and text.lower() not in e.memo.lower():
                continue
            found.append(e)
        return found

    def by_month(self, root=None):
        """Net amount per month for the entries below root."""
        totals = {}
        for e in self.where(root):
            key = month_key(e.date)
            totals[key] = totals.get(key, 0) + e.amount
        return dict(sorted(totals.items()))

    def by_quarter(self, root=None):
        totals = {}
        for e in self.where(root):
            key = quarter_key(e.date)
            totals[key] = totals.get(key, 0) + e.amount
        return dict(sorted(totals.items()))

    # -- checks ----------------------------------------------------------------------

    def transactions(self):
        """Entries grouped by txn id. Entries without an id are left out."""
        groups = {}
        for e in self.entries:
            if e.txn:
                groups.setdefault(e.txn, []).append(e)
        return groups

    def unbalanced(self):
        """Ids of transactions whose amounts do not add up to zero."""
        bad = []
        for txn, group in sorted(self.transactions().items()):
            if sum(e.amount for e in group) != 0:
                bad.append(txn)
        return bad

    def duplicates(self):
        """Groups of entries that agree on date, account, amount and memo."""
        seen = {}
        for e in self.entries:
            seen.setdefault(e.key(), []).append(e)
        return [group for group in seen.values() if len(group) > 1]

    def check(self):
        """Return a list of problems, empty when the ledger looks sound."""
        problems = []
        for txn in self.unbalanced():
            problems.append("transaction %s does not balance" % txn)
        for group in self.duplicates():
            first = group[0]
            problems.append("%d entries repeat %s %s %s" % (
                len(group), first.date, first.account, format_amount(first.amount)))
        if sum(e.amount for e in self.entries if e.txn) != 0:
            problems.append("transactions together do not sum to zero")
        return problems

    # -- statements -----------------------------------------------------------------------

    def trial_balance(self):
        """Debit and credit totals per account, plus the grand totals."""
        rows = []
        for account in self.accounts():
            mine = [e for e in self.entries if e.account == account]
            rows.append((account, sum_debits(mine), sum_credits(mine)))
        return rows, sum_debits(self.entries), sum_credits(self.entries)

    def income_statement(self):
        """Income and expenses under the 'income' and 'expenses' roots."""
        income = -self.rollup("income")
        expenses = self.rollup("expenses")
        return {"income": income, "expenses": expenses, "net": income - expenses}

    def top_accounts(self, root, n=3):
        """The n accounts below root with the largest absolute balance."""
        totals = {}
        for e in self.entries:
            if under(e.account, root):
                totals[e.account] = totals.get(e.account, 0) + e.amount
        ranked = sorted(totals.items(), key=lambda kv: (-abs(kv[1]), kv[0]))
        return ranked[:n]

    # -- rendering --------------------------------------------------------------------------

    def render_balances(self, width=12):
        """A text table of the tree balances, indented by depth."""
        tree = self.tree()
        lines = []
        for name in sorted(tree):
            depth = len(account_parts(name)) - 1
            label = "  " * depth + account_parts(name)[-1]
            lines.append("%-28s %s" % (label, format_amount(tree[name], width)))
        return "\n".join(lines)

    def render_running(self, account):
        lines = []
        for entry, total in self.running(account):
            lines.append("%s %s %s %s" % (
                entry.date.isoformat(), format_amount(entry.amount, 10),
                format_amount(total, 10), entry.memo))
        return "\n".join(lines)


# -- budgets -------------------------------------------------------------------------------


def parse_budget(text):
    """Read lines like 'expenses:food = 400.00' into {account: cents}."""
    budget = {}
    for number, line in enumerate(text.splitlines(), start=1):
        line = line.split("#", 1)[0].strip()
        if not line:
            continue
        name, sep, amount = line.partition("=")
        if not sep:
            raise LedgerError("budget line %d: expected 'account = amount'" % number)
        budget[":".join(account_parts(name.strip()))] = parse_amount(amount)
    return budget


def budget_report(ledger, budget, month):
    """Rows of (root, budgeted, spent, remaining) for one 'YYYY-MM' month.

    Spent is the net debit total of the entries at or below the root.
    """
    rows = []
    for root in sorted(budget):
        spent = 0
        for e in ledger.entries:
            if under(e.account, root) and month_key(e.date) == month:
                spent += e.amount
        rows.append((root, budget[root], spent, budget[root] - spent))
    return rows


def over_budget(ledger, budget, month):
    """Roots whose spending in the month is above the budget."""
    return [row[0] for row in budget_report(ledger, budget, month) if row[3] < 0]


def render_budget(rows):
    lines = []
    for root, planned, spent, left in rows:
        flag = "  OVER" if left < 0 else ""
        lines.append("%-24s %10s %10s %10s%s" % (
            root, format_amount(planned), format_amount(spent), format_amount(left), flag))
    return "\n".join(lines)


def merge_ledgers(first, second):
    """A new ledger with the entries of both, in date order."""
    out = Ledger()
    out.entries = sorted(first.entries + second.entries, key=lambda e: (e.date, e.line))
    return out


def dedupe(ledger):
    """A new ledger that keeps the first entry of each group of repeats."""
    out = Ledger()
    seen = set()
    for e in ledger.entries:
        if e.key() in seen:
            continue
        seen.add(e.key())
        out.entries.append(e)
    return out


def split_entry(ledger, index, parts):
    """Replace one entry by several that add up to the same amount.

    parts is a list of (account, cents). The entry keeps its date and memo.
    """
    entry = ledger.entries[index]
    if sum(cents for _, cents in parts) != entry.amount:
        raise LedgerError("parts do not add up to %s" % format_amount(entry.amount))
    txn = entry.txn or "s%d" % ledger._next_txn
    ledger._next_txn += 1
    del ledger.entries[index]
    for offset, (account, cents) in enumerate(parts):
        parsed = Entry(entry.date, ":".join(account_parts(account)), cents,
                       entry.memo, txn, entry.line)
        ledger.entries.insert(index + offset, parsed)
    return txn


# -- reconciliation -----------------------------------------------------------------------


def reconcile(ledger_entries, statement_entries, window_days=2):
    """Match ledger entries to bank statement entries.

    Two entries match when the amounts are equal and the dates are at most
    window_days apart. Each entry matches once, the closest date first.
    Returns (matched pairs, unmatched ledger entries, unmatched statement entries).
    """
    free = list(statement_entries)
    matched = []
    left = []
    for entry in sorted(ledger_entries, key=lambda e: (e.date, e.line)):
        best = None
        for other in free:
            gap = abs((entry.date - other.date).days)
            if other.amount == entry.amount and gap < window_days:
                if best is None or gap < best[0]:
                    best = (gap, other)
        if best is None:
            left.append(entry)
        else:
            free.remove(best[1])
            matched.append((entry, best[1]))
    return matched, left, free
