import unittest

from inventory import Inventory, StockError


def make():
    inv = Inventory()
    inv.add_item("A1", "bolt", qty=5, unit_price=0.5, tags=["metal"])
    inv.add_item("B2", "nut", qty=12, unit_price=0.25, tags=["metal"])
    inv.add_item("C3", "washer", qty=3, unit_price=0.1)
    return inv


class InventoryTest(unittest.TestCase):
    def test_receive_and_ship(self):
        inv = make()
        self.assertEqual(inv.receive("A1", 5), 10)
        self.assertEqual(inv.ship("A1", 4), 6)

    def test_ship_too_much(self):
        with self.assertRaises(StockError):
            make().ship("C3", 4)

    def test_ship_zero_is_rejected(self):
        with self.assertRaises(StockError):
            make().ship("A1", 0)

    def test_add_item_allows_zero_quantity(self):
        inv = make()
        inv.add_item("D4", "gasket")
        self.assertEqual(inv.get("D4").qty, 0)

    def test_low_stock_includes_threshold(self):
        self.assertEqual(make().low_stock(5), ["A1", "C3"])

    def test_low_stock_default(self):
        self.assertEqual(make().low_stock(), ["A1", "C3"])

    def test_restock_plan_default_target(self):
        self.assertEqual(make().restock_plan(), {"A1": 15, "B2": 8, "C3": 17})

    def test_restock_plan_custom_target(self):
        self.assertEqual(make().restock_plan(6), {"A1": 1, "C3": 3})

    def test_value_and_tags(self):
        inv = make()
        self.assertEqual(inv.total_value(), 5.8)
        self.assertEqual(inv.with_tag("metal"), ["A1", "B2"])


if __name__ == "__main__":
    unittest.main()
