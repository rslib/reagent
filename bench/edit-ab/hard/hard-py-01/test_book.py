import unittest

from book import BUY, SELL, FOK, IOC, BookError, OrderBook


class PartialFillTest(unittest.TestCase):
    def test_depth_after_partial_fill(self):
        book = OrderBook()
        book.add_limit(SELL, 101, 10, owner="mm")
        book.add_limit(SELL, 102, 5, owner="mm")
        book.add_limit(BUY, 101, 4, owner="bob")
        self.assertEqual(book.depth(SELL), [(101, 6), (102, 5)])
        self.assertEqual(book.volume_at(SELL, 101), 6)
        book.check_invariants()

    def test_shrinking_keeps_the_level_total_right(self):
        book = OrderBook()
        a, _ = book.add_limit(BUY, 99, 10, owner="a")
        book.add_limit(BUY, 99, 7, owner="b")
        book.modify(a.id, qty=6)
        self.assertEqual(book.volume_at(BUY, 99), 13)
        self.assertEqual(book.queue_position(a.id), 0)
        book.check_invariants()


class MatchingTest(unittest.TestCase):
    def test_equal_prices_trade(self):
        book = OrderBook()
        book.add_limit(SELL, 100, 5, owner="s")
        order, trades = book.add_limit(BUY, 100, 5, owner="b")
        self.assertEqual([(t.price, t.qty) for t in trades], [(100, 5)])
        self.assertEqual(order.status, "filled")
        self.assertEqual(book.open_count(), 0)

    def test_price_improvement_goes_to_the_taker(self):
        book = OrderBook()
        book.add_limit(SELL, 100, 5, owner="s")
        _, trades = book.add_limit(BUY, 105, 5, owner="b")
        self.assertEqual(trades[0].price, 100)

    def test_time_priority_inside_a_level(self):
        book = OrderBook()
        first, _ = book.add_limit(SELL, 100, 3, owner="first")
        second, _ = book.add_limit(SELL, 100, 3, owner="second")
        _, trades = book.add_limit(BUY, 100, 4, owner="b")
        self.assertEqual([(t.maker_id, t.qty) for t in trades],
                         [(first.id, 3), (second.id, 1)])
        self.assertEqual(book.queue_position(second.id), 0)

    def test_walks_several_levels(self):
        book = OrderBook()
        book.add_limit(SELL, 100, 2)
        book.add_limit(SELL, 101, 2)
        book.add_limit(SELL, 103, 2)
        order, trades = book.add_limit(BUY, 102, 10)
        self.assertEqual([t.price for t in trades], [100, 101])
        self.assertEqual(order.remaining, 6)
        self.assertEqual(book.best_bid(), 102)
        self.assertEqual(book.best_ask(), 103)

    def test_sell_side_crossing(self):
        book = OrderBook()
        book.add_limit(BUY, 100, 2)
        book.add_limit(BUY, 99, 2)
        _, trades = book.add_limit(SELL, 99, 3)
        self.assertEqual([(t.price, t.qty) for t in trades], [(100, 2), (99, 1)])

    def test_market_order_drops_the_rest(self):
        book = OrderBook()
        book.add_limit(SELL, 100, 3)
        order, trades = book.add_market(BUY, 8)
        self.assertEqual(sum(t.qty for t in trades), 3)
        self.assertEqual(order.status, "cancelled")
        self.assertEqual(book.open_count(), 0)

    def test_ioc_does_not_rest(self):
        book = OrderBook()
        book.add_limit(SELL, 100, 3)
        order, _ = book.add_limit(BUY, 100, 5, tif=IOC)
        self.assertEqual(order.remaining, 2)
        self.assertIsNone(book.best_bid())


class FillOrKillTest(unittest.TestCase):
    def test_fok_buy_fills_when_enough_is_available(self):
        book = OrderBook()
        book.add_limit(SELL, 100, 3)
        book.add_limit(SELL, 101, 3)
        book.add_limit(SELL, 105, 9)
        order, trades = book.add_limit(BUY, 101, 6, tif=FOK)
        self.assertEqual(order.status, "filled")
        self.assertEqual(sum(t.qty for t in trades), 6)

    def test_fok_buy_rejects_when_short(self):
        book = OrderBook()
        book.add_limit(SELL, 100, 3)
        book.add_limit(SELL, 101, 2)
        book.add_limit(SELL, 105, 9)
        order, trades = book.add_limit(BUY, 101, 6, tif=FOK)
        self.assertEqual(order.status, "rejected")
        self.assertEqual(trades, [])
        self.assertEqual(book.volume_at(SELL, 100), 3)

    def test_fok_sell_checks_bids(self):
        book = OrderBook()
        book.add_limit(BUY, 100, 4)
        book.add_limit(BUY, 98, 4)
        order, _ = book.add_limit(SELL, 99, 5, tif=FOK)
        self.assertEqual(order.status, "rejected")
        order, _ = book.add_limit(SELL, 99, 4, tif=FOK)
        self.assertEqual(order.status, "filled")

    def test_cumulative_quantities(self):
        book = OrderBook()
        for price, qty in ((100, 1), (101, 2), (102, 4)):
            book.add_limit(SELL, price, qty)
        for price, qty in ((99, 1), (98, 2), (97, 4)):
            book.add_limit(BUY, price, qty)
        self.assertEqual(book.cumulative_ask_qty(101), 3)
        self.assertEqual(book.cumulative_ask_qty(99), 0)
        self.assertEqual(book.cumulative_bid_qty(98), 3)
        self.assertEqual(book.cumulative_bid_qty(100), 0)


class CancelTest(unittest.TestCase):
    def test_cancel_last_order_removes_the_level(self):
        book = OrderBook()
        a, _ = book.add_limit(BUY, 100, 5)
        book.add_limit(BUY, 99, 5)
        book.cancel(a.id)
        self.assertEqual(book.best_bid(), 99)
        self.assertEqual(book.depth(BUY), [(99, 5)])
        book.check_invariants()

    def test_cancel_keeps_a_level_with_other_orders(self):
        book = OrderBook()
        a, _ = book.add_limit(SELL, 100, 5)
        book.add_limit(SELL, 100, 2)
        book.cancel(a.id)
        self.assertEqual(book.depth(SELL), [(100, 2)])

    def test_cancel_unknown_order(self):
        book = OrderBook()
        with self.assertRaises(BookError):
            book.cancel("nope")

    def test_cancel_owner(self):
        book = OrderBook()
        book.add_limit(BUY, 100, 1, owner="x")
        book.add_limit(BUY, 101, 1, owner="y")
        book.add_limit(SELL, 110, 1, owner="x")
        self.assertEqual(book.cancel_owner("x"), 2)
        self.assertEqual([o.owner for o in book.orders_for("y")], ["y"])
        self.assertEqual(book.best_ask(), None)

    def test_modify_price_loses_priority(self):
        book = OrderBook()
        a, _ = book.add_limit(BUY, 100, 5)
        b, _ = book.add_limit(BUY, 100, 5)
        new, _ = book.modify(a.id, price=100, qty=8)
        self.assertEqual(book.queue_position(b.id), 0)
        self.assertEqual(book.queue_position(new.id), 5)


class StopTest(unittest.TestCase):
    def test_buy_stop_fires_at_the_stop_price(self):
        book = OrderBook()
        book.add_limit(SELL, 100, 1)
        book.add_limit(SELL, 105, 5)
        stop = book.add_stop(BUY, 100, 3, owner="stopper")
        book.add_limit(BUY, 100, 1)
        self.assertEqual(book.pending_stops(), [])
        self.assertEqual(stop.status, "filled")
        self.assertEqual(book.volume(), 4)

    def test_sell_stop_fires_at_the_stop_price(self):
        book = OrderBook()
        book.add_limit(BUY, 100, 1)
        book.add_limit(BUY, 95, 5)
        stop = book.add_stop(SELL, 100, 3, owner="stopper")
        book.add_limit(SELL, 100, 1)
        self.assertEqual(book.pending_stops(), [])
        self.assertEqual(stop.filled, 3)

    def test_stop_waits_when_price_is_not_reached(self):
        book = OrderBook()
        book.add_limit(SELL, 100, 1)
        book.add_stop(BUY, 101, 1)
        book.add_limit(BUY, 100, 1)
        self.assertEqual(len(book.pending_stops()), 1)


class StatsTest(unittest.TestCase):
    def setUp(self):
        self.book = OrderBook(maker_bps=5, taker_bps=25)
        self.book.add_limit(SELL, 1000, 10, owner="mm")
        self.book.add_limit(BUY, 1000, 4, owner="bob")
        self.book.add_limit(BUY, 1010, 6, owner="amy")

    def test_vwap_and_volume(self):
        self.assertEqual(self.book.volume(), 10)
        self.assertEqual(self.book.vwap(), 1000.0)
        self.assertEqual(self.book.high_low(), (1000, 1000))

    def test_positions(self):
        self.assertEqual(self.book.position("mm"), -10)
        self.assertEqual(self.book.position("bob"), 4)
        self.assertEqual(self.book.cash_flow("mm"), 10000)

    def test_fee_rounds_half_up(self):
        book = OrderBook(maker_bps=0, taker_bps=25)
        book.add_limit(SELL, 1235, 10, owner="mm")
        _, trades = book.add_limit(BUY, 1235, 10, owner="bob")
        maker_fee, taker_fee = book.fee_for(trades[0])
        self.assertEqual(maker_fee, 0)
        self.assertEqual(taker_fee, 31)

    def test_fees_by_owner_and_net_cash(self):
        fees = self.book.fees_by_owner()
        self.assertEqual(fees["mm"], 5)
        self.assertEqual(fees["bob"], 10)
        self.assertEqual(fees["amy"], 15)
        self.assertEqual(self.book.net_cash("bob"), -4010)


class SnapshotTest(unittest.TestCase):
    def test_round_trip(self):
        book = OrderBook(tick=5)
        book.add_limit(BUY, 100, 3, owner="a")
        book.add_limit(SELL, 110, 4, owner="b")
        book.add_limit(BUY, 100, 2, owner="c")
        copy = OrderBook.restore(book.snapshot())
        self.assertEqual(copy.depth(BUY), [(100, 5)])
        self.assertEqual(copy.depth(SELL), [(110, 4)])
        self.assertEqual(copy.snapshot(), book.snapshot())
        self.assertEqual(copy.render(), book.render())

    def test_tick_grid(self):
        book = OrderBook(tick=5)
        with self.assertRaises(BookError):
            book.add_limit(BUY, 101, 1)


if __name__ == "__main__":
    unittest.main()
