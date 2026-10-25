"""A limit order book with price-time priority matching.

Prices are integer ticks and quantities are integer units, so every number in
the book is exact. Orders on the same side and price fill in arrival order.
"""

import bisect
from collections import deque

BUY = "buy"
SELL = "sell"
GTC = "gtc"
IOC = "ioc"
FOK = "fok"
SIDES = (BUY, SELL)
TIFS = (GTC, IOC, FOK)

OPEN = "open"
FILLED = "filled"
CANCELLED = "cancelled"
REJECTED = "rejected"


class BookError(Exception):
    """Raised for an order the book cannot accept."""


class Order:
    """One order. `remaining` falls as the order fills."""

    __slots__ = ("id", "side", "price", "qty", "remaining", "owner", "seq", "tif", "status")

    def __init__(self, order_id, side, price, qty, owner, seq, tif):
        self.id = order_id
        self.side = side
        self.price = price
        self.qty = qty
        self.remaining = qty
        self.owner = owner
        self.seq = seq
        self.tif = tif
        self.status = OPEN

    def __repr__(self):
        return "Order(%s %s %s@%s rem=%s %s)" % (
            self.id, self.side, self.qty, self.price, self.remaining, self.status)

    @property
    def filled(self):
        return self.qty - self.remaining


class Trade:
    """One execution between a resting (maker) and an incoming (taker) order."""

    __slots__ = ("seq", "price", "qty", "maker_id", "taker_id", "maker_owner",
                 "taker_owner", "taker_side")

    def __init__(self, seq, price, qty, maker, taker):
        self.seq = seq
        self.price = price
        self.qty = qty
        self.maker_id = maker.id
        self.taker_id = taker.id
        self.maker_owner = maker.owner
        self.taker_owner = taker.owner
        self.taker_side = taker.side

    @property
    def notional(self):
        return self.price * self.qty

    def __repr__(self):
        return "Trade(#%s %s@%s maker=%s taker=%s)" % (
            self.seq, self.qty, self.price, self.maker_id, self.taker_id)


class Level:
    """All resting orders at one price on one side, oldest first."""

    def __init__(self, price):
        self.price = price
        self.orders = deque()
        self.total = 0

    def __len__(self):
        return len(self.orders)

    def add(self, order):
        self.orders.append(order)
        self.total += order.remaining

    def front(self):
        return self.orders[0]

    def pop_front(self):
        order = self.orders.popleft()
        self.total -= order.remaining
        return order

    def remove(self, order):
        self.orders.remove(order)
        self.total -= order.remaining

    def reduce(self, order, amount):
        """Shrink an order that stays in the queue."""
        order.remaining -= amount
        self.total -= order.remaining


class OrderBook:
    """Matches incoming orders against the resting book.

    `maker_bps` and `taker_bps` are fees in basis points of the notional.
    """

    def __init__(self, tick=1, maker_bps=0, taker_bps=10, max_qty=1000000):
        if tick < 1:
            raise BookError("tick must be at least 1")
        self.tick = tick
        self.maker_bps = maker_bps
        self.taker_bps = taker_bps
        self.max_qty = max_qty
        self._bids = {}
        self._asks = {}
        self._bid_prices = []
        self._ask_prices = []
        self._orders = {}
        self._stops = []
        self._next_id = 1
        self._seq = 0
        self._trade_seq = 0
        self.trades = []
        self.history = []

    # -- validation and ids -------------------------------------------------

    def _new_id(self):
        order_id = "o%d" % self._next_id
        self._next_id += 1
        return order_id

    def _validate(self, side, price, qty, tif):
        if side not in SIDES:
            raise BookError("unknown side %r" % (side,))
        if tif not in TIFS:
            raise BookError("unknown time in force %r" % (tif,))
        if not isinstance(qty, int) or qty <= 0:
            raise BookError("quantity must be a positive integer")
        if qty > self.max_qty:
            raise BookError("quantity above the limit")
        if price is not None:
            if not isinstance(price, int) or price <= 0:
                raise BookError("price must be a positive integer")
            if price % self.tick != 0:
                raise BookError("price is not on the tick grid")

    def _log(self, event, order):
        self.history.append((event, order.id, order.remaining))

    # -- price ladders --------------------------------------------------------

    def _levels(self, side):
        return self._bids if side == BUY else self._asks

    def _prices(self, side):
        return self._bid_prices if side == BUY else self._ask_prices

    def _insert_price(self, side, price):
        prices = self._prices(side)
        index = bisect.bisect_left(prices, price)
        if index == len(prices) or prices[index] != price:
            prices.insert(index, price)

    def _drop_price(self, side, price):
        prices = self._prices(side)
        index = bisect.bisect_left(prices, price)
        if index < len(prices) and prices[index] == price:
            del prices[index]
        del self._levels(side)[price]

    def _best_price(self, side):
        prices = self._prices(side)
        if not prices:
            return None
        return prices[-1] if side == BUY else prices[0]

    def best_bid(self):
        return self._best_price(BUY)

    def best_ask(self):
        return self._best_price(SELL)

    def spread(self):
        bid, ask = self.best_bid(), self.best_ask()
        if bid is None or ask is None:
            return None
        return ask - bid

    def mid(self):
        bid, ask = self.best_bid(), self.best_ask()
        if bid is None or ask is None:
            return None
        return (bid + ask) / 2

    # -- entering orders ------------------------------------------------------

    def add_limit(self, side, price, qty, owner="", tif=GTC):
        """Add a limit order. Returns (order, trades)."""
        self._validate(side, price, qty, tif)
        self._seq += 1
        order = Order(self._new_id(), side, price, qty, owner, self._seq, tif)
        if tif == FOK and self._available(side, price) < qty:
            order.status = REJECTED
            self._log("reject", order)
            return order, []
        self._orders[order.id] = order
        trades = self._match(order, price)
        if order.remaining == 0:
            order.status = FILLED
            del self._orders[order.id]
            self._log("filled", order)
        elif tif == GTC:
            self._rest(order)
        else:
            order.status = CANCELLED
            del self._orders[order.id]
            self._log("expire", order)
        trades += self._trigger_stops()
        return order, trades

    def add_market(self, side, qty, owner=""):
        """Take liquidity at any price. The unfilled rest is dropped."""
        self._validate(side, None, qty, IOC)
        self._seq += 1
        order = Order(self._new_id(), side, None, qty, owner, self._seq, IOC)
        self._orders[order.id] = order
        trades = self._match(order, None)
        del self._orders[order.id]
        order.status = FILLED if order.remaining == 0 else CANCELLED
        self._log(order.status, order)
        trades += self._trigger_stops()
        return order, trades

    def _available(self, side, price):
        """Units an incoming order could take right now at its limit."""
        if side == BUY:
            return self.cumulative_ask_qty(price)
        return self.cumulative_bid_qty(price)

    def _crosses(self, order, limit, level_price):
        if limit is None:
            return True
        if order.side == BUY:
            return level_price < limit
        return level_price >= limit

    def _match(self, order, limit):
        opposite = SELL if order.side == BUY else BUY
        trades = []
        while order.remaining > 0:
            best = self._best_price(opposite)
            if best is None or not self._crosses(order, limit, best):
                break
            level = self._levels(opposite)[best]
            while order.remaining > 0 and len(level) > 0:
                maker = level.front()
                qty = min(order.remaining, maker.remaining)
                self._trade_seq += 1
                trade = Trade(self._trade_seq, best, qty, maker, order)
                trades.append(trade)
                self.trades.append(trade)
                order.remaining -= qty
                if qty == maker.remaining:
                    level.pop_front()
                    maker.remaining = 0
                    maker.status = FILLED
                    del self._orders[maker.id]
                    self._log("filled", maker)
                else:
                    level.reduce(maker, qty)
                    self._log("partial", maker)
            if len(level) == 0:
                self._drop_price(opposite, best)
        return trades

    def _rest(self, order):
        levels = self._levels(order.side)
        level = levels.get(order.price)
        if level is None:
            level = Level(order.price)
            levels[order.price] = level
            self._insert_price(order.side, order.price)
        level.add(order)
        self._log("rest", order)

    # -- stop orders ----------------------------------------------------------

    def add_stop(self, side, stop_price, qty, owner=""):
        """Hold a market order until the last trade price reaches stop_price.

        A buy stop fires at or above the stop price, a sell stop at or below.
        """
        self._validate(side, stop_price, qty, IOC)
        self._seq += 1
        order = Order(self._new_id(), side, stop_price, qty, owner, self._seq, IOC)
        self._stops.append(order)
        self._log("stop", order)
        return order

    def pending_stops(self):
        return sorted(self._stops, key=lambda o: o.seq)

    def _stop_fires(self, order, last):
        if order.side == BUY:
            return last >= order.price
        return last <= order.price

    def _trigger_stops(self):
        """Fire every stop the last trade reached. Returns the new trades."""
        fired_trades = []
        while self._stops and self.trades:
            last = self.trades[-1].price
            ready = [o for o in self._stops if self._stop_fires(o, last)]
            if not ready:
                break
            order = min(ready, key=lambda o: o.seq)
            self._stops.remove(order)
            self._orders[order.id] = order
            fired_trades += self._match(order, None)
            del self._orders[order.id]
            order.status = FILLED if order.remaining == 0 else CANCELLED
            self._log(order.status, order)
        return fired_trades

    # -- changing and removing orders -----------------------------------------

    def get(self, order_id):
        try:
            return self._orders[order_id]
        except KeyError:
            raise BookError("no open order %s" % order_id)

    def cancel(self, order_id):
        """Remove a resting order and return it."""
        order = self.get(order_id)
        level = self._levels(order.side)[order.price]
        level.remove(order)
        del self._orders[order.id]
        order.status = CANCELLED
        self._log("cancel", order)
        return order

    def modify(self, order_id, qty=None, price=None):
        """Change an order. A smaller quantity keeps its place in the queue.

        A larger quantity or a new price sends the order to the back.
        """
        order = self.get(order_id)
        if qty is None and price is None:
            raise BookError("nothing to change")
        new_qty = order.remaining if qty is None else qty
        new_price = order.price if price is None else price
        self._validate(order.side, new_price, new_qty, order.tif)
        if new_price == order.price and new_qty <= order.remaining:
            level = self._levels(order.side)[order.price]
            level.reduce(order, order.remaining - new_qty)
            order.qty -= order.remaining - new_qty
            order.remaining = new_qty
            self._log("shrink", order)
            return order, []
        self.cancel(order_id)
        return self.add_limit(order.side, new_price, new_qty, order.owner, order.tif)

    def cancel_owner(self, owner):
        """Cancel every resting order of one owner. Returns the count."""
        mine = [o.id for o in self._orders.values() if o.owner == owner]
        for order_id in mine:
            self.cancel(order_id)
        return len(mine)

    # -- reading the book -----------------------------------------------------

    def depth(self, side, n=5):
        """Up to n best price levels as (price, total quantity) pairs."""
        prices = self._prices(side)
        ordered = reversed(prices) if side == BUY else iter(prices)
        out = []
        for price in ordered:
            if len(out) == n:
                break
            out.append((price, self._levels(side)[price].total))
        return out

    def volume_at(self, side, price):
        level = self._levels(side).get(price)
        return level.total if level else 0

    def queue_position(self, order_id):
        """Units ahead of this order at its price level."""
        order = self.get(order_id)
        ahead = 0
        for other in self._levels(order.side)[order.price].orders:
            if other.id == order.id:
                return ahead
            ahead += other.remaining
        raise BookError("order is not queued")

    def orders_for(self, owner):
        found = [o for o in self._orders.values() if o.owner == owner]
        return sorted(found, key=lambda o: o.seq)

    def open_count(self):
        return len(self._orders)

    def cumulative_bid_qty(self, limit_price):
        """Total bid quantity at prices of at least limit_price."""
        total = 0
        for price in self._bid_prices:
            if price >= limit_price:
                total += self._bids[price].total
        return total

    def cumulative_ask_qty(self, limit_price):
        """Total ask quantity at prices of at most limit_price."""
        total = 0
        for price in self._ask_prices:
            if price >= limit_price:
                total += self._asks[price].total
        return total

    def imbalance(self, n=3):
        """Bid share of the top n levels, from 0.0 to 1.0, or None if empty."""
        bids = sum(q for _, q in self.depth(BUY, n))
        asks = sum(q for _, q in self.depth(SELL, n))
        if bids + asks == 0:
            return None
        return bids / (bids + asks)

    # -- trade statistics -----------------------------------------------------

    def last_price(self):
        return self.trades[-1].price if self.trades else None

    def volume(self):
        return sum(t.qty for t in self.trades)

    def vwap(self, last=None):
        """Volume weighted average price of the last n trades (all by default)."""
        sample = self.trades if last is None else self.trades[-last:]
        qty = sum(t.qty for t in sample)
        if qty == 0:
            return None
        return sum(t.notional for t in sample) / qty

    def high_low(self):
        if not self.trades:
            return None
        prices = [t.price for t in self.trades]
        return max(prices), min(prices)

    def trades_for(self, owner):
        return [t for t in self.trades if owner in (t.maker_owner, t.taker_owner)]

    def position(self, owner):
        """Net units bought minus sold by an owner across all trades."""
        net = 0
        for t in self.trades:
            if t.taker_owner == owner:
                net += t.qty if t.taker_side == BUY else -t.qty
            if t.maker_owner == owner:
                net += -t.qty if t.taker_side == BUY else t.qty
        return net

    def cash_flow(self, owner):
        """Money received minus money paid by an owner, fees not included."""
        flow = 0
        for t in self.trades:
            buyer_is_taker = t.taker_side == BUY
            buyer = t.taker_owner if buyer_is_taker else t.maker_owner
            seller = t.maker_owner if buyer_is_taker else t.taker_owner
            if buyer == owner:
                flow -= t.notional
            if seller == owner:
                flow += t.notional
        return flow

    # -- snapshots ------------------------------------------------------------

    def snapshot(self):
        """A plain dict of the resting orders, enough to rebuild the book."""
        rows = []
        for side in SIDES:
            for price in self._prices(side):
                for order in self._levels(side)[price].orders:
                    rows.append({
                        "id": order.id, "side": order.side, "price": order.price,
                        "qty": order.qty, "remaining": order.remaining,
                        "owner": order.owner, "seq": order.seq, "tif": order.tif,
                    })
        rows.sort(key=lambda r: r["seq"])
        return {"tick": self.tick, "next_id": self._next_id, "seq": self._seq,
                "orders": rows}

    @classmethod
    def restore(cls, data, **kwargs):
        book = cls(tick=data["tick"], **kwargs)
        book._next_id = data["next_id"]
        book._seq = data["seq"]
        for row in data["orders"]:
            order = Order(row["id"], row["side"], row["price"], row["qty"],
                          row["owner"], row["seq"], row["tif"])
            order.remaining = row["remaining"]
            book._orders[order.id] = order
            book._rest(order)
        return book

    # -- rendering ------------------------------------------------------------

    def render(self, n=5):
        """A small text ladder: asks on top, bids below."""
        lines = []
        for price, qty in reversed(self.depth(SELL, n)):
            lines.append("ask %8d %8d" % (price, qty))
        spread = self.spread()
        lines.append("--- spread %s ---" % ("n/a" if spread is None else spread))
        for price, qty in self.depth(BUY, n):
            lines.append("bid %8d %8d" % (price, qty))
        return "\n".join(lines)

    def summary(self):
        """A one-line text summary for logs."""
        return "bid=%s ask=%s last=%s vol=%d open=%d stops=%d" % (
            self.best_bid(), self.best_ask(), self.last_price(),
            self.volume(), self.open_count(), len(self._stops))

    def check_invariants(self):
        """Raise AssertionError when the book breaks one of its own rules."""
        for side in SIDES:
            prices = self._prices(side)
            assert prices == sorted(set(prices)), "ladder is not sorted"
            assert set(prices) == set(self._levels(side)), "ladder and levels differ"
            for price, level in self._levels(side).items():
                assert len(level) > 0, "empty level kept"
                assert level.total == sum(o.remaining for o in level.orders), "level total off"
                assert all(o.price == price for o in level.orders), "order at wrong level"
        bid, ask = self.best_bid(), self.best_ask()
        assert bid is None or ask is None or bid < ask, "book is crossed"
        return True

    # -- fees -------------------------------------------------------------------

    def _fee(self, notional, bps):
        """Fee in whole currency units, rounded half up."""
        return (notional * bps) // 10000

    def fee_for(self, trade):
        """Returns (maker_fee, taker_fee) for one trade."""
        return (self._fee(trade.notional, self.maker_bps),
                self._fee(trade.notional, self.taker_bps))

    def fees_by_owner(self):
        """Total fees paid per owner over all trades."""
        totals = {}
        for t in self.trades:
            maker_fee, taker_fee = self.fee_for(t)
            totals[t.maker_owner] = totals.get(t.maker_owner, 0) + maker_fee
            totals[t.taker_owner] = totals.get(t.taker_owner, 0) + taker_fee
        return totals

    def net_cash(self, owner):
        """Cash flow after fees."""
        return self.cash_flow(owner) - self.fees_by_owner().get(owner, 0)
