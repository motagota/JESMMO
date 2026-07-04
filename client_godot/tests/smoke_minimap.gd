## Headless smoke test: instantiate Minimap in a real scene tree (so _ready()
## actually runs) and drive every setter, to catch runtime errors --draw--
## the headless `--import` step doesn't fully exercise (nested class scoping,
## _draw() referencing outer-class constants, etc.).
## Run: Godot --headless --path client_godot -s res://tests/smoke_minimap.gd
extends SceneTree

func _initialize() -> void:
	var mm = load("res://ui/Minimap.gd").new()
	# _ready() (which builds the inner _View) is deferred to the node's first
	# frame in the tree, not synchronous with add_child -- wait for it, same
	# as real gameplay events always do (they need a server round-trip first).
	mm.ready.connect(_drive.bind(mm))
	root.add_child(mm)

func _drive(mm) -> void:
	mm.set_player(3200.0, 3200.0, Vector2(0, -1))
	mm.set_home(4000.0, 3200.0)
	mm.set_district_bounds({"x0": 4800, "y0": 0, "x1": 6400, "y1": 6400})
	mm.set_plots([
		{"plot_id": "p1", "bounds": {"x": 5000, "y": 100, "w": 80, "h": 80}, "owner_name": "Alice", "tier": 0},
		{"plot_id": "p2", "bounds": {"x": 5100, "y": 200, "w": 80, "h": 80}, "owner_name": null, "tier": 0},
	], "p1")

	# The actual regression this test exists for: the widget's final on-screen
	# rect must fall entirely inside the viewport, not off the edge.
	var view = mm._view
	var vp := root.get_visible_rect()
	print("SMOKE: viewport=", vp, " minimap rect=", view.get_rect())
	var rect: Rect2 = view.get_rect()
	if rect.position.x < 0 or rect.position.y < 0 \
			or rect.position.x + rect.size.x > vp.size.x \
			or rect.position.y + rect.size.y > vp.size.y:
		push_error("SMOKE_FAIL: minimap rect %s falls outside viewport %s" % [rect, vp])
		quit(1)
		return

	print("SMOKE_OK minimap instantiated, driven, and on-screen")
	quit(0)
