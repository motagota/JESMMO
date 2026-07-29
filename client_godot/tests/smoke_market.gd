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
	if not _body_text().contains("Order book"):
		_fail("commodities is the default section, got: %s" % _body_text()); return true
	_market.set_section(MarketPanel.Section.WAREHOUSE)
	if not _body_text().contains("collect where you bought"):
		_fail("warehouse section didn't render, got: %s" % _body_text()); return true
	_market.set_section(MarketPanel.Section.LISTINGS)
	if not _body_text().contains("durability"):
		_fail("listings section didn't render, got: %s" % _body_text()); return true

	# Walking away drops the trading state, so a stale market id can never
	# outlive being there.
	_market.set_market("")
	if not _body_text().contains("not trading"):
		_fail("walking away should clear the trading state"); return true

	print("SMOKE_OK: built markets are found by structure_kind (and nothing else is), and the panel gates on the server's ack, not on standing nearby")
	quit(0)
	return true

## The panel's current text. Children replaced by a rebuild are `queue_free`d,
## which is DEFERRED — they're still children until the next frame — so a
## same-frame read must skip them or it sees the previous section too.
func _body_text() -> String:
	var out := ""
	for c in _market._body.get_children():
		if c is Label and not c.is_queued_for_deletion():
			out += c.text + " | "
	return out
