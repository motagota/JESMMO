## Skills panel: the character's use-based skills with level and a progress bar to
## the next level. Toggled with the **K** key. Fed by `skill.update` pushes (via
## `Main`); use-based progression only rises, so bars never regress. The XP→level
## curve is mirrored from the server in `Protocol` so the bar fill matches exactly.
class_name SkillsPanel
extends CanvasLayer

var _list: VBoxContainer
## skill_id -> {xp, level}. Sorted by id so the rows keep a stable order.
var _skills: Dictionary = {}

var _pinned := false # toggled by the K key

func _ready() -> void:
	layer = 8
	var panel := PanelContainer.new()
	panel.position = Vector2(700, 40)
	panel.custom_minimum_size = Vector2(260, 0)
	add_child(panel)

	var col := VBoxContainer.new()
	col.add_theme_constant_override("separation", 8)
	panel.add_child(col)

	var head := Label.new()
	head.text = "Skills  (K)"
	head.add_theme_font_size_override("font_size", 14)
	col.add_child(head)

	_list = VBoxContainer.new()
	_list.add_theme_constant_override("separation", 10)
	col.add_child(_list)
	_rebuild()
	visible = false

func set_skill(skill_id: String, xp: int, level: int) -> void:
	_skills[skill_id] = {"xp": xp, "level": level}
	_rebuild()

func _unhandled_key_input(event: InputEvent) -> void:
	if event is InputEventKey and event.pressed and not event.echo and event.keycode == KEY_K:
		_pinned = not _pinned
		visible = _pinned
		get_viewport().set_input_as_handled()

func _rebuild() -> void:
	if not _list:
		return
	for c in _list.get_children():
		c.queue_free()
	if _skills.is_empty():
		var empty := Label.new()
		empty.text = "(no skills yet — go gather or build)"
		empty.modulate = Color(0.6, 0.6, 0.6)
		_list.add_child(empty)
		return
	var ids := _skills.keys()
	ids.sort()
	for sid in ids:
		var s: Dictionary = _skills[sid]
		var xp := int(s["xp"])
		var level := int(s["level"])
		# XP band for the current level -> next level, so the bar shows progress within
		# the level rather than absolute xp.
		var floor_xp := Protocol.xp_for_level(level)
		var next_xp := Protocol.xp_for_level(level + 1)
		var into := xp - floor_xp
		var span := maxi(next_xp - floor_xp, 1)

		var head := Label.new()
		head.add_theme_font_size_override("font_size", 13)
		head.text = "%s — Lv %d" % [String(sid).capitalize(), level]
		_list.add_child(head)

		var bar := ProgressBar.new()
		bar.min_value = 0
		bar.max_value = span
		bar.value = clampi(into, 0, span)
		bar.custom_minimum_size = Vector2(230, 16)
		bar.show_percentage = false
		_list.add_child(bar)

		var caption := Label.new()
		caption.add_theme_font_size_override("font_size", 11)
		caption.modulate = Color(0.75, 0.8, 0.85)
		caption.text = "%d / %d xp to Lv %d" % [into, span, level + 1]
		_list.add_child(caption)
