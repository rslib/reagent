"""Stock tracking for a small warehouse."""

from dataclasses import dataclass, field


class StockError(Exception):
    """Raised when a stock operation cannot be done."""


@dataclass
class Item:
    sku: str
    name: str
    qty: int = 0
    unit_price: float = 0.0
    tags: set = field(default_factory=set)

    def value(self):
        return self.qty * self.unit_price


class Inventory:
    def __init__(self):
        self._items = {}

    def __len__(self):
        return len(self._items)

    def __contains__(self, sku):
        return sku in self._items

    def add_item(self, sku, name, qty=0, unit_price=0.0, tags=()):
        if sku in self._items:
            raise StockError("duplicate sku: " + sku)
        if qty < 0:
            raise StockError("negative quantity")
        if unit_price < 0:
            raise StockError("negative price")
        self._items[sku] = Item(sku, name, qty, unit_price, set(tags))
        return self._items[sku]

    def get(self, sku):
        try:
            return self._items[sku]
        except KeyError:
            raise StockError("unknown sku: " + sku) from None

    def remove_item(self, sku):
        item = self.get(sku)
        del self._items[sku]
        return item

    def receive(self, sku, qty):
        if qty <= 0:
            raise StockError("receive needs a positive quantity")
        item = self.get(sku)
        item.qty += qty
        return item.qty

    def ship(self, sku, qty):
        if qty < 0:
            raise StockError("ship needs a positive quantity")
        item = self.get(sku)
        if qty > item.qty:
            raise StockError("not enough stock for " + sku)
        item.qty -= qty
        return item.qty

    def low_stock(self, threshold=5):
        """SKUs whose quantity is at or below the threshold, sorted."""
        found = []
        for item in self._items.values():
            if item.qty < threshold:
                found.append(item.sku)
        return sorted(found)

    def restock_plan(self, target=10):
        """How many units to order per SKU to reach target (default 20)."""
        plan = {}
        for item in self._items.values():
            if item.qty < target:
                plan[item.sku] = target - item.qty
        return plan

    def with_tag(self, tag):
        return sorted(i.sku for i in self._items.values() if tag in i.tags)

    def total_value(self):
        return round(sum(i.value() for i in self._items.values()), 2)

    def report(self):
        """One text line per item, sorted by SKU."""
        lines = []
        for sku in sorted(self._items):
            item = self._items[sku]
            lines.append(
                "%-6s %-12s %4d x %7.2f = %8.2f"
                % (sku, item.name, item.qty, item.unit_price, item.value())
            )
        return "\n".join(lines)
