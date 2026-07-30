## Market client test (market epic #136, issue #137), headless and offline.
##
## Covers what only the client can get wrong: a built market arrives as a
## generic `type == "structure"` entity distinguished by `state.kind`, so
## finding it needs the stored `structure_kind` rather than the entity type —
## the exact thing `nearest_market` exists for — plus the panel shell's
## show/hide and section behaviour.
##
## The build → market-exists → server-side range-gate flow is covered end to
## end by the Rust integration test (`market_open_requires_a_built_market_in_range`),
## which can mint the 80 units of materials a real client would have to mine.
## Run: Godot --headless --path client_godot -s res://tests/smoke_market.gd
extends SceneTree

const SITE := Vector2(12900, 12800) # the authored market site (world.rs)

var _entities: EntityManager
var _market: MarketPanel

func _fail(msg: String) -> void:
	print("SMOKE_FAIL: ", msg)
	quit(1)

func _initialize() -> void:
	_entities = EntityManager.new()
	root.add_child(_entities)
	_market = MarketPanel.new()
	root.add_child(_market)

func _structure(kind: String, at: Vector2) -> Dictionary:
	return {"x": at.x, "y": at.y, "type": "structure", "kind": kind, "facing": [0, 0]}

func _process(_delta: float) -> bool:
	# --- proximity ------------------------------------------------------------
	if _entities.nearest_market(SITE, Protocol.MARKET_RANGE) != "":
		_fail("found a market before any entity existed"); return true

	# A completed market arrives as a generic structure carrying kind=market.
	_entities.upsert("structure_market", "zone_a", _structure("market", SITE))
	var found := _entities.nearest_market(SITE, Protocol.MARKET_RANGE)
	if found != "structure_market":
		_fail("nearest_market didn't find the built market, got '%s'" % found); return true

	# Range is respected: just outside MARKET_RANGE finds nothing.
	var far := SITE + Vector2(Protocol.MARKET_RANGE + 5.0, 0)
	if _entities.nearest_market(far, Protocol.MARKET_RANGE) != "":
		_fail("found the market from outside MARKET_RANGE"); return true

	# Other city structures must NOT read as markets — the whole reason the
	# lookup matches on `structure_kind` and not the entity type.
	_entities.upsert("structure_well", "zone_a", _structure("town_well", SITE + Vector2(5, 0)))
	if _entities.nearest_market(SITE, Protocol.MARKET_RANGE) != "structure_market":
		_fail("a non-market structure was matched as a market"); return true

	# Nor do the authored fixtures that share the *storage*/board plumbing.
	_entities.upsert("storehouse_town", "zone_a", {"x": SITE.x, "y": SITE.y, "type": "storage"})
	if _entities.nearest_market(SITE, Protocol.MARKET_RANGE) != "structure_market":
		_fail("the storehouse was matched as a market"); return true

	# --- panel shell ----------------------------------------------------------
	_market.show_panel(false)
	if _market.visible:
		_fail("panel should start hidden"); return true

	# Before the server acks (`market.opened`), the panel is open but not
	# tradable — standing near a market is never itself the authorisation.
	_market.show_panel(true)
	_market.set_gold(500)
	if not _market.visible:
		_fail("panel should show"); return true
	if not _body_text().contains("not trading"):
		_fail("expected the not-yet-trading state, got: %s" % _body_text()); return true
	if not _body_text().contains("500"):
		_fail("purse should render, got: %s" % _body_text()); return true

	# Once the server confirms, the sections are live and switchable.
	_market.set_market("order-abc")
	if _body_text().contains("not trading"):
		_fail("should be trading after market.opened"); return true
	if not _body_text().contains("asks (for sale)"):
		_fail("commodities is the default section, got: %s" % _body_text()); return true
	_market.set_section(MarketPanel.Section.WAREHOUSE)
	if not _body_text().contains("nothing stored here yet"):
		_fail("warehouse section didn't render empty, got: %s" % _body_text()); return true
	_market.set_section(MarketPanel.Section.LISTINGS)
	if not _body_text().contains("durability"):
		_fail("listings section didn't render, got: %s" % _body_text()); return true

	# --- warehouse section (#138) ---------------------------------------------
	_market.set_section(MarketPanel.Section.WAREHOUSE)
	_market.set_inventory([
		{"item_id": "wood", "qty": 12},
		{"item_id": "pickaxe", "qty": 1, "durability": 43, "max_durability": 50},
	])
	_market.set_warehouse([
		{"id": "a", "item_id": "stone", "qty": 20, "state": "available"},
		{"id": "b", "item_id": "stone", "qty": 5, "state": "locked"},
		{"id": "c", "item_id": "axe", "qty": 1, "state": "available", "durability": 31, "max_durability": 50},
	], 3, 60)
	var w := _body_text()
	if not w.contains("3/60 slots"):
		_fail("slot usage should render, got: %s" % w); return true
	if not w.contains("stone  x20") or not w.contains("stone  x5"):
		_fail("available and locked stock should BOTH be listed, got: %s" % w); return true
	if not w.contains("🔒 on sale"):
		_fail("locked stock must say why it's untouchable, got: %s" % w); return true
	if not w.contains("axe  (31/50)"):
		_fail("a warehoused tool should show its own wear, got: %s" % w); return true
	if not w.contains("pickaxe  (43/50)"):
		_fail("carried tools should be depositable with their wear shown, got: %s" % w); return true

	# Only the AVAILABLE rows get a Withdraw button — locked stock is escrowed
	# and offering the button would be a lie.
	var withdraws := _buttons("Withdraw")
	if withdraws != 2:
		_fail("expected 2 Withdraw buttons (available stone + axe), got %d" % withdraws); return true
	if _buttons("Deposit") != 2:
		_fail("expected a Deposit button per carried item, got %d" % _buttons("Deposit")); return true

	# The deposit signal carries what the server needs; the server re-bounds it.
	var deposited := []
	_market.do_deposit.connect(func(item_id, qty): deposited.append([item_id, qty]))
	for row in _market._body.get_children():
		if row is HBoxContainer and not row.is_queued_for_deletion():
			for c in row.get_children():
				if c is Button and c.text == "Deposit":
					c.pressed.emit()
					break
	if deposited.is_empty():
		_fail("Deposit should emit do_deposit"); return true
	if deposited[0][0] != "wood" or deposited[0][1] != 12:
		_fail("deposit emitted the wrong payload: %s" % [deposited[0]]); return true

	# --- order book (#139) ----------------------------------------------------
	_market.set_section(MarketPanel.Section.COMMODITIES)
	_market.set_book("wood", [{"price": 8, "qty": 20}, {"price": 9, "qty": 5}], [{"price": 6, "qty": 3}])
	var bk := _body_text()
	if not bk.contains("best bid 6") or not bk.contains("best ask 8"):
		_fail("spread should read from the top of each side, got: %s" % bk); return true
	if not bk.contains("20 @ 8g") or not bk.contains("5 @ 9g") or not bk.contains("3 @ 6g"):
		_fail("every price level should render, got: %s" % bk); return true

	# Depth for a DIFFERENT commodity must be ignored — a stale push for
	# something we're not looking at would otherwise corrupt the view.
	_market.set_book("stone", [{"price": 99, "qty": 99}], [])
	if _body_text().contains("99 @ 99g"):
		_fail("depth for an unwatched item leaked into the view"); return true

	# The ticker shows the last fill.
	_market.note_trade("wood", 8, 5)
	if not _body_text().contains("last: 5 x wood @ 8g"):
		_fail("ticker didn't render, got: %s" % _body_text()); return true

	# Your own resting orders are the one place ownership IS shown, with a
	# Cancel each.
	_market.set_orders([
		{"order_id": "o1", "side": "sell", "item_id": "wood", "unit_price": 9, "qty_total": 10, "qty_remaining": 4},
	])
	if not _body_text().contains("sell 4/10 wood @ 9g"):
		_fail("own orders should render with fill progress, got: %s" % _body_text()); return true
	if _buttons("Cancel") != 1:
		_fail("expected one Cancel button, got %d" % _buttons("Cancel")); return true

	# Sell/Buy emit the watched commodity with the form's price and qty; the
	# server re-validates and re-bounds all of it.
	var sold := []
	var bought := []
	_market.do_sell.connect(func(i, p, q, h): sold.append([i, p, q, h]))
	_market.do_buy.connect(func(i, p, q, h): bought.append([i, p, q, h]))
	_market._form_price = 11
	_market._form_qty = 7
	_press("Sell")
	_press("Buy")
	if sold != [["wood", 11, 7, Protocol.DEFAULT_ORDER_HOURS]]:
		_fail("Sell emitted %s" % [sold]); return true
	if bought != [["wood", 11, 7, Protocol.DEFAULT_ORDER_HOURS]]:
		_fail("Buy emitted %s" % [bought]); return true

	# A resting order holds escrow, so it carries a duration (#140) — picking
	# a different one must actually travel with the command.
	sold.clear()
	_market._form_hours = Protocol.ORDER_DURATIONS_HOURS[0] # the shortest offered
	_press("Sell")
	if sold.is_empty() or sold[0][3] != Protocol.ORDER_DURATIONS_HOURS[0]:
		_fail("the chosen duration should ride the order, got %s" % [sold]); return true

	# Switching commodity clears the old depth and asks for the new book,
	# so one book's levels can never be read as another's.
	var watched := []
	_market.do_watch.connect(func(i): watched.append(i))
	_market._watch("stone")
	if watched != ["stone"]:
		_fail("switching should request the new book, got %s" % [watched]); return true
	if _body_text().contains("20 @ 8g"):
		_fail("old depth survived a commodity switch"); return true

	# --- fees (#141) ----------------------------------------------------------
	# The cost of placing must be visible BEFORE committing — the server charges
	# its own number, and these mirrored formulas have to agree with it or the
	# preview is a lie.
	_market._form_price = 8
	_market._form_qty = 20
	_market.set_book("stone", [], []) # force a rebuild at these values
	var fees := _body_text()
	if not fees.contains("listing fee %dg" % Protocol.listing_fee(160)):
		_fail("the listing fee should be previewed, got: %s" % fees); return true
	if not fees.contains("not refunded if you cancel"):
		_fail("the preview must say the fee isn't refundable, got: %s" % fees); return true
	if not fees.contains("taxed %dg" % Protocol.sale_tax(160)):
		_fail("the sale tax should be previewed, got: %s" % fees); return true

	# Fees round up and are never zero — the anti-exploit. A fee that rounded to
	# zero on small orders would make splitting an order a free lane.
	if Protocol.listing_fee(1) < 1 or Protocol.sale_tax(1) < 1:
		_fail("fees must never be zero on a nonzero amount"); return true
	if Protocol.listing_fee(101) != 2 or Protocol.sale_tax(101) != 4:
		_fail("fees must round UP (got %d / %d)" % [Protocol.listing_fee(101), Protocol.sale_tax(101)]); return true
	var split := 0
	for i in range(20):
		split += Protocol.listing_fee(8)
	if split < Protocol.listing_fee(160):
		_fail("splitting an order must not be cheaper than placing it whole"); return true

	# What the house actually took shows up after the fact.
	_market.note_fees(2, 5)
	var paid := _body_text()
	if not paid.contains("listing fee 2g") or not paid.contains("sale tax 5g"):
		_fail("the fees actually charged should render, got: %s" % paid); return true

	# The form must SURVIVE a rebuild. The body is rebuilt on every push —
	# including other players' trades — so a price you typed would otherwise be
	# wiped mid-order by someone else's activity.
	_market._form_price = 42
	_market._form_qty = 9
	_market.note_trade("stone", 3, 1) # somebody else trades; panel rebuilds
	if int(_market._price_field.value) != 42 or int(_market._qty_field.value) != 9:
		_fail("a rebuild wiped the order form (%s / %s)" % [
			_market._price_field.value, _market._qty_field.value]); return true
	var kept := []
	_market.do_buy.connect(func(i, p, q, h): kept.append([p, q]))
	_press("Buy")
	if kept != [[42, 9]]:
		_fail("the order should place what's still in the form, got %s" % [kept]); return true

	# Walking away drops the trading state, so a stale market id can never
	# outlive being there.
	_market.set_market("")
	if not _body_text().contains("not trading"):
		_fail("walking away should clear the trading state"); return true

	print("SMOKE_OK: markets found by structure_kind; panel gates on the server's ack not proximity; warehouse shows locked stock as unwithdrawable with tools keeping their wear; book renders anonymous depth, ignores other commodities' pushes, and place/cancel emit correctly")
	quit(0)
	return true

## Count visible buttons with the given label anywhere in the panel body.
func _buttons(label: String, node: Node = null) -> int:
	var n := 0
	for c in (node if node != null else _market._body).get_children():
		if c.is_queued_for_deletion():
			continue
		if c is Button and c.text == label:
			n += 1
		elif c.get_child_count() > 0:
			n += _buttons(label, c)
	return n

## Press the first visible button with the given label.
func _press(label: String, node: Node = null) -> bool:
	for c in (node if node != null else _market._body).get_children():
		if c.is_queued_for_deletion():
			continue
		if c is Button and c.text == label:
			c.pressed.emit()
			return true
		elif c.get_child_count() > 0 and _press(label, c):
			return true
	return false

## The panel's current text. Recurses, since item rows are Labels nested in an
## HBoxContainer alongside their button. Children replaced by a rebuild are
## `queue_free`d, which is DEFERRED — they're still children until the next
## frame — so a same-frame read must skip them or it sees the old section too.
func _body_text(node: Node = null) -> String:
	var out := ""
	for c in (node if node != null else _market._body).get_children():
		if c.is_queued_for_deletion():
			continue
		if c is Label:
			out += c.text + " | "
		elif c.get_child_count() > 0:
			out += _body_text(c)
	return out
