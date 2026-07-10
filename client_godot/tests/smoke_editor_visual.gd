## Live visual check of EDITOR MODE in the real game scene (terrain editing
## #78): boots the actual Main scene with --editor-mode (real editor
## auto-login, real free-fly camera, real streamer), drives the real
## BrushController through a ~2s programmatic raise stroke near the town
## centre (no synthetic harness — same code path as a human dragging LMB,
## minus the mouse), waits for the authoritative patch + mesh rebuild, and
## screenshots what the editor actually sees.
##
## Requires a live server on ws://127.0.0.1:8766. Run WITHOUT --headless:
##   Godot --path client_godot -s res://tests/smoke_editor_visual.gd -- --editor-mode --out=C:/some/path/shot.png
extends SceneTree

const _PAINT_AT := Vector2(3230.0, 3230.0)

var _main
var _t := 0.0
var _painted_ticks := 0
var _committed := false
var _out_path := "user://editor_visual.png"

func _initialize() -> void:
	for arg in OS.get_cmdline_user_args():
		if arg.begins_with("--out="):
			_out_path = arg.substr(len("--out="))
	_main = load("res://Main.tscn").instantiate()
	root.add_child(_main)

func _process(delta: float) -> bool:
	_t += delta
	# By ~5s: editor login, terrain.data, and the camera-anchored tile ring
	# should be resident. Paint for ~2s (40 ticks of 50ms) — the same
	# _paint_tick a mouse drag drives, at a fixed ground point.
	if _t > 5.0 and _painted_ticks < 40 and _main._brush != null:
		_main._brush._paint_tick(_PAINT_AT, 0.05, false)
		_painted_ticks += 1
		return false
	if _painted_ticks >= 40 and not _committed:
		_committed = true
		_main._brush._commit_stroke()
		return false
	# Give the patch + throttled mesh rebuild a moment, then shoot.
	if _t > 11.0:
		var lift := Protocol.terrain_height(_PAINT_AT.x, _PAINT_AT.y)
		var img := root.get_texture().get_image()
		var err := img.save_png(_out_path)
		if err != OK:
			push_error("SMOKE_FAIL: could not save screenshot to %s (error %d)" % [_out_path, err])
			quit(1)
			return true
		print("SMOKE_OK: editor-mode stroke painted (ground now %.2fm at the brush point) -> %s" % [lift, _out_path])
		quit(0)
		return true
	return false
