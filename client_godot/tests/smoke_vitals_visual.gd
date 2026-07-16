## Visual check for the vitals HUD (#89): two staged screenshots — swimming
## (hp + draining breath + edge-of-forest buildup) and the poison proc with
## the death overlay on top. No server needed.
## Run: Godot --path client_godot -s res://tests/smoke_vitals_visual.gd
## Screenshots: user://vitals_swim.png, user://vitals_proc.png
extends SceneTree

var _v: VitalsHud
var _frames := 0

func _initialize() -> void:
	# A neutral backdrop so the bars/tints read like in-game.
	var env := Environment.new()
	env.background_mode = Environment.BG_COLOR
	env.background_color = Color(0.35, 0.45, 0.38)
	var we := WorldEnvironment.new()
	we.environment = env
	root.add_child(we)
	var cam := Camera3D.new()
	root.add_child(cam)
	cam.make_current()

	_v = VitalsHud.new()
	root.add_child(_v)
	# Stage 1: mid-swim, breath half gone, a whiff of poison from the bank trees.
	_v.set_vitals(72, 100, 90, 200, true, 35, 100, false)

func _process(_delta: float) -> bool:
	_frames += 1
	if _frames == 10:
		root.get_viewport().get_texture().get_image().save_png("user://vitals_swim.png")
		# Stage 2: the proc, mid-death.
		_v.set_vitals(22, 100, 200, 200, false, 100, 100, true)
		_v.show_death()
	if _frames == 20:
		root.get_viewport().get_texture().get_image().save_png("user://vitals_proc.png")
		print("SMOKE_OK: wrote user://vitals_swim.png and user://vitals_proc.png")
		quit(0)
	return false
