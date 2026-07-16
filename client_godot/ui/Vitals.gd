## The vitals HUD (#89) — the client's first health display. Bottom-centre
## bar stack driven ONLY by server state (`status_update`'s hp/breath/poison
## fields, #87/#88): no client-side prediction of drain rates, the same
## server-owns-the-truth posture as the editor brush reconcile.
##
## - Health: always visible once the first vitals arrive.
## - Breath: visible while submerged or still refilling — it drains in the
##   water, lingers while it recovers on land, then tucks away.
## - Poison: visible while any buildup exists; at the proc it locks to full,
##   the label turns to a warning, and a sickly screen tint makes the state
##   unmissable (there is no cure in v1 — the tint IS the death sentence).
## - Death: a brief dark overlay on `you_died`, fading out as the respawn
##   (which arrives almost immediately) takes over.
##
## Everything is built in `_init`, not `_ready` — headless tests may drive
## it before it enters the tree (the #79 lesson).
class_name VitalsHud
extends CanvasLayer

const _BAR_W := 260.0
const _BAR_H := 16.0

var _root: Control
var _stack: VBoxContainer
var _hp_row: Control
var _hp_fill: ColorRect
var _hp_label: Label
var _breath_row: Control
var _breath_fill: ColorRect
var _breath_label: Label
var _poison_row: Control
var _poison_fill: ColorRect
var _poison_label: Label
var _poison_tint: ColorRect
var _death_overlay: ColorRect
var _death_label: Label
var _death_tween: Tween

var _seen_vitals := false

func _init() -> void:
	layer = 6 # above the text HUD (5)

	# Full-screen, click-through root the anchored children hang off.
	_root = Control.new()
	_root.set_anchors_preset(Control.PRESET_FULL_RECT)
	_root.mouse_filter = Control.MOUSE_FILTER_IGNORE
	add_child(_root)

	# Poison proc tint: under everything else on this layer, whole screen.
	_poison_tint = ColorRect.new()
	_poison_tint.set_anchors_preset(Control.PRESET_FULL_RECT)
	_poison_tint.color = Color(0.38, 0.10, 0.45, 0.20)
	_poison_tint.mouse_filter = Control.MOUSE_FILTER_IGNORE
	_poison_tint.visible = false
	_root.add_child(_poison_tint)

	# The bar stack, bottom-centre.
	_stack = VBoxContainer.new()
	_stack.set_anchors_preset(Control.PRESET_CENTER_BOTTOM)
	_stack.offset_left = -_BAR_W * 0.5
	_stack.offset_right = _BAR_W * 0.5
	_stack.offset_top = -96
	_stack.offset_bottom = -18
	_stack.alignment = BoxContainer.ALIGNMENT_END
	_stack.add_theme_constant_override("separation", 6)
	_stack.mouse_filter = Control.MOUSE_FILTER_IGNORE
	_stack.visible = false # until the first vitals arrive
	_root.add_child(_stack)

	var poison := _bar(Color(0.55, 0.25, 0.65))
	_poison_row = poison[0]; _poison_fill = poison[1]; _poison_label = poison[2]
	_poison_row.visible = false
	var breath := _bar(Color(0.35, 0.65, 0.95))
	_breath_row = breath[0]; _breath_fill = breath[1]; _breath_label = breath[2]
	_breath_row.visible = false
	var hp := _bar(Color(0.80, 0.22, 0.22))
	_hp_row = hp[0]; _hp_fill = hp[1]; _hp_label = hp[2]

	# Death overlay: over everything, including the bars.
	_death_overlay = ColorRect.new()
	_death_overlay.set_anchors_preset(Control.PRESET_FULL_RECT)
	_death_overlay.color = Color(0.06, 0.0, 0.0, 0.0)
	_death_overlay.mouse_filter = Control.MOUSE_FILTER_IGNORE
	_death_overlay.visible = false
	_root.add_child(_death_overlay)
	_death_label = Label.new()
	_death_label.text = "You died"
	_death_label.add_theme_font_size_override("font_size", 48)
	_death_label.set_anchors_preset(Control.PRESET_CENTER)
	_death_label.offset_left = -120
	_death_label.offset_top = -30
	_death_label.modulate = Color(0.9, 0.75, 0.75)
	_death_overlay.add_child(_death_label)

## One labelled bar row: dark backing, coloured fill, centred text.
func _bar(fill_color: Color) -> Array:
	var row := Panel.new()
	row.custom_minimum_size = Vector2(_BAR_W, _BAR_H + 4)
	row.mouse_filter = Control.MOUSE_FILTER_IGNORE
	var style := StyleBoxFlat.new()
	style.bg_color = Color(0.08, 0.08, 0.10, 0.82)
	style.set_corner_radius_all(3)
	row.add_theme_stylebox_override("panel", style)
	var fill := ColorRect.new()
	fill.color = fill_color
	fill.position = Vector2(2, 2)
	fill.size = Vector2(_BAR_W - 4, _BAR_H)
	fill.mouse_filter = Control.MOUSE_FILTER_IGNORE
	row.add_child(fill)
	var label := Label.new()
	label.set_anchors_preset(Control.PRESET_FULL_RECT)
	label.horizontal_alignment = HORIZONTAL_ALIGNMENT_CENTER
	label.vertical_alignment = VERTICAL_ALIGNMENT_CENTER
	label.add_theme_font_size_override("font_size", 12)
	label.add_theme_color_override("font_color", Color(0.95, 0.95, 0.95))
	label.add_theme_color_override("font_outline_color", Color(0, 0, 0, 0.8))
	label.add_theme_constant_override("outline_size", 4)
	label.mouse_filter = Control.MOUSE_FILTER_IGNORE
	row.add_child(label)
	_stack.add_child(row)
	return [row, fill, label]

func _set_fill(fill: ColorRect, ratio: float) -> void:
	fill.size.x = maxf((_BAR_W - 4.0) * clampf(ratio, 0.0, 1.0), 0.0)

## Feed one own-player `status_update`'s vitals (server-authoritative).
func set_vitals(hp: int, max_hp: int, breath: int, max_breath: int, submerged: bool,
		poison_buildup: int, max_poison: int, poisoned: bool) -> void:
	_seen_vitals = true
	_stack.visible = true

	_set_fill(_hp_fill, float(hp) / maxf(float(max_hp), 1.0))
	_hp_label.text = "HP %d / %d" % [maxi(hp, 0), max_hp]

	# Breath shows in the water and lingers while refilling on land.
	_breath_row.visible = submerged or breath < max_breath
	if _breath_row.visible:
		_set_fill(_breath_fill, float(breath) / maxf(float(max_breath), 1.0))
		if submerged and breath == 0:
			_breath_label.text = "DROWNING"
		elif submerged:
			_breath_label.text = "Breath"
		else:
			_breath_label.text = "Breath (recovering)"

	# Poison shows while anything is built up; the proc locks it on.
	_poison_row.visible = poisoned or poison_buildup > 0
	if _poison_row.visible:
		_set_fill(_poison_fill, 1.0 if poisoned else float(poison_buildup) / maxf(float(max_poison), 1.0))
		_poison_label.text = "POISONED" if poisoned else "Poison"
	_poison_tint.visible = poisoned

## `you_died`: flash the overlay, then fade as the respawn takes over.
func show_death() -> void:
	_death_overlay.visible = true
	_death_overlay.color.a = 0.6
	_death_label.modulate.a = 1.0
	if _death_tween and _death_tween.is_valid():
		_death_tween.kill()
	_death_tween = create_tween()
	_death_tween.tween_interval(1.2)
	_death_tween.tween_property(_death_overlay, "color:a", 0.0, 1.3)
	_death_tween.parallel().tween_property(_death_label, "modulate:a", 0.0, 1.3)
	_death_tween.tween_callback(func(): _death_overlay.visible = false)
