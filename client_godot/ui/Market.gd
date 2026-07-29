## Market panel (market epic #136, issue #137): shown while the player stands
## at a built Market structure. Built in code like the other panels; `Main`
## toggles visibility by proximity and hands it the `market.opened` ack.
##
## This is the SHELL. The three sections it lays out are where the rest of the
## epic lands — commodities order book (#139/#140), the listing board for
## unique items (#142), and the player's warehouse at this market (#138) —
## so each of those issues has somewhere to attach without re-litigating the
## layout. Each renders a placeholder until its issue ships.
##
## Standing here is not what authorises anything: the server independently
## range-checks every market command (`MARKET_RANGE` in proxy.rs), so this
## panel is a view, never a permission.
class_name MarketPanel
extends CanvasLayer

## Move goods between carried inventory and this market's warehouse (#138).
signal do_deposit(item_id: String, qty: int)
signal do_withdraw(item_id: String, qty: int)
## Order book (#139): rest a sell, cross the book with a buy, cancel your own,
## or switch which commodity's depth you're looking at.
signal do_sell(item_id: String, unit_price: int, qty: int)
signal do_buy(item_id: String, unit_price: int, qty: int)
signal do_cancel(order_id: String)
signal do_watch(item_id: String)

## The section the player is looking at. Kept as an explicit enum rather than
## tab indices so the later issues can add their own without renumbering.
enum Section { COMMODITIES, LISTINGS, WAREHOUSE }

var _title: Label
var _body: VBoxContainer
var _tabs: HBoxContainer
var _section: Section = Section.COMMODITIES
var _market_id := ""
var _gold := 0
## The warehouse at THIS market (#138) — rows as the server sent them, plus
## slot usage. Locked rows are shown but never offered a Withdraw button.
var _warehouse: Array = []
var _used_slots := 0
var _total_slots := 0
## Carried inventory, so the warehouse section can offer Deposit per item.
var _inventory: Array = []
## Order book state (#139): which commodity we're watching, its aggregated
## depth, our own resting orders, and the last few ticks.
var _watching := "wood"
var _asks: Array = []
var _bids: Array = []
var _orders: Array = []
var _last_trade := ""
## The commodities a v1 book can hold — the stackable items. Tools are
## excluded by the server too (they go to the listing board, #142); listing
## them here would just be an invitation to a rejection.
const TRADABLE := ["wood", "stone", "plank", "tool_kit"]
var _price_field: SpinBox
var _qty_field: SpinBox

func _ready() -> void:
	layer = 8
	var panel := PanelContainer.new()
	panel.position = Vector2(360, 80)
	panel.custom_minimum_size = Vector2(420, 0)
	add_child(panel)

	var col := VBoxContainer.new()
	col.add_theme_constant_override("separation", 8)
	panel.add_child(col)

	_title = Label.new()
	_title.add_theme_font_size_override("font_size", 15)
	_title.modulate = Color(1.0, 0.88, 0.55)
	_title.text = "⚖ Market"
	col.add_child(_title)

	_tabs = HBoxContainer.new()
	_tabs.add_theme_constant_override("separation", 6)
	col.add_child(_tabs)
	for s in [Section.COMMODITIES, Section.LISTINGS, Section.WAREHOUSE] as Array[Section]:
		var b := Button.new()
		b.text = _section_name(s)
		b.focus_mode = Control.FOCUS_NONE
		b.pressed.connect(func(): set_section(s))
		_tabs.add_child(b)

	_body = VBoxContainer.new()
	_body.add_theme_constant_override("separation", 6)
	col.add_child(_body)
	_rebuild()

func _section_name(s: Section) -> String:
	match s:
		Section.COMMODITIES: return "Commodities"
		Section.LISTINGS: return "Listings"
		Section.WAREHOUSE: return "Warehouse"
	return "?"

func show_panel(show: bool) -> void:
	visible = show

## The server acked that we're at a real market and may trade (#137). The id is
## the completed build order's own id — every later market command is scoped to
## it, even though only the capital's market exists in v1.
func set_market(market_id: String) -> void:
	if _market_id == market_id:
		return
	_market_id = market_id
	_rebuild()

## The purse, mirrored from `gold.update` (#145) — you can't judge a price
## without it, and every section needs it.
func set_gold(gold: int) -> void:
	if _gold == gold:
		return
	_gold = gold
	_rebuild()

func set_section(s: Section) -> void:
	if _section == s:
		return
	_section = s
	_rebuild()

## One commodity's aggregated depth (#139), from `market.book`. Ignored if
## it's not the book we're looking at.
func set_book(item_id: String, asks: Array, bids: Array) -> void:
	if item_id != _watching:
		return
	_asks = asks
	_bids = bids
	if _section == Section.COMMODITIES:
		_rebuild()

## Your own resting orders at this market (#139).
func set_orders(orders: Array) -> void:
	_orders = orders
	if _section == Section.COMMODITIES:
		_rebuild()

## The ticker (#139) — the last fill, so a trader can see the market move.
func note_trade(item_id: String, unit_price: int, qty: int) -> void:
	_last_trade = "last: %d x %s @ %dg" % [qty, item_id, unit_price]
	if _section == Section.COMMODITIES:
		_rebuild()

## Your warehouse at this market (#138), straight from `warehouse.state`.
func set_warehouse(items: Array, used: int, slots: int) -> void:
	_warehouse = items
	_used_slots = used
	_total_slots = slots
	_rebuild()

## Carried inventory, so the warehouse section knows what you could deposit.
func set_inventory(items: Array) -> void:
	_inventory = items
	if _section == Section.WAREHOUSE:
		_rebuild()

func _rebuild() -> void:
	if not _body:
		return
	for c in _body.get_children():
		c.queue_free()
	for i in range(_tabs.get_child_count()):
		var b := _tabs.get_child(i) as Button
		b.modulate = Color(1, 1, 1) if i == int(_section) else Color(0.65, 0.65, 0.7)

	var purse := Label.new()
	purse.add_theme_font_size_override("font_size", 12)
	purse.modulate = Color(1.0, 0.85, 0.2)
	purse.text = "purse: %d gold" % _gold
	_body.add_child(purse)

	if _market_id == "":
		var waiting := Label.new()
		waiting.modulate = Color(0.6, 0.6, 0.6)
		waiting.text = "(not trading — step up to the market)"
		_body.add_child(waiting)
		return

	var todo := Label.new()
	todo.add_theme_font_size_override("font_size", 12)
	todo.modulate = Color(0.65, 0.65, 0.7)
	todo.autowrap_mode = TextServer.AUTOWRAP_WORD_SMART
	todo.custom_minimum_size = Vector2(380, 0)
	match _section:
		Section.COMMODITIES:
			_rebuild_book()
			return
		Section.LISTINGS:
			todo.text = "Listing board — unique items (tools carry their own durability) at a fixed ask."
		Section.WAREHOUSE:
			_rebuild_warehouse()
			return
	_body.add_child(todo)

## The commodities section (#139): which good you're watching, its depth, the
## ticker, your own resting orders, and the place-order controls.
##
## Depth is deliberately anonymous — the server aggregates by price level and
## never says who is behind a level, so this can't show it either.
func _rebuild_book() -> void:
	var picker := HBoxContainer.new()
	for item in TRADABLE:
		var b := Button.new()
		b.text = item
		b.focus_mode = Control.FOCUS_NONE
		b.modulate = Color(1, 1, 1) if item == _watching else Color(0.6, 0.6, 0.65)
		b.pressed.connect(func(): _watch(item))
		picker.add_child(b)
	_body.add_child(picker)

	var best_ask := 0
	if not _asks.is_empty():
		best_ask = int((_asks[0] as Dictionary).get("price", 0))
	var spread := Label.new()
	spread.add_theme_font_size_override("font_size", 12)
	var bid_txt := "—" if _bids.is_empty() else str(int((_bids[0] as Dictionary).get("price", 0)))
	var ask_txt := "—" if _asks.is_empty() else str(best_ask)
	spread.text = "%s — best bid %s / best ask %s" % [_watching, bid_txt, ask_txt]
	_body.add_child(spread)
	if _last_trade != "":
		var tick := Label.new()
		tick.add_theme_font_size_override("font_size", 11)
		tick.modulate = Color(0.6, 0.9, 0.6)
		tick.text = "  " + _last_trade
		_body.add_child(tick)

	var asks_head := Label.new()
	asks_head.add_theme_font_size_override("font_size", 11)
	asks_head.modulate = Color(0.95, 0.7, 0.7)
	asks_head.text = "asks (for sale)"
	_body.add_child(asks_head)
	if _asks.is_empty():
		var none := Label.new()
		none.modulate = Color(0.6, 0.6, 0.6)
		none.text = "  (nothing for sale)"
		_body.add_child(none)
	for lvl_v in _asks:
		var lvl: Dictionary = lvl_v
		var l := Label.new()
		l.text = "  %d @ %dg" % [int(lvl.get("qty", 0)), int(lvl.get("price", 0))]
		_body.add_child(l)

	var bids_head := Label.new()
	bids_head.add_theme_font_size_override("font_size", 11)
	bids_head.modulate = Color(0.7, 0.9, 0.7)
	bids_head.text = "bids (wanted)"
	_body.add_child(bids_head)
	if _bids.is_empty():
		var none_b := Label.new()
		none_b.modulate = Color(0.6, 0.6, 0.6)
		none_b.text = "  (no buy orders yet)"
		_body.add_child(none_b)
	for lvl_v in _bids:
		var lvl: Dictionary = lvl_v
		var l := Label.new()
		l.text = "  %d @ %dg" % [int(lvl.get("qty", 0)), int(lvl.get("price", 0))]
		_body.add_child(l)

	# Place-order controls. The server re-validates everything; these bounds
	# just stop obviously-doomed commands leaving the client.
	var form := HBoxContainer.new()
	var price_lbl := Label.new()
	price_lbl.text = "price"
	form.add_child(price_lbl)
	_price_field = SpinBox.new()
	_price_field.min_value = Protocol.PRICE_TICK_GOLD
	_price_field.max_value = 100000
	_price_field.step = Protocol.PRICE_TICK_GOLD
	_price_field.value = maxi(best_ask, Protocol.PRICE_TICK_GOLD)
	form.add_child(_price_field)
	var qty_lbl := Label.new()
	qty_lbl.text = "qty"
	form.add_child(qty_lbl)
	_qty_field = SpinBox.new()
	_qty_field.min_value = Protocol.MIN_ORDER_QTY
	_qty_field.max_value = Protocol.MAX_ORDER_QTY
	_qty_field.value = 1
	form.add_child(_qty_field)
	_body.add_child(form)

	var actions := HBoxContainer.new()
	var sell_btn := Button.new()
	sell_btn.text = "Sell"
	sell_btn.pressed.connect(func(): do_sell.emit(_watching, int(_price_field.value), int(_qty_field.value)))
	actions.add_child(sell_btn)
	var buy_btn := Button.new()
	buy_btn.text = "Buy"
	buy_btn.pressed.connect(func(): do_buy.emit(_watching, int(_price_field.value), int(_qty_field.value)))
	actions.add_child(buy_btn)
	_body.add_child(actions)
	var hint := Label.new()
	hint.add_theme_font_size_override("font_size", 10)
	hint.modulate = Color(0.6, 0.6, 0.65)
	hint.autowrap_mode = TextServer.AUTOWRAP_WORD_SMART
	hint.custom_minimum_size = Vector2(380, 0)
	hint.text = "Selling escrows from your warehouse here. Buying fills instantly at the seller's price — bid above the ask and you keep the difference."
	_body.add_child(hint)

	# Your own resting orders — the one place ownership IS shown, because
	# it's yours.
	var mine_head := Label.new()
	mine_head.add_theme_font_size_override("font_size", 11)
	mine_head.modulate = Color(0.8, 0.85, 0.95)
	mine_head.text = "your orders"
	_body.add_child(mine_head)
	if _orders.is_empty():
		var none_o := Label.new()
		none_o.modulate = Color(0.6, 0.6, 0.6)
		none_o.text = "  (none resting)"
		_body.add_child(none_o)
	for o_v in _orders:
		var o: Dictionary = o_v
		var order_id := String(o.get("order_id", ""))
		var row := HBoxContainer.new()
		var l := Label.new()
		l.custom_minimum_size = Vector2(230, 0)
		l.text = "  %s %d/%d %s @ %dg" % [
			String(o.get("side", "")), int(o.get("qty_remaining", 0)),
			int(o.get("qty_total", 0)), String(o.get("item_id", "")),
			int(o.get("unit_price", 0))]
		row.add_child(l)
		var cancel := Button.new()
		cancel.text = "Cancel"
		cancel.pressed.connect(func(): do_cancel.emit(order_id))
		row.add_child(cancel)
		_body.add_child(row)

## Which commodity's book is on screen — Main seeds its depth on market.open.
func watching() -> String:
	return _watching

func _watch(item_id: String) -> void:
	if _watching == item_id:
		return
	_watching = item_id
	_asks = []
	_bids = []
	do_watch.emit(item_id)
	_rebuild()

## The warehouse section (#138): what you're holding here, and what you could
## put in. Locked rows render greyed with no Withdraw — they're escrowed
## against an open sell order, and a player looking at goods they can't take
## needs to see why rather than just find the button missing.
func _rebuild_warehouse() -> void:
	var head := Label.new()
	head.add_theme_font_size_override("font_size", 12)
	head.modulate = Color(0.8, 0.85, 0.95)
	head.text = "Held here — %d/%d slots" % [_used_slots, _total_slots]
	_body.add_child(head)

	if _warehouse.is_empty():
		var empty := Label.new()
		empty.modulate = Color(0.6, 0.6, 0.6)
		empty.text = "  (nothing stored here yet)"
		_body.add_child(empty)
	for it_v in _warehouse:
		var it: Dictionary = it_v
		var item_id := String(it.get("item_id", ""))
		var locked := String(it.get("state", "available")) == "locked"
		var row := HBoxContainer.new()
		var lbl := Label.new()
		lbl.custom_minimum_size = Vector2(210, 0)
		if it.has("durability"):
			lbl.text = "  %s  (%d/%d)" % [item_id, int(it.get("durability", 0)), int(it.get("max_durability", 0))]
		else:
			lbl.text = "  %s  x%d" % [item_id, int(it.get("qty", 0))]
		if locked:
			lbl.text += "   🔒 on sale"
			lbl.modulate = Color(0.6, 0.6, 0.65)
		row.add_child(lbl)
		if not locked:
			var qty := int(it.get("qty", 0))
			var btn := Button.new()
			btn.text = "Withdraw"
			btn.pressed.connect(func(): do_withdraw.emit(item_id, qty))
			row.add_child(btn)
		_body.add_child(row)

	var carry_head := Label.new()
	carry_head.add_theme_font_size_override("font_size", 12)
	carry_head.modulate = Color(0.8, 0.85, 0.95)
	carry_head.text = "Carried — deposit to trade"
	_body.add_child(carry_head)
	if _inventory.is_empty():
		var none := Label.new()
		none.modulate = Color(0.6, 0.6, 0.6)
		none.text = "  (carrying nothing)"
		_body.add_child(none)
	for it_v in _inventory:
		var it: Dictionary = it_v
		var item_id := String(it.get("item_id", ""))
		var qty := int(it.get("qty", 0))
		var row := HBoxContainer.new()
		var lbl := Label.new()
		lbl.custom_minimum_size = Vector2(210, 0)
		if it.has("durability"):
			lbl.text = "  %s  (%d/%d)" % [item_id, int(it.get("durability", 0)), int(it.get("max_durability", 0))]
		else:
			lbl.text = "  %s  x%d" % [item_id, qty]
		row.add_child(lbl)
		var btn := Button.new()
		btn.text = "Deposit"
		btn.pressed.connect(func(): do_deposit.emit(item_id, qty))
		row.add_child(btn)
		_body.add_child(row)
