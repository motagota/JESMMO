## Editor session history + undo (terrain editing #79). Records this
## session's accepted edit ops (from `terrain.edit_ack`), newest first;
## Ctrl+Z undoes the most recent un-reverted op, or select an entry and
## press its Undo button. H toggles the panel. Reverts are issued
## newest-first by design — the server restores whole pre-edit blocks, so
## out-of-order reverts can clobber later overlapping strokes (the panel
## still allows selecting older ops; the entry label warns).
##
## State transitions are driven only by server acks (`mark_reverted` from
## `terrain.revert_ack`), never optimistically — a rejected revert leaves
## the entry undoable.
class_name HistoryPanel
extends CanvasLayer

signal do_revert(op_id: String)

## Ordered newest-first: [{id, brush, reverted}]
var _ops: Array = []
var _list: ItemList
var _undo_button: Button
var _key_down: Dictionary = {}

## Built in _init (not _ready) so the panel is usable the moment it's
## constructed — callers (and headless tests) may record ops before the
## node enters the tree.
func _init() -> void:
	layer = 8
	visible = false
	var panel := PanelContainer.new()
	panel.position = Vector2(20, 200)
	panel.custom_minimum_size = Vector2(300, 0)
	add_child(panel)

	var col := VBoxContainer.new()
	col.add_theme_constant_override("separation", 8)
	panel.add_child(col)

	var head := Label.new()
	head.text = "Edit History  [H] close — Ctrl+Z undoes last"
	head.add_theme_font_size_override("font_size", 14)
	col.add_child(head)

	_list = ItemList.new()
	_list.custom_minimum_size = Vector2(280, 180)
	col.add_child(_list)

	_undo_button = Button.new()
	_undo_button.text = "Undo selected"
	_undo_button.pressed.connect(_on_undo_selected)
	col.add_child(_undo_button)

## An accepted op of ours (`terrain.edit_ack`) — newest goes on top.
func record_op(op_id: String, brush: String) -> void:
	_ops.push_front({"id": op_id, "brush": brush, "reverted": false})
	_refresh()

## The server confirmed a revert (`terrain.revert_ack`).
func mark_reverted(op_id: String) -> void:
	for op in _ops:
		if op["id"] == op_id:
			op["reverted"] = true
	_refresh()

## The op id Ctrl+Z would revert right now: newest un-reverted, or "".
func undo_last_target() -> String:
	for op in _ops:
		if not op["reverted"]:
			return op["id"]
	return ""

func _on_undo_selected() -> void:
	var selected := _list.get_selected_items()
	if selected.is_empty():
		return
	var op: Dictionary = _ops[selected[0]]
	if not op["reverted"]:
		do_revert.emit(op["id"])

func _refresh() -> void:
	_list.clear()
	for i in range(_ops.size()):
		var op: Dictionary = _ops[i]
		var mark: String = "undone  " if op["reverted"] else ""
		var order_note: String = "" if i == 0 or op["reverted"] else "  (older — undo may clobber newer overlaps)"
		_list.add_item("%s%s  %s%s" % [mark, op["brush"], op["id"].substr(0, 8), order_note])

func _process(_delta: float) -> void:
	if _key_edge(KEY_H):
		visible = not visible
	if Input.is_physical_key_pressed(KEY_CTRL) and _key_edge(KEY_Z):
		var target := undo_last_target()
		if target != "":
			do_revert.emit(target)

func _key_edge(key: Key) -> bool:
	var down := Input.is_physical_key_pressed(key)
	var was: bool = _key_down.get(key, false)
	_key_down[key] = down
	return down and not was
