## The weapon slot on the client (#160): a sword and a tool are held at once,
## each with its own HUD line, and neither overwrites the other. Two slots exist
## precisely so a player can mine and defend themselves without choosing, and a
## single shared readout would undo that at the UI layer.
##
## Run: Godot --headless --path client_godot -s res://tests/smoke_sword.gd
extends SceneTree

var _hud: Hud

func _fail(msg: String) -> void:
	print("SMOKE_FAIL: ", msg)
	quit(1)

func _hud_text() -> String:
	var parts := PackedStringArray()
	for n in _hud.find_children("*", "Label", true, false):
		parts.append(String(n.text))
	for n in _hud.find_children("*", "Button", true, false):
		parts.append(String(n.text))
	return " | ".join(parts)

func _initialize() -> void:
	_hud = Hud.new()
	root.add_child(_hud)

func _process(_delta: float) -> bool:
	# --- both slots, neither clobbering the other ----------------------------
	_hud.set_tool("pickaxe", 24, 30)
	_hud.set_weapon("sword", 27, 30, 40)
	var both := _hud_text()
	if not both.contains("pickaxe") or not both.contains("24/30"):
		_fail("the tool line went missing when a weapon was armed: %s" % both); return true
	if not both.contains("sword") or not both.contains("27/30"):
		_fail("the weapon line didn't render: %s" % both); return true
	if not both.contains("40 damage"):
		_fail("a swing's worth should be shown, got: %s" % both); return true

	# Wearing the blade updates only the weapon line.
	_hud.set_weapon("sword", 3, 30, 40)
	var worn := _hud_text()
	if not worn.contains("3/30"):
		_fail("weapon durability didn't update: %s" % worn); return true
	if not worn.contains("24/30"):
		_fail("updating the weapon clobbered the tool's durability: %s" % worn); return true

	# --- breaking disarms you, and says what a swing is worth now ------------
	_hud.set_weapon("", 0, 0, 20)
	var bare := _hud_text()
	if bare.contains("sword"):
		_fail("a broken sword is still shown as armed: %s" % bare); return true
	if not bare.contains("Unarmed") or not bare.contains("20 damage"):
		_fail("unarmed should still say what a swing is worth: %s" % bare); return true
	if not bare.contains("pickaxe"):
		_fail("losing the sword cleared the tool line too: %s" % bare); return true

	# --- and unequipping the tool leaves the weapon alone --------------------
	_hud.set_weapon("sword", 30, 30, 40)
	_hud.set_tool("", 0, 0)
	var toolless := _hud_text()
	if toolless.contains("pickaxe"):
		_fail("the tool line should be empty: %s" % toolless); return true
	if not toolless.contains("sword"):
		_fail("unequipping the tool cleared the weapon: %s" % toolless); return true

	print("SMOKE_OK: tool and weapon occupy separate HUD lines, wear independently, ",
		"and neither clobbers the other when the other changes")
	quit(0)
	return true
