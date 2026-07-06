## Headless smoke test: World.apply_plot_roster's per-plot marker shape --
## a free plot gets just the tile+border (no signpost at all), a taken one
## also gets a small signpost (post + name-plank + label) naming the owner.
## Run: Godot --headless --path client_godot -s res://tests/smoke_world_plots.gd
extends SceneTree

func _initialize() -> void:
	var world = load("res://world/World.gd").new()
	root.add_child(world)
	world.apply_partition({"world": 6400, "zones": []})

	var plots := [
		{"plot_id": "free1", "bounds": {"x": 4800, "y": 0, "w": 80, "h": 80}, "owner_name": null},
		{"plot_id": "taken1", "bounds": {"x": 4880, "y": 0, "w": 80, "h": 80}, "owner_name": "Alice"},
	]
	world.apply_plot_roster(plots, "not_mine")

	# Each marker is 1 fill + 4 border strips = 5 nodes; a taken plot adds a
	# signpost (post + plank + label) = 3 more, so 8 total.
	var count: int = world._plots_root.get_child_count()
	print("total _plots_root children for 1 free + 1 taken plot:", count)
	if count != 13:
		print("SMOKE_FAIL: expected 5 (free) + 8 (taken) = 13 children, got %d" % count)
		quit(1)
		return

	# Confirm exactly one Label3D exists (the taken plot's signpost label,
	# reading the owner's name) -- the free plot must not have added one.
	var labels := 0
	var label_texts := []
	for child in world._plots_root.get_children():
		if child is Label3D:
			labels += 1
			label_texts.append(child.text)
	print("Label3D count:", labels, "texts:", label_texts)
	if labels != 1 or label_texts[0] != "Alice":
		print("SMOKE_FAIL: expected exactly one Label3D reading 'Alice', got %s" % [label_texts])
		quit(1)
		return

	print("SMOKE_OK: free plots get no signpost, taken plots get exactly one naming the owner")
	quit(0)
