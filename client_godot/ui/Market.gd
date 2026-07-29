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

## The section the player is looking at. Kept as an explicit enum rather than
## tab indices so the later issues can add their own without renumbering.
enum Section { COMMODITIES, LISTINGS, WAREHOUSE }

var _title: Label
var _body: VBoxContainer
var _tabs: HBoxContainer
var _section: Section = Section.COMMODITIES
var _market_id := ""
var _gold := 0

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
			todo.text = "Order book — buy and sell stackable goods at a price. Coming with the matching engine."
		Section.LISTINGS:
			todo.text = "Listing board — unique items (tools carry their own durability) at a fixed ask."
		Section.WAREHOUSE:
			todo.text = "Your stock held at this market. Goods are local: you collect where you bought."
	_body.add_child(todo)
