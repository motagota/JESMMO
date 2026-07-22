## Heads-up display: connection/zone/position, the gathered inventory, a one-line
## skills glance, a transient gather-progress line, floating "+N item" feedback, and
## a level-up banner. Built in code; driven by `Main`. The full skills breakdown
## (progress bars) lives in the dedicated `SkillsPanel` (K); this keeps a compact
## at-a-glance readout.
class_name Hud
extends CanvasLayer

## Right-clicked/clicked to unequip (mining/abilities epic #123, #119) — the
## "in hand" line doubles as the unarm affordance, so there's no separate
## button crowding the corner.
signal unequip_pressed

var _status: Label
var _inv: Label
var _skill: Label
var _gather: Label
var _build_hint: Label
var _rent_hint: Label
var _tool: Button
var _gain: Label
var _levelup: Label
var _announce: Label

var conn := "connecting…"
var zone := "—"
var pos := Vector2.ZERO
var _home := Vector2.ZERO
var _has_home := false

## skill_id -> {level, xp}, so each skill renders on the one line independently.
var _skills: Dictionary = {}

var _gain_tween: Tween
var _levelup_tween: Tween
var _announce_tween: Tween

func _ready() -> void:
	layer = 5
	var box := VBoxContainer.new()
	box.position = Vector2(12, 12)
	add_child(box)

	_status = _line(box)
	_inv = _line(box)
	_inv.text = "inventory: (empty)"
	_skill = _line(box)
	_skill.text = "gathering: Lv 0    [K] skills"
	_gather = _line(box)
	_gather.modulate = Color(0.8, 1.0, 0.6)
	_build_hint = _line(box)
	_build_hint.modulate = Color(0.85, 0.7, 1.0)
	_build_hint.text = "[B] build"
	_rent_hint = _line(box)
	_rent_hint.modulate = Color(1.0, 0.9, 0.5)
	_rent_hint.text = "[P] plot & rent"

	# In-hand line (mining/abilities epic #123, #119): a flat button so
	# clicking it unequips — empty (no text, no click target) while nothing's
	# armed, so it doesn't read as a dead affordance.
	_tool = Button.new()
	_tool.flat = true
	_tool.focus_mode = Control.FOCUS_NONE
	_tool.alignment = HORIZONTAL_ALIGNMENT_LEFT
	_tool.add_theme_font_size_override("font_size", 14)
	_tool.add_theme_color_override("font_color", Color(0.85, 0.95, 1.0))
	_tool.mouse_filter = Control.MOUSE_FILTER_IGNORE # nothing armed yet: not a click target
	_tool.pressed.connect(func(): unequip_pressed.emit())
	box.add_child(_tool)

	# Floating gain feedback, centred-ish on screen.
	_gain = Label.new()
	_gain.add_theme_font_size_override("font_size", 28)
	_gain.position = Vector2(560, 360)
	_gain.modulate = Color(0.9, 1.0, 0.6, 0.0)
	add_child(_gain)

	# Level-up banner, just above the gain feedback; gold so it reads as a milestone.
	_levelup = Label.new()
	_levelup.add_theme_font_size_override("font_size", 34)
	_levelup.position = Vector2(500, 300)
	_levelup.modulate = Color(1.0, 0.85, 0.2, 0.0)
	add_child(_levelup)

	# Onboarding announcements (e.g. "your plot is ready"), just above the
	# level-up banner; a distinct blue so it doesn't read as a skill milestone.
	_announce = Label.new()
	_announce.add_theme_font_size_override("font_size", 30)
	_announce.position = Vector2(420, 260)
	_announce.modulate = Color(0.55, 0.85, 1.0, 0.0)
	add_child(_announce)

	_refresh_status()

func _line(parent: Control) -> Label:
	var l := Label.new()
	l.add_theme_font_size_override("font_size", 14)
	parent.add_child(l)
	return l

# --- status line --------------------------------------------------------------

func set_conn(text: String) -> void:
	conn = text
	_refresh_status()

func set_zone(text: String) -> void:
	zone = text
	_refresh_status()

func set_pos(wx: float, wy: float) -> void:
	pos = Vector2(wx, wy)
	_refresh_status()

## The player's home plot centre (from `plot.assigned`), so the status line can
## show a distance/compass reading back to it (#11).
func set_home(wx: float, wy: float) -> void:
	_home = Vector2(wx, wy)
	_has_home = true
	_refresh_status()

func _refresh_status() -> void:
	if _status:
		var home_part := ""
		if _has_home:
			home_part = "   |   home: %s %dm" % [_compass(_home - pos), int(round(pos.distance_to(_home)))]
		_status.text = "%s   |   zone: %s   |   pos: (%d, %d)%s   |   [E] gather" % [
			conn, zone, int(round(pos.x)), int(round(pos.y)), home_part]

## A rough compass heading from the player toward `delta` (world units).
func _compass(delta: Vector2) -> String:
	if delta.length() < 1.0:
		return "here"
	var heading := ""
	if delta.y < -1.0:
		heading += "N"
	elif delta.y > 1.0:
		heading += "S"
	if delta.x > 1.0:
		heading += "E"
	elif delta.x < -1.0:
		heading += "W"
	return heading

# --- gameplay -----------------------------------------------------------------

func set_inventory(items: Array, used: int, capacity: int) -> void:
	var cap := "  [%d/%d]" % [used, capacity] if capacity > 0 else ""
	if items.is_empty():
		_inv.text = "inventory: (empty)" + cap
		return
	var parts := PackedStringArray()
	for it_v in items:
		var it: Dictionary = it_v
		parts.append("%s x%d" % [String(it.get("item_id", "?")), int(it.get("qty", 0))])
	_inv.text = "inventory: " + ", ".join(parts) + cap

## The armed tool, or "" for nothing equipped (mining/abilities epic #123,
## #119). A blank in-hand line while empty means the click target only
## exists once there's something to unequip.
func set_tool(item_id: String) -> void:
	if item_id == "":
		_tool.text = ""
		_tool.mouse_filter = Control.MOUSE_FILTER_IGNORE
	else:
		_tool.text = "In hand: %s %s   (click to unequip)" % [Protocol.item_icon(item_id), item_id]
		_tool.mouse_filter = Control.MOUSE_FILTER_STOP

func set_skill(skill_id: String, xp: int, level: int) -> void:
	# Track each skill independently so a building-XP gain doesn't overwrite the
	# gathering line (and vice versa).
	_skills[skill_id] = {"level": level, "xp": xp}
	var parts := PackedStringArray()
	for sid in _skills:
		var s: Dictionary = _skills[sid]
		parts.append("%s: Lv %d  (%d xp)" % [sid, int(s["level"]), int(s["xp"])])
	parts.append("[K] skills")
	_skill.text = "  |  ".join(parts)

func set_gather_progress(pct: int) -> void:
	if pct <= 0 or pct >= 100:
		_gather.text = ""
	else:
		_gather.text = "gathering… %d%%" % pct

## Build/place mode hint (#12): `active` shows the current kind/rotation and the
## controls; inactive clears the line.
func set_build_hint(active: bool, kind: String, rot: int) -> void:
	if active:
		_build_hint.text = "placing %s (%d°)   [Tab] kind  [R] rotate  [click/Enter] place  [Esc] cancel" % [kind, rot]
	else:
		_build_hint.text = "[B] build"

## Rent status hint (#14): a compact one-line readout, refreshed on every
## `rent.status` push (login, pay, auto-pay toggle, or a ticker-driven change).
func set_rent_hint(state: String, due_at: int) -> void:
	var now := int(Time.get_unix_time_from_system())
	var when := ""
	if state == "reclaimed":
		when = "reclaimed"
	elif now < due_at:
		when = "due in %dh" % maxi((due_at - now) / 3600, 0)
	else:
		when = "OVERDUE"
	_rent_hint.text = "rent: %s (%s)   [P] plot & rent" % [state.capitalize(), when]

func flash_gain(item_id: String, qty: int) -> void:
	_gather.text = ""
	_gain.text = "+%d %s" % [qty, item_id]
	_gain.modulate.a = 1.0
	if _gain_tween and _gain_tween.is_valid():
		_gain_tween.kill()
	_gain_tween = create_tween()
	_gain_tween.tween_property(_gain, "modulate:a", 0.0, 1.0)

## Celebrate a skill level-up (from a `skill.levelup` push): a gold banner that fades.
func flash_levelup(skill_id: String, level: int) -> void:
	_levelup.text = "%s  Level %d!" % [String(skill_id).capitalize(), level]
	_levelup.modulate.a = 1.0
	if _levelup_tween and _levelup_tween.is_valid():
		_levelup_tween.kill()
	_levelup_tween = create_tween()
	_levelup_tween.tween_property(_levelup, "modulate:a", 0.0, 2.0)

## A one-shot onboarding banner (e.g. the "here's your plot" moment) that lingers
## briefly before fading, longer than the levelup flash since it's read once ever.
func flash_announce(text: String) -> void:
	_announce.text = text
	_announce.modulate.a = 1.0
	if _announce_tween and _announce_tween.is_valid():
		_announce_tween.kill()
	_announce_tween = create_tween()
	_announce_tween.tween_interval(1.5)
	_announce_tween.tween_property(_announce, "modulate:a", 0.0, 2.0)
