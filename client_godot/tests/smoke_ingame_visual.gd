## Live visual check of the REAL game (not a synthetic harness): instantiates
## the actual Main scene — real camera rig, real lighting, real spawn, real
## streamer wiring — guest-logs-in through the real Login panel's signal,
## waits for terrain + streamed tiles to settle, and screenshots exactly what
## a player standing at spawn sees. Exists because a bespoke test camera can
## look great while the in-game view doesn't; this catches that gap.
##
## Requires a live server on ws://127.0.0.1:8766. Run WITHOUT --headless:
##   Godot --path client_godot -s res://tests/smoke_ingame_visual.gd -- --out=C:/some/path/shot.png
extends SceneTree

var _main
var _t := 0.0
var _guest_sent := false
var _out_path := "user://ingame_visual.png"

func _initialize() -> void:
	for arg in OS.get_cmdline_user_args():
		if arg.begins_with("--out="):
			_out_path = arg.substr(len("--out="))
	_main = load("res://Main.tscn").instantiate()
	root.add_child(_main)

func _process(delta: float) -> bool:
	_t += delta
	# Give the real connect/auth_required round-trip a moment, then guest in
	# through the same signal the Guest button fires.
	if not _guest_sent and _t > 1.5:
		_main._login.do_guest.emit()
		_guest_sent = true
	# By ~10s: welcome, terrain.data, and the spawn ring's 9 tiles should all
	# have arrived and built (tile meshes build synchronously on arrival).
	if _t > 10.0:
		var img := root.get_texture().get_image()
		var err := img.save_png(_out_path)
		if err != OK:
			push_error("SMOKE_FAIL: could not save screenshot to %s (error %d)" % [_out_path, err])
			quit(1)
			return true
		var tiles: int = _main._streamer._loaded.size()
		print("SMOKE_OK: in-game view captured with %d streamed tiles resident -> %s" % [tiles, _out_path])
		quit(0)
		return true
	return false
